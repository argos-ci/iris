//! What each anti-aliasing detector calls rendering noise, on shapes built
//! for the purpose: every scenario knows whether the two images show the same
//! thing rendered twice, or two different things.
//!
//! `Classic` is the detector of pixelmatch and odiff, kept for parity. The
//! scenarios where it gets the wrong answer are the reason `Subpixel` exists,
//! and are pinned here so the difference between the two stays deliberate.

mod support;

use adiff::{compare, compare_files, Antialiasing, Image, Options, Tier};
use support::{fixtures, image_from_fn};

type Color = [u8; 3];

const WHITE: Color = [255, 255, 255];
const BLACK: Color = [0, 0, 0];

fn grey(level: u8) -> Color {
  [level; 3]
}

/// `a` and `b` mixed, `share` of `b`.
fn mix(a: Color, b: Color, share: f64) -> Color {
  [0, 1, 2]
    .map(|channel| (a[channel] as f64 * (1.0 - share) + b[channel] as f64 * share).round() as u8)
}

/// An image whose every row is `profile`: vertical edges and strokes.
fn columns(profile: &[Color], height: u32) -> Image {
  image_from_fn(profile.len() as u32, height, |x, _| {
    let [r, g, b] = profile[x as usize];
    [r, g, b, 255]
  })
}

/// Differing pixels each detector reports, counting any visible difference.
struct Reported {
  off: u64,
  classic: u64,
  subpixel: u64,
}

fn reported(base: &Image, head: &Image) -> Reported {
  let run = |antialiasing| {
    let options = Options {
      tiers: vec![Tier {
        threshold: 0.05,
        min_score: 0.0,
        max_ignored_pixels: None,
      }],
      antialiasing,
      percent_decimals: None,
      fingerprint: false,
      shifts: false,
      ..Options::default()
    };
    compare(base, head, &options).unwrap().tiers[0].diff_pixels
  };
  Reported {
    off: run(Antialiasing::Off),
    classic: run(Antialiasing::Classic),
    subpixel: run(Antialiasing::Subpixel),
  }
}

const ROWS: u32 = 12;

/* ------------------------- noise both detectors see ------------------------- */

#[test]
fn an_edge_blended_differently_is_noise() {
  // The same edge, 0.3 px further in one render.
  let base = columns(&[WHITE, WHITE, WHITE, grey(153), BLACK, BLACK, BLACK], ROWS);
  let head = columns(&[WHITE, WHITE, WHITE, grey(77), BLACK, BLACK, BLACK], ROWS);
  let result = reported(&base, &head);
  assert_eq!(result.off, 12);
  assert_eq!((result.classic, result.subpixel), (0, 0));
}

#[test]
fn a_blend_spilling_into_the_next_pixel_is_noise() {
  // Half a pixel further: the blend now covers two pixels instead of one.
  let base = columns(&[WHITE, WHITE, WHITE, grey(102), BLACK, BLACK], ROWS);
  let head = columns(&[WHITE, WHITE, grey(204), grey(25), BLACK, BLACK], ROWS);
  let result = reported(&base, &head);
  assert_eq!(result.off, 24);
  assert_eq!((result.classic, result.subpixel), (0, 0));
}

/* --------------------- noise only the new detector sees --------------------- */

/// A background that is never flat: a different shade at every pixel, the
/// same in both images. Text over a photo, a gradient, a dithered fill.
fn textured(profile: impl Fn(u32) -> f64, width: u32, height: u32) -> Image {
  image_from_fn(width, height, |x, y| {
    let grain = |seed: u32| ((x * 7 + y * 13 + seed) % 11) as f64;
    let light = 215.0 + grain(0);
    let dark = 40.0 + grain(5);
    let level = (light + (dark - light) * profile(x)).round() as u8;
    [level, level, level, 255]
  })
}

#[test]
fn an_edge_over_a_textured_background_is_noise() {
  let edge = |blend: f64| {
    textured(
      move |x| match x {
        0..=4 => 0.0,
        5 => blend,
        _ => 1.0,
      },
      11,
      ROWS,
    )
  };
  let result = reported(&edge(0.35), &edge(0.7));
  assert_eq!(result.off, 12);
  // Nothing is exactly flat, so the classic detector has nothing to hold on to.
  assert_eq!(result.classic, 12);
  assert_eq!(result.subpixel, 0);
}

#[test]
fn a_hairline_snapped_to_another_pixel_is_noise() {
  // A one pixel stem straddling two columns in one render, sitting on one in
  // the other: what font hinting does to every vertical stroke of a text.
  let base = columns(&[WHITE, WHITE, grey(128), grey(128), WHITE, WHITE], ROWS);
  let head = columns(&[WHITE, WHITE, BLACK, WHITE, WHITE, WHITE], ROWS);
  let result = reported(&base, &head);
  assert_eq!(result.off, 24);
  // A stem has no darker neighbour: to the classic detector it is content.
  assert_eq!(result.classic, 24);
  assert_eq!(result.subpixel, 0);
}

#[test]
fn a_hairline_a_little_lighter_is_noise() {
  let base = columns(&[WHITE, WHITE, grey(60), WHITE, WHITE], ROWS);
  let head = columns(&[WHITE, WHITE, grey(95), WHITE, WHITE], ROWS);
  let result = reported(&base, &head);
  assert_eq!((result.off, result.classic, result.subpixel), (12, 12, 0));
}

#[test]
fn a_translucent_edge_is_noise() {
  // The blend is in the alpha channel instead of the colour.
  let edge = |alpha: u8| {
    image_from_fn(7, ROWS, |x, _| match x {
      0..=2 => [255, 255, 255, 255],
      3 => [0, 0, 0, alpha],
      _ => [0, 0, 0, 255],
    })
  };
  let result = reported(&edge(90), &edge(170));
  assert_eq!((result.off, result.subpixel), (12, 0));
}

/* -------------------- changes only the new detector sees -------------------- */

#[test]
fn an_antialiased_edge_moved_by_a_pixel_is_a_change() {
  let base = columns(&[WHITE, WHITE, grey(153), BLACK, BLACK, BLACK], ROWS);
  let head = columns(&[WHITE, WHITE, WHITE, grey(153), BLACK, BLACK], ROWS);
  let result = reported(&base, &head);
  assert_eq!(result.off, 24);
  // Each pixel on its own looks like a blend next to something flat.
  assert_eq!(result.classic, 0);
  assert_eq!(result.subpixel, 24);
}

#[test]
fn a_recoloured_shape_is_a_change_edges_included() {
  // A bar with soft edges goes from charcoal to blue. Its edge pixels are
  // blends in both images — of different colours.
  let bar = |ink: Color| {
    columns(
      &[
        WHITE,
        WHITE,
        mix(WHITE, ink, 0.5),
        ink,
        ink,
        ink,
        mix(WHITE, ink, 0.5),
        WHITE,
        WHITE,
      ],
      ROWS,
    )
  };
  let result = reported(&bar([60, 60, 60]), &bar([40, 70, 210]));
  assert_eq!(result.off, 5 * 12);
  // The classic detector drops the two edge columns.
  assert_eq!(result.classic, 3 * 12);
  assert_eq!(result.subpixel, 5 * 12);
}

#[test]
fn a_pixel_that_is_not_a_blend_of_its_sides_is_a_change() {
  // One pixel of an edge turns red: brighter than black, darker than white,
  // and nothing black and white add up to.
  let base = columns(&[WHITE, WHITE, WHITE, grey(128), BLACK, BLACK, BLACK], ROWS);
  let mut pixels = base.as_rgba8().to_vec();
  let index = (5 * 7 + 3) * 4;
  pixels[index..index + 4].copy_from_slice(&[255, 0, 0, 255]);
  let head = Image::from_rgba8(7, ROWS, &pixels).unwrap();

  let result = reported(&base, &head);
  assert_eq!(result.off, 1);
  assert_eq!(result.classic, 0);
  assert_eq!(result.subpixel, 1);
}

/* ------------------------ changes both detectors see ------------------------ */

#[test]
fn a_hard_edge_moved_by_a_pixel_is_a_change() {
  let base = columns(&[WHITE, WHITE, WHITE, BLACK, BLACK, BLACK], ROWS);
  let head = columns(&[WHITE, WHITE, WHITE, WHITE, BLACK, BLACK], ROWS);
  let result = reported(&base, &head);
  assert_eq!((result.off, result.classic, result.subpixel), (12, 12, 12));
}

#[test]
fn a_hairline_moved_by_a_pixel_is_a_change() {
  let base = columns(&[WHITE, WHITE, BLACK, WHITE, WHITE, WHITE], ROWS);
  let head = columns(&[WHITE, WHITE, WHITE, BLACK, WHITE, WHITE], ROWS);
  let result = reported(&base, &head);
  assert_eq!((result.off, result.classic, result.subpixel), (24, 24, 24));
}

#[test]
fn a_stroke_twice_as_bold_is_a_change() {
  let base = columns(&[WHITE, WHITE, BLACK, WHITE, WHITE, WHITE], ROWS);
  let head = columns(&[WHITE, WHITE, BLACK, BLACK, WHITE, WHITE], ROWS);
  let result = reported(&base, &head);
  assert_eq!((result.off, result.classic, result.subpixel), (12, 12, 12));
}

#[test]
fn a_new_hairline_is_a_change() {
  let base = columns(&[WHITE; 7], ROWS);
  let head = columns(&[WHITE, WHITE, WHITE, grey(150), WHITE, WHITE, WHITE], ROWS);
  let result = reported(&base, &head);
  assert_eq!((result.off, result.classic, result.subpixel), (12, 12, 12));
}

#[test]
fn a_recoloured_hairline_is_a_change() {
  let base = columns(&[WHITE, WHITE, [60, 60, 60], WHITE, WHITE], ROWS);
  let head = columns(&[WHITE, WHITE, [60, 60, 250], WHITE, WHITE], ROWS);
  let result = reported(&base, &head);
  assert_eq!((result.off, result.classic, result.subpixel), (12, 12, 12));
}

/* ---------------------------------- shapes ---------------------------------- */

/// A disc with anti-aliased edges, rendered by supersampling. `background`
/// and `ink` give the shade of each at a pixel.
fn disc(
  cx: f64,
  cy: f64,
  background: impl Fn(u32, u32) -> f64,
  ink: impl Fn(u32, u32) -> f64,
) -> Image {
  image_from_fn(48, 48, |x, y| {
    let mut inside = 0;
    for sy in 0..8 {
      for sx in 0..8 {
        let dx = x as f64 + (sx as f64 + 0.5) / 8.0 - cx;
        let dy = y as f64 + (sy as f64 + 0.5) / 8.0 - cy;
        inside += u32::from(dx * dx + dy * dy <= 15.0 * 15.0);
      }
    }
    let coverage = inside as f64 / 64.0;
    let level = (background(x, y) * (1.0 - coverage) + ink(x, y) * coverage).round() as u8;
    [level, level, level, 255]
  })
}

fn paper(_: u32, _: u32) -> f64 {
  250.0
}

fn charcoal(_: u32, _: u32) -> f64 {
  30.0
}

#[test]
fn a_shape_nudged_by_a_fraction_of_a_pixel_is_noise() {
  let result = reported(
    &disc(24.0, 24.0, paper, charcoal),
    &disc(24.3, 24.2, paper, charcoal),
  );
  assert!(result.off > 60, "{}", result.off);
  assert_eq!(result.subpixel, 0);
  assert!(result.classic <= 4, "{}", result.classic);
}

#[test]
fn a_shape_nudged_over_a_textured_background_is_noise() {
  // Neither the shape nor what is behind it is one flat colour: a photo
  // over a gradient, an icon over a map.
  let grain = |base: f64| move |x: u32, y: u32| base + ((x * 7 + y * 13) % 11) as f64;
  let result = reported(
    &disc(24.0, 24.0, grain(215.0), grain(40.0)),
    &disc(24.3, 24.2, grain(215.0), grain(40.0)),
  );
  assert!(result.off > 60, "{}", result.off);
  assert_eq!(result.subpixel, 0);
  // The classic detector needs one side of the edge to be exactly flat.
  assert!(
    result.classic * 2 > result.off,
    "{} of {}",
    result.classic,
    result.off
  );
}

#[test]
fn a_shape_moved_by_a_pixel_is_a_change() {
  let result = reported(
    &disc(24.0, 24.0, paper, charcoal),
    &disc(25.0, 24.0, paper, charcoal),
  );
  assert!(result.off > 80, "{}", result.off);
  // Moving a disc sideways moves its left and right flanks by a pixel, and
  // its top and bottom by next to nothing. The flanks are reported, and are
  // enough to report the change; the classic detector calls the whole
  // outline anti-aliasing.
  assert!(
    result.subpixel >= 40,
    "{} of {}",
    result.subpixel,
    result.off
  );
  assert!(result.classic < 10, "{} of {}", result.classic, result.off);
}

#[test]
fn a_shape_that_grew_is_a_change() {
  let ring = |radius: f64| {
    image_from_fn(48, 48, |x, y| {
      let distance = ((x as f64 - 23.5).powi(2) + (y as f64 - 23.5).powi(2)).sqrt();
      let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
      let level = (250.0 * (1.0 - coverage) + 30.0 * coverage).round() as u8;
      [level, level, level, 255]
    })
  };
  let result = reported(&ring(12.0), &ring(14.0));
  assert!(
    result.subpixel * 10 > result.off * 9,
    "{} of {}",
    result.subpixel,
    result.off
  );
}

/* ------------------------------- screenshots -------------------------------- */

fn on_fixture(name: &str, antialiasing: Antialiasing) -> (u64, u64) {
  let directory = fixtures().join("pairs").join(name);
  let options = Options {
    antialiasing,
    fingerprint: false,
    shifts: false,
    ..Options::default()
  };
  let result = compare_files(
    directory.join("base.png"),
    directory.join("compare.png"),
    &options,
  )
  .unwrap();
  (result.tiers[0].diff_pixels, result.tiers[1].diff_pixels)
}

#[test]
fn sees_through_compression_and_font_noise_on_real_screenshots() {
  // Pairs where nothing changed but the rendering. What is left at the
  // colour-sensitive tier is what could add up to a false alarm.
  for name in [
    "fontAliasing",
    "imageCompression2",
    "imageCompression3",
    "imageCompression4",
    "imageCompression5",
    "imageCompression6",
    "imageCompression7",
  ] {
    let (_, classic) = on_fixture(name, Antialiasing::Classic);
    let (_, subpixel) = on_fixture(name, Antialiasing::Subpixel);
    assert!(subpixel < classic, "{name}: {subpixel} vs {classic}");
    if name != "fontAliasing" {
      // Compression noise: the classic detector removes about one pixel in
      // a hundred, this one more than three in five.
      assert!(
        subpixel * 5 < classic * 2,
        "{name}: {subpixel} vs {classic}"
      );
    }
  }
}

#[test]
fn keeps_real_changes_on_real_screenshots() {
  // Pairs where something did change. The detector must not eat into it.
  for name in [
    "simple",
    "tableAlpha",
    "big-images",
    "minorChange1",
    "minorChange2",
  ] {
    let off = on_fixture(name, Antialiasing::Off);
    let classic = on_fixture(name, Antialiasing::Classic);
    let subpixel = on_fixture(name, Antialiasing::Subpixel);
    for (off, classic, subpixel) in [
      (off.0, classic.0, subpixel.0),
      (off.1, classic.1, subpixel.1),
    ] {
      // Never more than one percent removed, and never more than the
      // classic detector removes.
      assert!(subpixel * 100 >= off * 99, "{name}: {subpixel} of {off}");
      assert!(
        subpixel + 1000 >= classic,
        "{name}: {subpixel} vs {classic}"
      );
    }
  }
}
