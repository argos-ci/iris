//! How much memory a comparison takes, measured by counting every byte the
//! process allocates.
//!
//! A screenshot service compares images of tens of megapixels on machines
//! that run several comparisons at once, so the cost per pixel is what
//! decides how many fit. Each test states a budget in bytes per pixel and
//! fails when a change goes over it.

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard};

use iris::{compare, compare_buffers, compare_files, Antialiasing, Image, Mask, Options};
use support::{encode_rgba, fixtures, image_from_fn};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(size: usize) {
  let now = CURRENT.fetch_add(size, Relaxed) + size;
  PEAK.fetch_max(now, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let pointer = unsafe { System.alloc(layout) };
    if !pointer.is_null() {
      grew(layout.size());
    }
    pointer
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    let pointer = unsafe { System.alloc_zeroed(layout) };
    if !pointer.is_null() {
      grew(layout.size());
    }
    pointer
  }

  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    unsafe { System.dealloc(pointer, layout) };
    CURRENT.fetch_sub(layout.size(), Relaxed);
  }

  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    let moved = unsafe { System.realloc(pointer, layout, size) };
    if !moved.is_null() {
      if size > layout.size() {
        grew(size - layout.size());
      } else {
        CURRENT.fetch_sub(layout.size() - size, Relaxed);
      }
    }
    moved
  }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The counters are global: a test holds this for its whole run, so another
/// test building its images cannot show up in its measurement.
fn exclusive() -> MutexGuard<'static, ()> {
  static SERIAL: Mutex<()> = Mutex::new(());
  SERIAL
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The most `work` had allocated at once, beyond what was allocated before.
fn peak<T>(work: impl FnOnce() -> T) -> (T, usize) {
  let start = CURRENT.load(Relaxed);
  PEAK.store(start, Relaxed);
  let value = work();
  (value, PEAK.load(Relaxed).saturating_sub(start))
}

const MIB: usize = 1 << 20;
/// Allowance for fixed costs — decoder state, thread bookkeeping — on top of
/// the bytes per pixel.
const FIXED: usize = 256 << 10;

#[track_caller]
fn assert_within(what: &str, used: usize, pixels: usize, bytes_per_pixel: f64) {
  let budget = (pixels as f64 * bytes_per_pixel) as usize + FIXED;
  println!(
    "{what}: {:.1} MiB, {:.3} bytes per pixel (budget {bytes_per_pixel})",
    used as f64 / MIB as f64,
    used as f64 / pixels as f64
  );
  assert!(
    used <= budget,
    "{what}: {used} bytes, over the budget of {budget} ({:.3} bytes per pixel)",
    used as f64 / pixels as f64
  );
}

/// A page of fake text on white, `seed` picking the words.
fn page(width: u32, height: u32, seed: u32) -> Image {
  image_from_fn(width, height, |x, y| {
    let line = (y / 18 + seed) as u64;
    let mark = (line.wrapping_mul(0x9e3779b97f4a7c15)
      ^ ((x / 3) as u64).wrapping_mul(0xc2b2ae3d27d4eb4f)
      ^ ((y % 18) as u64).wrapping_mul(0x165667b19e3779f9))
    .wrapping_mul(0xff51afd7ed558ccd)
      >> 61;
    if y % 18 < 12 && mark < 3 {
      [24, 28, 40, 255]
    } else {
      [255, 255, 255, 255]
    }
  })
}

/// The same page with a banner pushed in at the top and a block recoloured:
/// a diff that touches most of the image, with shifts and edges to examine.
fn changed_page(width: u32, height: u32) -> Image {
  let original = page(width, height, 0);
  let source = original.pixels();
  image_from_fn(width, height, |x, y| {
    if (40..80).contains(&y) {
      return [220, 40, 40, 255];
    }
    if (300..500).contains(&y) && (100..400).contains(&x) {
      return [200, 220, 255, 255];
    }
    let from = if y >= 80 { y - 40 } else { y };
    source[(from * width + x) as usize].to_ne_bytes()
  })
}

fn options(antialiasing: Antialiasing) -> Options {
  Options {
    antialiasing,
    // Spread over threads as in production: their buffers count too.
    threads: 4,
    ..Options::default()
  }
}

#[test]
fn comparing_takes_a_fraction_of_a_byte_per_pixel() {
  let _serial = exclusive();
  let (width, height) = (1500, 2000);
  let (base, head) = (page(width, height, 0), changed_page(width, height));
  let pixels = (width * height) as usize;

  for antialiasing in [
    Antialiasing::Off,
    Antialiasing::Classic,
    Antialiasing::Subpixel,
  ] {
    let (result, used) = peak(|| compare(&base, &head, &options(antialiasing)).unwrap());
    assert!(result.score > 0.2 && result.shift.is_some());
    // A bit per pixel for each of the two tiers, one of them kept as the mask.
    assert_within(&format!("compare, {antialiasing:?}"), used, pixels, 0.3);

    // What the Node.js binding does next, once the images are gone: the
    // encoder keeps a filtered copy of the bits and the compressed output.
    let mask = result.mask.unwrap();
    let (_, used) = peak(|| mask.to_png().unwrap());
    assert_within("mask.to_png", used, pixels, 0.4);
  }
}

#[test]
fn images_of_different_sizes_are_not_copied_onto_the_canvas() {
  let _serial = exclusive();
  // One wider, the other taller: both would need padding.
  let base = page(1500, 2000, 0);
  let head = page(1200, 2600, 0);
  let canvas = 1500 * 2600;

  for antialiasing in [Antialiasing::Classic, Antialiasing::Subpixel] {
    let (result, used) = peak(|| compare(&base, &head, &options(antialiasing)).unwrap());
    assert_eq!((result.width, result.height), (1500, 2600));
    assert!(result.score > 0.0);
    assert_within(
      &format!("different sizes, {antialiasing:?}"),
      used,
      canvas,
      0.3,
    );
  }
}

/// Encode an image as PNG with the colour type given, from its grey level.
fn encode_grey(image: &Image, color: png::ColorType) -> Vec<u8> {
  let samples: Vec<u8> = image
    .pixels()
    .iter()
    .flat_map(|pixel| {
      let [r, _, _, a] = pixel.to_ne_bytes();
      match color {
        png::ColorType::Grayscale => vec![r],
        _ => vec![r, a],
      }
    })
    .collect();
  let mut output = Vec::new();
  let mut encoder = png::Encoder::new(&mut output, image.width(), image.height());
  encoder.set_color(color);
  encoder.set_depth(png::BitDepth::Eight);
  encoder.set_compression(png::Compression::Fast);
  let mut writer = encoder.write_header().unwrap();
  writer.write_image_data(&samples).unwrap();
  writer.finish().unwrap();
  output
}

#[test]
fn decoding_allocates_the_pixels_once() {
  let _serial = exclusive();
  let image = page(1500, 2000, 3);
  let pixels = 1500 * 2000;

  let cases = [
    ("PNG, RGBA", encode_rgba(&image)),
    ("PNG, grey", encode_grey(&image, png::ColorType::Grayscale)),
    (
      "PNG, grey and alpha",
      encode_grey(&image, png::ColorType::GrayscaleAlpha),
    ),
    (
      "JPEG",
      std::fs::read(fixtures().join("jpeg/screenshot.jpg")).unwrap(),
    ),
  ];
  for (what, bytes) in cases {
    let (decoded, used) = peak(|| Image::decode(&bytes).unwrap());
    let pixels = if what == "JPEG" {
      (decoded.width() * decoded.height()) as usize
    } else {
      pixels
    };
    // The four bytes of every pixel, and next to nothing else.
    assert_within(what, used, pixels, 4.1);
  }
}

#[test]
fn comparing_files_holds_the_two_images_and_little_else() {
  let _serial = exclusive();
  let directory = fixtures().join("pairs/big-images");
  let (base, head) = (directory.join("base.png"), directory.join("compare.png"));
  // 1000x3848 and 1000x4469.
  let images = 1000 * (3848 + 4469);
  let canvas = 1000 * 4469;

  let (result, used) =
    peak(|| compare_files(&base, &head, &options(Antialiasing::Classic)).unwrap());
  assert!(result.score > 0.5);
  let budget_per_pixel = (4.0 * images as f64 + 0.3 * canvas as f64) / images as f64;
  assert_within("compare_files", used, images, budget_per_pixel);
}

/// A PNG that claims to be `width` x `height`, with no pixels to back it: what
/// a decompression bomb looks like before it goes off.
fn png_claiming(width: u32, height: u32) -> Vec<u8> {
  fn crc(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
      crc ^= byte as u32;
      for _ in 0..8 {
        crc = if crc & 1 == 1 {
          (crc >> 1) ^ 0xedb88320
        } else {
          crc >> 1
        };
      }
    }
    !crc
  }
  fn chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = output.len();
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let checksum = crc(&output[start..]);
    output.extend_from_slice(&checksum.to_be_bytes());
  }

  let mut output = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
  let mut header = Vec::new();
  header.extend_from_slice(&width.to_be_bytes());
  header.extend_from_slice(&height.to_be_bytes());
  // 8-bit RGBA, deflate, adaptive filtering, not interlaced.
  header.extend_from_slice(&[8, 6, 0, 0, 0]);
  chunk(&mut output, b"IHDR", &header);
  // An empty zlib stream.
  chunk(
    &mut output,
    b"IDAT",
    &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
  );
  chunk(&mut output, b"IEND", &[]);
  output
}

#[test]
fn a_canvas_over_the_limit_is_refused_before_anything_is_decoded() {
  let _serial = exclusive();
  // 60000x60000 would be 14 GB of pixels.
  let bomb = png_claiming(60_000, 60_000);
  let small = encode_rgba(&page(100, 100, 0));
  let limited = Options {
    max_pixels: Some(100_000_000),
    ..Options::default()
  };

  for (base, head) in [(&bomb, &small), (&small, &bomb)] {
    let (result, used) = peak(|| compare_buffers(base, head, &limited));
    assert!(
      matches!(result, Err(iris::Error::TooLarge { .. })),
      "{result:?}"
    );
    assert!(used < MIB, "{used} bytes");
  }

  // The canvas is what counts: two images that fit alone can make one that
  // does not.
  let wide = encode_rgba(&page(20_000, 1, 0));
  let tall = encode_rgba(&page(1, 20_000, 0));
  let (result, used) = peak(|| compare_buffers(&wide, &tall, &limited));
  assert!(
    matches!(
      result,
      Err(iris::Error::TooLarge {
        width: 20_000,
        height: 20_000,
        ..
      })
    ),
    "{result:?}"
  );
  assert!(used < MIB, "{used} bytes");
}

#[test]
fn fingerprinting_a_stored_mask_holds_one_bit_per_pixel() {
  let _serial = exclusive();
  let (width, height) = (3000, 8000);
  let mask = Mask::from_fn(width, height, |x, y| (x / 7 + y / 11) % 3 == 0);
  let png = mask.to_png().unwrap();

  let (fingerprint, used) = peak(|| iris::fingerprint_png(&png).unwrap());
  assert_eq!(fingerprint, mask.fingerprint());
  assert_within("fingerprint_png", used, (width * height) as usize, 0.14);
}
