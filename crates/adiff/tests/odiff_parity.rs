//! Pixel-for-pixel parity with odiff, the tool this crate replaces.
//!
//! Each case runs the real `odiff` binary and compares its diff mask with
//! ours bit by bit, at several thresholds. A stored fingerprint is derived
//! from that mask, so anything short of identical would orphan it.
//!
//! The binary is not vendored: point `ODIFF_BIN` at one (the `odiff-bin` npm
//! package ships it). Without it the tests pass vacuously, unless
//! `REQUIRE_ODIFF` is set, which CI does.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use adiff::{compare, Antialiasing, Image, Mask, Options, Tier};
use support::{encode_rgba, fixtures, image_from_fn, Random};

fn odiff_binary() -> Option<PathBuf> {
  let binary = std::env::var_os("ODIFF_BIN").map(PathBuf::from);
  if binary.is_none() {
    assert!(
      std::env::var_os("REQUIRE_ODIFF").is_none(),
      "REQUIRE_ODIFF is set but ODIFF_BIN is not"
    );
    eprintln!("ODIFF_BIN is not set, skipping the odiff parity check");
  }
  binary
}

/// A scratch directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
  fn new(name: &str) -> Self {
    let path = std::env::temp_dir().join(format!("adiff-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    Self(path)
  }

  fn write(&self, name: &str, image: &Image) -> PathBuf {
    let path = self.0.join(name);
    std::fs::write(&path, encode_rgba(image)).unwrap();
    path
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.0);
  }
}

/// Run odiff the way the Argos backend does, and read back what it found.
fn odiff(
  binary: &Path,
  base: &Path,
  head: &Path,
  diff: &Path,
  threshold: f64,
  antialiasing: bool,
) -> (u64, Option<Mask>) {
  let _ = std::fs::remove_file(diff);
  let mut command = Command::new(binary);
  command
    .arg(base)
    .arg(head)
    .arg(diff)
    .arg("--parsable-stdout")
    .arg("--diff-mask")
    .arg(format!("--threshold={threshold}"));
  if antialiasing {
    command.arg("--antialiasing");
  }
  let output = command.output().expect("failed to run odiff");
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    matches!(output.status.code(), Some(0 | 22)),
    "odiff exited with {:?}: {stdout}{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr)
  );
  let count = stdout
    .trim()
    .split(';')
    .next()
    .and_then(|count| count.parse().ok())
    .unwrap_or(0);
  let mask = diff
    .exists()
    .then(|| Mask::from_png(&std::fs::read(diff).unwrap()).unwrap());
  (count, mask)
}

fn ours(base: &Image, head: &Image, threshold: f64, antialiasing: bool) -> (u64, Option<Mask>) {
  let options = Options {
    tiers: vec![Tier {
      threshold,
      min_score: 0.0,
      max_ignored_pixels: None,
    }],
    antialiasing: if antialiasing {
      Antialiasing::Classic
    } else {
      Antialiasing::Off
    },
    fingerprint: false,
    shifts: false,
    percent_decimals: None,
    ..Options::default()
  };
  let result = compare(base, head, &options).unwrap();
  (result.tiers[0].diff_pixels, result.mask)
}

/// The thresholds Argos runs at its default, strictest and loosest settings,
/// plus odiff's own default.
const THRESHOLDS: [f64; 6] = [0.15, 0.0225, 0.3, 0.045, 0.0, 0.1];

#[track_caller]
fn assert_same_as_odiff(
  binary: &Path,
  scratch: &Scratch,
  name: &str,
  (base, base_path): (&Image, &Path),
  (head, head_path): (&Image, &Path),
  thresholds: &[f64],
) {
  let diff_path = scratch.0.join("diff.png");
  for &threshold in thresholds {
    for antialiasing in [true, false] {
      let context = format!("{name}, threshold {threshold}, antialiasing {antialiasing}");
      let (expected_count, expected_mask) = odiff(
        binary,
        base_path,
        head_path,
        &diff_path,
        threshold,
        antialiasing,
      );
      let (count, mask) = ours(base, head, threshold, antialiasing);
      assert_eq!(count, expected_count, "pixel count: {context}");
      match (mask, expected_mask) {
        (None, None) => {}
        (Some(mask), Some(expected)) => {
          assert_eq!(
            (mask.width(), mask.height()),
            (expected.width(), expected.height()),
            "mask size: {context}"
          );
          assert!(mask == expected, "mask pixels: {context}");
        }
        (mask, expected) => panic!(
          "{context}: we {} a mask, odiff {}",
          if mask.is_some() {
            "wrote"
          } else {
            "did not write"
          },
          if expected.is_some() { "did" } else { "did not" },
        ),
      }
    }
  }
}

/// Grow an image to the canvas, the way the backend does before calling
/// odiff — which refuses images of different sizes.
fn pad(image: &Image, width: u32, height: u32) -> Image {
  let (source, source_width) = (image.pixels(), image.width());
  image_from_fn(width, height, |x, y| {
    if x < source_width && y < image.height() {
      source[(y * source_width + x) as usize].to_ne_bytes()
    } else {
      [0, 0, 0, 0]
    }
  })
}

#[test]
fn matches_odiff_on_the_screenshot_fixtures() {
  let Some(binary) = odiff_binary() else { return };
  let scratch = Scratch::new("fixtures");

  let mut names: Vec<String> = std::fs::read_dir(fixtures().join("pairs"))
    .unwrap()
    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
    .collect();
  names.sort();
  assert!(names.len() >= 19, "only {} fixtures found", names.len());

  for name in names {
    let directory = fixtures().join("pairs").join(&name);
    let (base_path, head_path) = (directory.join("base.png"), directory.join("compare.png"));
    let (base, head) = (
      Image::open(&base_path).unwrap(),
      Image::open(&head_path).unwrap(),
    );
    let width = base.width().max(head.width());
    let height = base.height().max(head.height());
    // A hundred megapixels through odiff a dozen times is minutes; the
    // thresholds Argos uses by default are enough for that one.
    let large = width as u64 * height as u64 > 50_000_000;
    let thresholds = if large {
      &THRESHOLDS[..2]
    } else {
      &THRESHOLDS[..]
    };

    if (base.width(), base.height()) == (head.width(), head.height()) {
      // Same size: odiff reads the original files, which also holds our PNG
      // decoder to its own.
      assert_same_as_odiff(
        &binary,
        &scratch,
        &name,
        (&base, &base_path),
        (&head, &head_path),
        thresholds,
      );
    } else {
      let padded_base = scratch.write("base.png", &pad(&base, width, height));
      let padded_head = scratch.write("head.png", &pad(&head, width, height));
      // We get the originals: the padding is ours to do.
      assert_same_as_odiff(
        &binary,
        &scratch,
        &name,
        (&base, &padded_base),
        (&head, &padded_head),
        thresholds,
      );
    }
  }
}

/// Flat cells with hard edges between them, then small perturbations: lots of
/// pixels near every threshold, and every neighbourhood shape the
/// anti-aliasing detector branches on.
fn noisy_pair(random: &mut Random, width: u32, height: u32, translucent: bool) -> (Image, Image) {
  let cell = random.between(2, 9) as u32;
  let columns = width.div_ceil(cell);
  let rows = height.div_ceil(cell);
  let alphas = [0u8, 1, 64, 128, 254, 255, 255, 255];
  let palette: Vec<[u8; 4]> = (0..columns * rows)
    .map(|_| {
      let [r, g, b, _] = (random.next() as u32).to_ne_bytes();
      let a = if translucent {
        alphas[random.below(8) as usize]
      } else {
        255
      };
      [r, g, b, a]
    })
    .collect();
  let base = image_from_fn(width, height, |x, y| {
    palette[((y / cell) * columns + x / cell) as usize]
  });

  let amplitude = [1i16, 3, 8, 20, 60][random.below(5) as usize];
  let density = [2u64, 5, 20][random.below(3) as usize];
  let source = base.pixels().to_vec();
  let head = image_from_fn(width, height, |x, y| {
    let mut pixel = source[(y * width + x) as usize].to_ne_bytes();
    if random.chance(1, density) {
      let channels = if translucent { 4 } else { 3 };
      for channel in pixel.iter_mut().take(channels) {
        let delta = random.between(0, 2 * amplitude as u64) as i16 - amplitude;
        *channel = (*channel as i16 + delta).clamp(0, 255) as u8;
      }
    }
    pixel
  });
  (base, head)
}

/// Discs rendered with coverage-based anti-aliasing, the second image a
/// fraction of a pixel away from the first: what font and shape rendering
/// differences look like.
fn shapes_pair(random: &mut Random, width: u32, height: u32) -> (Image, Image) {
  let discs: Vec<(f64, f64, f64, [u8; 3])> = (0..random.between(1, 6))
    .map(|_| {
      let [r, g, b, _] = (random.next() as u32).to_ne_bytes();
      (
        random.below(width as u64) as f64,
        random.below(height as u64) as f64,
        random.between(2, 20) as f64,
        [r, g, b],
      )
    })
    .collect();
  let background = [250u8, 250, 250];
  let (shift_x, shift_y) = (
    random.below(100) as f64 / 100.0,
    random.below(100) as f64 / 100.0,
  );

  let render = |dx: f64, dy: f64| {
    image_from_fn(width, height, |x, y| {
      let mut color = background.map(|channel| channel as f64);
      for &(cx, cy, radius, fill) in &discs {
        // 4x4 supersampling.
        let mut inside = 0;
        for sy in 0..4 {
          for sx in 0..4 {
            let px = x as f64 + (sx as f64 + 0.5) / 4.0 - cx - dx;
            let py = y as f64 + (sy as f64 + 0.5) / 4.0 - cy - dy;
            if px * px + py * py <= radius * radius {
              inside += 1;
            }
          }
        }
        let coverage = inside as f64 / 16.0;
        for channel in 0..3 {
          color[channel] = color[channel] * (1.0 - coverage) + fill[channel] as f64 * coverage;
        }
      }
      [
        color[0].round() as u8,
        color[1].round() as u8,
        color[2].round() as u8,
        255,
      ]
    })
  };
  (render(0.0, 0.0), render(shift_x, shift_y))
}

#[test]
fn matches_odiff_on_generated_images() {
  let Some(binary) = odiff_binary() else { return };
  let scratch = Scratch::new("generated");
  let mut random = Random::new(7);

  // Widths around odiff's vector sizes, and degenerate strips.
  let sizes = [
    (64, 48),
    (97, 53),
    (130, 101),
    (33, 200),
    (1, 50),
    (50, 1),
    (2, 2),
    (3, 3),
    (8, 8),
    (9, 7),
    (15, 16),
    (17, 31),
  ];
  for (index, &(width, height)) in sizes.iter().cycle().take(60).enumerate() {
    let (kind, (base, head)) = match index % 3 {
      0 => (
        "opaque noise",
        noisy_pair(&mut random, width, height, false),
      ),
      1 => (
        "translucent noise",
        noisy_pair(&mut random, width, height, true),
      ),
      _ => ("shapes", shapes_pair(&mut random, width, height)),
    };
    let base_path = scratch.write("base.png", &base);
    let head_path = scratch.write("head.png", &head);
    assert_same_as_odiff(
      &binary,
      &scratch,
      &format!("{kind} #{index} ({width}x{height})"),
      (&base, &base_path),
      (&head, &head_path),
      &THRESHOLDS,
    );
  }
}
