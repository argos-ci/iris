use crate::antialias::is_antialiased;
use crate::delta::{color_delta, max_delta};
use crate::error::{Error, Result};
use crate::image::Image;
use crate::mask::{Extent, Mask};
use crate::options::{Antialiasing, Options, MAX_TIERS};
use crate::shift::{self, ShiftReport};
use crate::subpixel::is_subpixel_shift;
use crate::view::View;

/// What one [`Tier`](crate::Tier) saw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TierResult {
  pub threshold: f64,
  /// Pixels over the tier's colour threshold, anti-aliasing excluded.
  pub diff_pixels: u64,
  /// `diff_pixels` as a share of the canvas.
  pub score: f64,
  /// Whether `score` reaches the tier's floor.
  pub reported: bool,
}

/// The outcome of comparing two images.
#[derive(Clone, Debug, PartialEq)]
pub struct Comparison {
  /// Size of the canvas both images were compared on: the larger width and
  /// the larger height. A smaller image is padded with transparent pixels.
  pub width: u32,
  pub height: u32,
  /// Share of the canvas that differs, 0 when no tier reports a difference.
  pub score: f64,
  /// Number of pixels set in `mask`.
  pub diff_pixels: u64,
  /// Index in [`Options::tiers`] of the tier the result comes from.
  pub tier: Option<usize>,
  /// One entry per tier, in the order of [`Options::tiers`].
  pub tiers: Vec<TierResult>,
  /// Differing pixels left out for being anti-aliasing.
  pub antialiased_pixels: u64,
  /// The pixels that differ, when a tier reports a difference.
  pub mask: Option<Mask>,
  /// Fingerprint of `mask`.
  pub fingerprint: Option<String>,
  /// Content that moved rather than changed, when there is any.
  pub shift: Option<ShiftReport>,
}

/// Everything one pass over the two images collects.
struct Scan {
  /// One bit per pixel and per tier, packed like the rows of a [`Mask`]:
  /// plane `r` holds the pixels over the threshold of the tier of rank `r`.
  /// A byte per pixel would do for any number of tiers, but two planes are a
  /// quarter of that, and the reported plane becomes the mask as it is.
  planes: Vec<Vec<u8>>,
  /// Pixels per class: a pixel of class `c` is over the tiers of rank `0..c`.
  histogram: [u64; MAX_TIERS + 1],
  /// Where the pixels of each class are.
  extents: [Extent; MAX_TIERS + 1],
  antialiased: u64,
  /// First and last row where the images differ at all, however slightly.
  raw_rows: Option<(usize, usize)>,
  /// Bitset of the columns where the images differ at all.
  raw_columns: Vec<u64>,
}

const CHUNK: usize = 8;

#[inline(always)]
fn chunk_is_equal(a: &[u32], b: &[u32]) -> bool {
  let a: &[u32; CHUNK] = a.try_into().expect("chunk length");
  let b: &[u32; CHUNK] = b.try_into().expect("chunk length");
  a == b
}

/// What one band of rows contributes to a [`Scan`].
struct Band {
  histogram: [u64; MAX_TIERS + 1],
  extents: [Extent; MAX_TIERS + 1],
  antialiased: u64,
  raw_rows: Option<(usize, usize)>,
  raw_columns: Vec<u64>,
}

/// Classify the pixels of the rows starting at `first_row`, as many as
/// `planes` hold, against all the tiers at once. `max_deltas` holds the
/// tiers' thresholds in ascending order, so the pixels over tier `n` are a
/// subset of those over tier `n - 1`: a pixel is set in the planes of every
/// tier up to the last one it exceeds. Anti-aliasing does not depend on the
/// threshold, so it is decided once.
fn scan_rows(
  base: View,
  head: View,
  first_row: usize,
  planes: &mut [&mut [u8]],
  max_deltas: &[i64],
  antialiasing: Antialiasing,
) -> Band {
  let width = base.width;
  let stride = width.div_ceil(8);
  let rows = planes[0].len() / stride;
  let mut histogram = [0u64; MAX_TIERS + 1];
  let mut extents = [Extent::default(); MAX_TIERS + 1];
  let mut antialiased = 0u64;
  let mut raw_rows = None;
  let mut raw_columns = vec![0u64; width.div_ceil(64)];
  let (mut base_scratch, mut head_scratch) = (Vec::new(), Vec::new());
  let lowest = max_deltas[0];
  // What "unchanged" means to the sub-pixel detector: under the most
  // sensitive tier, and never looser than a barely visible difference.
  let same = lowest.min(max_delta(0.025));

  for row in 0..rows {
    let y = first_row + row;
    let base_row = base.full_row(y, &mut base_scratch);
    let head_row = head.full_row(y, &mut head_scratch);
    if base_row == head_row {
      continue;
    }
    raw_rows = Some((raw_rows.map_or(y, |(first, _)| first), y));

    let mut x = 0;
    while x < width {
      let end = (x + CHUNK).min(width);
      if end - x == CHUNK && chunk_is_equal(&base_row[x..end], &head_row[x..end]) {
        x = end;
        continue;
      }
      for x in x..end {
        let (before, after) = (base_row[x], head_row[x]);
        if before == after {
          continue;
        }
        raw_columns[x / 64] |= 1 << (x % 64);

        let delta = color_delta(before, after);
        if delta <= lowest {
          continue;
        }
        let is_noise = match antialiasing {
          Antialiasing::Off => false,
          Antialiasing::Classic => is_antialiased(&base, &head, x, y),
          Antialiasing::Subpixel => is_subpixel_shift(&base, &head, x, y, same),
        };
        if is_noise {
          antialiased += 1;
          continue;
        }

        let mut class = 1;
        while class < max_deltas.len() && delta > max_deltas[class] {
          class += 1;
        }
        let (byte, bit) = (row * stride + x / 8, 0x80 >> (x % 8));
        for plane in planes[..class].iter_mut() {
          plane[byte] |= bit;
        }
        histogram[class] += 1;
        extents[class].include(x, y);
      }
      x = end;
    }
  }

  Band {
    histogram,
    extents,
    antialiased,
    raw_rows,
    raw_columns,
  }
}

/// Images under this many pixels are scanned on the calling thread: starting
/// threads would cost more than they save.
const PARALLEL_FROM: usize = 1 << 20;
/// Threads `Options::threads == 0` settles on, at most.
const AUTO_THREADS: usize = 4;

/// Rows handed to a thread at a time. Small enough that a thread on a slow
/// core holds the others back by a few rows, not by its share of the image.
const BAND_ROWS: usize = 32;

/// Scan the two images in bands of rows, shared out between threads as they
/// free up. A pixel's class depends on nothing another band writes, so the
/// result is the same whatever the number of threads.
fn scan(
  base: View,
  head: View,
  max_deltas: &[i64],
  antialiasing: Antialiasing,
  threads: usize,
) -> Scan {
  let (width, height) = (base.width, base.height);
  let stride = width.div_ceil(8);
  let mut planes: Vec<Vec<u8>> = max_deltas
    .iter()
    .map(|_| vec![0u8; stride * height])
    .collect();

  let threads = match threads {
    0 if width * height < PARALLEL_FROM => 1,
    0 => std::thread::available_parallelism().map_or(1, |count| count.get().min(AUTO_THREADS)),
    threads => threads,
  }
  .min(height.div_ceil(BAND_ROWS));

  // Band `n` gets rows `n * BAND_ROWS..` of every plane.
  let mut work: Vec<Vec<&mut [u8]>> = (0..height.div_ceil(BAND_ROWS))
    .map(|_| Vec::with_capacity(planes.len()))
    .collect();
  for plane in planes.iter_mut() {
    for (band, rows) in plane.chunks_mut(BAND_ROWS * stride).enumerate() {
      work[band].push(rows);
    }
  }
  let queue = std::sync::Mutex::new(work.into_iter().enumerate());
  let worker = || {
    let mut done = Vec::new();
    loop {
      // Take the lock for the time it takes to pick a band, not to scan it.
      let next = queue.lock().expect("a scanning thread panicked").next();
      let Some((band, mut planes)) = next else {
        return done;
      };
      done.push(scan_rows(
        base,
        head,
        band * BAND_ROWS,
        &mut planes,
        max_deltas,
        antialiasing,
      ));
    }
  };
  let bands: Vec<Band> = std::thread::scope(|scope| {
    let helpers: Vec<_> = (1..threads).map(|_| scope.spawn(worker)).collect();
    let mut bands = worker();
    for helper in helpers {
      bands.extend(
        helper
          .join()
          .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
      );
    }
    bands
  });

  let mut scan = Scan {
    planes,
    histogram: [0; MAX_TIERS + 1],
    extents: [Extent::default(); MAX_TIERS + 1],
    antialiased: 0,
    raw_rows: None,
    raw_columns: vec![0u64; width.div_ceil(64)],
  };
  for band in bands {
    for (total, count) in scan.histogram.iter_mut().zip(band.histogram) {
      *total += count;
    }
    for (total, extent) in scan.extents.iter_mut().zip(&band.extents) {
      total.merge(extent);
    }
    scan.antialiased += band.antialiased;
    for (total, columns) in scan.raw_columns.iter_mut().zip(band.raw_columns) {
      *total |= columns;
    }
    if let Some((first, last)) = band.raw_rows {
      scan.raw_rows = Some(match scan.raw_rows {
        None => (first, last),
        Some((start, end)) => (start.min(first), end.max(last)),
      });
    }
  }
  scan
}

/// Round a percentage the way the `odiff` command line prints it: take the
/// shortest decimal form of the value, then round that half up. The two steps
/// do not always agree with rounding the value itself — 0.015 is stored as
/// 0.01499…, and still prints as 0.02.
fn round_percentage(value: f64, decimals: u8) -> f64 {
  let decimals = decimals as usize;
  let text = value.to_string();
  let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
  if fraction.len() <= decimals {
    return value;
  }
  let Ok(kept) = format!("{whole}{}", &fraction[..decimals]).parse::<u128>() else {
    return value;
  };
  let scale = 10u128.pow(decimals as u32);
  let rounded = kept + u128::from(fraction.as_bytes()[decimals] >= b'5');
  format!("{}.{:0decimals$}", rounded / scale, rounded % scale)
    .parse()
    .unwrap_or(value)
}

fn score(count: u64, pixels: f64, percent_decimals: Option<u8>) -> f64 {
  match percent_decimals {
    None => count as f64 / pixels,
    Some(decimals) => round_percentage(100.0 * count as f64 / pixels, decimals) / 100.0,
  }
}

/// Refuse a canvas over [`Options::max_pixels`], from the sizes of the two
/// images: callers that have not decoded them yet check before they do.
pub(crate) fn check_canvas(options: &Options, base: (u32, u32), head: (u32, u32)) -> Result<()> {
  let (width, height) = (base.0.max(head.0), base.1.max(head.1));
  match options.max_pixels {
    Some(max_pixels) if width as u64 * height as u64 > max_pixels => Err(Error::TooLarge {
      width,
      height,
      max_pixels,
    }),
    _ => Ok(()),
  }
}

/// Compare two decoded images.
pub fn compare(base: &Image, head: &Image, options: &Options) -> Result<Comparison> {
  options.validate()?;
  check_canvas(
    options,
    (base.width(), base.height()),
    (head.width(), head.height()),
  )?;

  let width = base.width().max(head.width());
  let height = base.height().max(head.height());
  let base_view = View::new(base, width as usize, height as usize);
  let head_view = View::new(head, width as usize, height as usize);

  // Most sensitive tier first. `order[rank]` is the tier's index in the
  // options, and a pixel of class `c` is over the tiers of rank `0..c`.
  let mut order: Vec<usize> = (0..options.tiers.len()).collect();
  order.sort_by(|&a, &b| {
    options.tiers[a]
      .threshold
      .total_cmp(&options.tiers[b].threshold)
  });
  let max_deltas: Vec<i64> = order
    .iter()
    .map(|&index| max_delta(options.tiers[index].threshold))
    .collect();

  let scan = scan(
    base_view,
    head_view,
    &max_deltas,
    options.antialiasing,
    options.threads,
  );

  let pixels = width as f64 * height as f64;
  let mut tiers = vec![
    TierResult {
      threshold: 0.0,
      diff_pixels: 0,
      score: 0.0,
      reported: false,
    };
    options.tiers.len()
  ];
  // The tier to report: the highest score, and between equal scores the
  // least sensitive tier, whose mask is the smaller one.
  let mut selected: Option<(usize, usize)> = None;
  for (rank, &index) in order.iter().enumerate() {
    let tier = &options.tiers[index];
    let diff_pixels: u64 = scan.histogram[rank + 1..].iter().sum();
    let score = score(diff_pixels, pixels, options.percent_decimals);
    let reported = score > 0.0 && score >= tier.floor(pixels);
    tiers[index] = TierResult {
      threshold: tier.threshold,
      diff_pixels,
      score,
      reported,
    };
    if reported && selected.is_none_or(|(_, best)| score >= tiers[best].score) {
      selected = Some((rank, index));
    }
  }

  let mut comparison = Comparison {
    width,
    height,
    score: 0.0,
    diff_pixels: 0,
    tier: None,
    tiers,
    antialiased_pixels: scan.antialiased,
    mask: None,
    fingerprint: None,
    shift: None,
  };

  let Some((rank, index)) = selected else {
    return Ok(comparison);
  };

  // The pixels over a tier are those of every class above its rank.
  let mut extent = Extent::default();
  for class in &scan.extents[rank + 1..] {
    extent.merge(class);
  }
  let mut planes = scan.planes;
  let mask = Mask::from_bits(width, height, planes.swap_remove(rank), extent);
  drop(planes);
  comparison.score = comparison.tiers[index].score;
  comparison.diff_pixels = mask.count();
  comparison.tier = Some(index);
  if options.fingerprint {
    comparison.fingerprint = Some(mask.fingerprint());
  }
  if options.shifts {
    if let Some(raw_rows) = scan.raw_rows {
      comparison.shift = shift::detect(&shift::Input {
        base: base_view,
        head: head_view,
        raw_rows,
        raw_columns: &scan.raw_columns,
        mask: &mask,
        min_rows: options.min_shift_rows as usize,
      });
    }
  }
  comparison.mask = Some(mask);

  Ok(comparison)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::mask::Rect;
  use crate::options::Tier;

  const WHITE: [u8; 4] = [255, 255, 255, 255];
  const BLACK: [u8; 4] = [0, 0, 0, 255];

  fn solid(width: u32, height: u32, color: [u8; 4]) -> Vec<[u8; 4]> {
    vec![color; (width * height) as usize]
  }

  fn image(width: u32, pixels: &[[u8; 4]]) -> Image {
    let height = pixels.len() as u32 / width;
    Image::from_rgba8(width, height, pixels.as_flattened()).unwrap()
  }

  /// One tier that reports any pixel over `threshold`.
  fn any_change(threshold: f64) -> Options {
    Options {
      tiers: vec![Tier {
        threshold,
        min_score: 0.0,
        max_ignored_pixels: None,
      }],
      antialiasing: Antialiasing::Off,
      shifts: false,
      percent_decimals: None,
      ..Options::default()
    }
  }

  #[test]
  fn identical_images_have_no_score() {
    let pixels = solid(40, 30, [12, 34, 56, 255]);
    let result = compare(
      &image(40, &pixels),
      &image(40, &pixels),
      &Options::default(),
    )
    .unwrap();
    assert_eq!(
      (result.score, result.diff_pixels, result.tier),
      (0.0, 0, None)
    );
    assert!(result.mask.is_none() && result.fingerprint.is_none() && result.shift.is_none());
    assert!(result
      .tiers
      .iter()
      .all(|tier| tier.diff_pixels == 0 && !tier.reported));
  }

  #[test]
  fn reports_the_pixels_that_changed() {
    let base = solid(50, 40, WHITE);
    let mut head = base.clone();
    for y in 10..15 {
      for x in 20..30 {
        head[y * 50 + x] = BLACK;
      }
    }
    let result = compare(&image(50, &base), &image(50, &head), &any_change(0.1)).unwrap();
    assert_eq!(result.diff_pixels, 50);
    assert_eq!(result.score, 50.0 / 2000.0);
    let mask = result.mask.unwrap();
    assert_eq!(
      mask.bounds(),
      Some(Rect {
        x: 20,
        y: 10,
        width: 10,
        height: 5
      })
    );
    assert!(mask.is_set(20, 10) && mask.is_set(29, 14) && !mask.is_set(19, 10));
    assert!(result.fingerprint.unwrap().starts_with("v1:"));
  }

  #[test]
  fn finds_changes_at_every_position_of_a_row() {
    // The scan skips unchanged runs eight pixels at a time; a change must be
    // found wherever it falls relative to those runs, including the tail.
    for width in [1usize, 7, 8, 9, 16, 21] {
      for x in 0..width {
        let base = solid(width as u32, 3, WHITE);
        let mut head = base.clone();
        head[width + x] = BLACK;
        let result = compare(
          &image(width as u32, &base),
          &image(width as u32, &head),
          &any_change(0.1),
        )
        .unwrap();
        let mask = result.mask.expect("a difference");
        assert_eq!(result.diff_pixels, 1, "width {width}, x {x}");
        assert!(mask.is_set(x as u32, 1), "width {width}, x {x}");
      }
    }
  }

  #[test]
  fn evaluates_every_tier_in_one_scan() {
    // Left half: a faint tint, over the sensitive threshold only. One block:
    // a hard change, over both.
    let (width, height) = (100u32, 100u32);
    let base = solid(width, height, WHITE);
    let mut head = base.clone();
    for y in 0..100 {
      for x in 0..50 {
        head[y * 100 + x] = [247, 247, 250, 255];
      }
    }
    for y in 40..50 {
      for x in 70..80 {
        head[y * 100 + x] = BLACK;
      }
    }
    let options = Options {
      antialiasing: Antialiasing::Off,
      shifts: false,
      ..Options::default()
    };
    let result = compare(&image(width, &base), &image(width, &head), &options).unwrap();

    let [tolerant, sensitive] = result.tiers[..] else {
      panic!("two tiers")
    };
    assert_eq!(tolerant.diff_pixels, 100);
    assert_eq!(sensitive.diff_pixels, 5100);
    assert!(tolerant.reported && sensitive.reported);
    // The sensitive tier covers more of the image, so it is the one reported.
    assert_eq!(result.tier, Some(1));
    assert_eq!(result.diff_pixels, 5100);
    assert_eq!(result.score, 0.51);
  }

  #[test]
  fn a_faint_change_must_be_large_to_be_reported() {
    let base = solid(100, 100, WHITE);
    let mut head = base.clone();
    // 2% of the image, under the sensitive tier's 3% floor.
    for pixel in head.iter_mut().take(200) {
      *pixel = [247, 247, 250, 255];
    }
    let options = Options {
      antialiasing: Antialiasing::Off,
      ..Options::default()
    };
    let result = compare(&image(100, &base), &image(100, &head), &options).unwrap();
    assert_eq!(result.score, 0.0);
    assert!(result.mask.is_none());
    assert_eq!(result.tiers[1].diff_pixels, 200);
    assert!(!result.tiers[1].reported);
  }

  #[test]
  fn the_tolerant_tier_ignores_a_handful_of_pixels() {
    let options = Options {
      antialiasing: Antialiasing::Off,
      shifts: false,
      percent_decimals: None,
      ..Options::default()
    };
    // On a megapixel the floor is the 20 pixel cap.
    for (changed, reported) in [(19, false), (20, true), (21, true)] {
      let base = solid(1000, 1000, WHITE);
      let mut head = base.clone();
      for pixel in head.iter_mut().skip(5000).step_by(7).take(changed) {
        *pixel = BLACK;
      }
      let result = compare(&image(1000, &base), &image(1000, &head), &options).unwrap();
      assert_eq!(result.score > 0.0, reported, "{changed} pixels");
      assert_eq!(result.tiers[0].diff_pixels, changed as u64);
    }
  }

  #[test]
  fn percent_decimals_reproduces_the_command_line_rounding() {
    let options = Options {
      antialiasing: Antialiasing::Off,
      shifts: false,
      percent_decimals: Some(2),
      ..Options::default()
    };
    // 0.0049% prints as 0.00, 0.005% as 0.01.
    for (changed, percentage) in [(49, 0.0), (50, 0.01), (51, 0.01), (149, 0.01), (150, 0.02)] {
      let base = solid(1000, 1000, WHITE);
      let mut head = base.clone();
      for pixel in head.iter_mut().skip(5000).step_by(7).take(changed) {
        *pixel = BLACK;
      }
      let result = compare(&image(1000, &base), &image(1000, &head), &options).unwrap();
      assert_eq!(result.score, percentage / 100.0, "{changed} pixels");
    }
  }

  #[test]
  fn rounds_percentages_like_odiff() {
    // Observed from odiff 4.5.0 on images built for the purpose.
    for (value, decimals, expected) in [
      (0.125, 2, 0.13),
      (0.375, 2, 0.38),
      (0.625, 2, 0.63),
      (0.01, 2, 0.01),
      (0.005, 2, 0.01),
      (0.015, 2, 0.02),
      (0.0049, 2, 0.0),
      (0.0051, 2, 0.01),
      (0.00625, 2, 0.01),
      (0.01875, 2, 0.02),
      (99.999, 2, 100.0),
      (12.0, 2, 12.0),
      (2.5, 0, 3.0),
    ] {
      assert_eq!(round_percentage(value, decimals), expected, "{value}");
    }
  }

  #[test]
  fn pads_the_smaller_image_with_transparent_pixels() {
    let base = solid(10, 10, BLACK);
    let head = solid(10, 14, BLACK);
    let result = compare(&image(10, &base), &image(10, &head), &any_change(0.1)).unwrap();
    assert_eq!((result.width, result.height), (10, 14));
    // The four extra rows are black against nothing.
    assert_eq!(result.diff_pixels, 40);
    assert_eq!(
      result.mask.unwrap().bounds(),
      Some(Rect {
        x: 0,
        y: 10,
        width: 10,
        height: 4
      })
    );
  }

  #[test]
  fn transparent_padding_reads_as_white() {
    // Missing pixels blend over white like any transparent pixel, so extra
    // white rows are not a difference.
    let base = solid(10, 10, WHITE);
    let head = solid(10, 14, WHITE);
    let result = compare(&image(10, &base), &image(10, &head), &any_change(0.0)).unwrap();
    assert_eq!((result.height, result.score), (14, 0.0));
  }

  #[test]
  fn a_wider_and_a_taller_image_share_a_canvas() {
    let base = solid(12, 5, BLACK);
    let head = solid(8, 9, BLACK);
    let result = compare(&image(12, &base), &image(8, &head), &any_change(0.1)).unwrap();
    assert_eq!((result.width, result.height), (12, 9));
    // 4x5 only in base, 8x4 only in head, the 4x4 corner in neither.
    assert_eq!(result.diff_pixels, 20 + 32);
  }

  #[test]
  fn leaves_antialiasing_out() {
    // A hard vertical edge, re-rasterised with a blended column.
    let grey = [128, 128, 128, 255];
    let row = |blend: [u8; 4]| -> Vec<[u8; 4]> {
      (0..20)
        .map(|x| match x {
          0..=9 => BLACK,
          10 => blend,
          _ => WHITE,
        })
        .collect()
    };
    let base: Vec<_> = (0..20).flat_map(|_| row(WHITE)).collect();
    let head: Vec<_> = (0..20).flat_map(|_| row(grey)).collect();

    let mut options = any_change(0.1);
    let strict = compare(&image(20, &base), &image(20, &head), &options).unwrap();
    assert_eq!(strict.diff_pixels, 20);

    options.antialiasing = Antialiasing::Classic;
    let lenient = compare(&image(20, &base), &image(20, &head), &options).unwrap();
    assert_eq!((lenient.score, lenient.antialiased_pixels), (0.0, 20));
  }

  #[test]
  fn tiers_keep_their_position_whatever_their_order() {
    let base = solid(100, 100, WHITE);
    let mut head = base.clone();
    for pixel in head.iter_mut().take(5000) {
      *pixel = [247, 247, 250, 255];
    }
    let mut options = Options {
      antialiasing: Antialiasing::Off,
      shifts: false,
      ..Options::default()
    };
    let forward = compare(&image(100, &base), &image(100, &head), &options).unwrap();
    options.tiers.reverse();
    let backward = compare(&image(100, &base), &image(100, &head), &options).unwrap();
    assert_eq!(forward.tier, Some(1));
    assert_eq!(backward.tier, Some(0));
    assert_eq!(forward.tiers[1], backward.tiers[0]);
    assert_eq!(forward.mask, backward.mask);
  }

  #[test]
  fn the_number_of_threads_does_not_change_the_result() {
    // Noise, edges and blocks across enough rows for every band to hold some.
    let (width, height) = (61u32, 97u32);
    let mut state = 0x2545f4914f6cdd1du64;
    let mut next = || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state
    };
    let base: Vec<[u8; 4]> = (0..width * height)
      .map(|index| {
        let cell = (index / width / 6) * 11 + (index % width) / 6;
        let shade = (cell * 37 % 200) as u8;
        [shade, shade / 2 + 60, 255 - shade, 255]
      })
      .collect();
    let head: Vec<[u8; 4]> = base
      .iter()
      .map(|&[r, g, b, a]| match next() % 5 {
        0 => [r.wrapping_add((next() % 90) as u8), g, b, a],
        1 => [r, g.saturating_sub(4), b, a],
        _ => [r, g, b, a],
      })
      .collect();

    for antialiasing in [
      Antialiasing::Off,
      Antialiasing::Classic,
      Antialiasing::Subpixel,
    ] {
      let run = |threads| {
        let options = Options {
          antialiasing,
          threads,
          ..Options::default()
        };
        compare(&image(width, &base), &image(width, &head), &options).unwrap()
      };
      let single = run(1);
      assert!(single.diff_pixels > 0);
      for threads in [2, 3, 7, 97, 500] {
        assert_eq!(run(threads), single, "{antialiasing:?}, {threads} threads");
      }
    }
  }

  #[test]
  fn reports_the_mask_of_the_tier_it_selects() {
    // Three tiers over a gradient of changes: whichever is reported, its mask
    // is what a comparison with that tier alone finds.
    let (width, height) = (60u32, 40u32);
    let base = solid(width, height, WHITE);
    let head: Vec<[u8; 4]> = (0..width * height)
      .map(|index| {
        let level = 255 - ((index % width) * 4) as u8;
        [level, level, 255, 255]
      })
      .collect();
    let thresholds = [0.3, 0.05, 0.15];
    for selected in 0..thresholds.len() {
      let tiers = thresholds
        .iter()
        .enumerate()
        .map(|(index, &threshold)| Tier {
          threshold,
          // Only the selected tier can report.
          min_score: if index == selected { 0.0 } else { 1.0 },
          max_ignored_pixels: None,
        })
        .collect();
      let options = Options {
        tiers,
        ..any_change(0.0)
      };
      let result = compare(&image(width, &base), &image(width, &head), &options).unwrap();
      let alone = compare(
        &image(width, &base),
        &image(width, &head),
        &any_change(thresholds[selected]),
      )
      .unwrap();
      assert_eq!(result.tier, Some(selected));
      assert_eq!(result.mask, alone.mask, "tier {selected}");
      assert_eq!(result.diff_pixels, alone.diff_pixels);
    }
  }

  #[test]
  fn refuses_a_canvas_over_the_limit() {
    let pixels = solid(10, 10, WHITE);
    let options = Options {
      max_pixels: Some(99),
      ..Options::default()
    };
    assert!(matches!(
      compare(&image(10, &pixels), &image(10, &pixels), &options),
      Err(Error::TooLarge { .. })
    ));
  }
}
