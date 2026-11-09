//! Perceptual colour distance in YIQ space, from "Measuring perceived color
//! difference using YIQ NTSC transmission color space in mobile applications"
//! (Kotsarenko & Ramos) — the metric pixelmatch and odiff use.
//!
//! The arithmetic is the 12-bit fixed point of odiff 4.5.0, reproduced
//! operation for operation: a pixel sitting on the threshold has to land on the
//! same side here as it did there, or the mask — and the fingerprint derived
//! from it — would drift from the ones already stored.

/// Largest value the metric can take, between black and white.
const MAX_YIQ_DELTA: f64 = 35215.0;

const SHIFT: u32 = 12;
const ONE: i64 = 1 << SHIFT;
const WHITE: i64 = 255 << SHIFT;

// `coefficient * 4096`, truncated rather than rounded: odiff 4.5.0 does, so
// the luma weights add up to 4094 and every delta comes out about 0.1% short
// of the textbook value. Later odiff versions round, and count a few more
// pixels near the threshold. `fixed_point_constants` checks these.
const Y_R: i64 = 1224;
const Y_G: i64 = 2402;
const Y_B: i64 = 468;
const I_R: i64 = 2441;
const I_G: i64 = -1123;
const I_B: i64 = -1318;
const Q_R: i64 = 866;
const Q_G: i64 = -2140;
const Q_B: i64 = 1274;
const Y_WEIGHT: i64 = 2069;
const I_WEIGHT: i64 = 1224;
const Q_WEIGHT: i64 = 801;

const Y_R_F: f64 = 0.29889531;
const Y_G_F: f64 = 0.58662247;
const Y_B_F: f64 = 0.11448223;

/// Largest [`color_delta`] a pixel may have and still count as unchanged.
/// `threshold` is on pixelmatch's scale: 0 is strict, 1 accepts everything.
pub(crate) fn max_delta(threshold: f64) -> i64 {
  let max = MAX_YIQ_DELTA * (threshold * threshold);
  (max * ONE as f64) as i64
}

#[inline(always)]
fn blend_over_white(channel: i64, alpha: i64) -> i64 {
  if alpha == 0 {
    WHITE
  } else {
    WHITE + ((channel - 255) * alpha * ONE) / 255
  }
}

#[inline(always)]
fn yiq(pixel: u32) -> (i64, i64, i64) {
  let [r, g, b, a] = pixel.to_ne_bytes();
  let a = a as i64;
  let r = blend_over_white(r as i64, a);
  let g = blend_over_white(g as i64, a);
  let b = blend_over_white(b as i64, a);
  (
    (r * Y_R + g * Y_G + b * Y_B) >> SHIFT,
    (r * I_R + g * I_G + b * I_B) >> SHIFT,
    (r * Q_R + g * Q_G + b * Q_B) >> SHIFT,
  )
}

/// Distance between two pixels, scaled by 4096. Semi-transparent pixels are
/// blended over white first.
#[inline]
pub(crate) fn color_delta(a: u32, b: u32) -> i64 {
  let [ar, ag, ab, aa] = a.to_ne_bytes();
  let [br, bg, bb, ba] = b.to_ne_bytes();

  let (y, i, q) = if aa == 255 && ba == 255 {
    // Blending an opaque channel is exactly `channel << 12`, which the shift
    // in `yiq` undoes, so the transform is linear in the channel differences.
    let r = ar as i64 - br as i64;
    let g = ag as i64 - bg as i64;
    let b = ab as i64 - bb as i64;
    (
      r * Y_R + g * Y_G + b * Y_B,
      r * I_R + g * I_G + b * I_B,
      r * Q_R + g * Q_G + b * Q_B,
    )
  } else {
    let (ay, ai, aq) = yiq(a);
    let (by, bi, bq) = yiq(b);
    (ay - by, ai - bi, aq - bq)
  };

  (y * y * Y_WEIGHT + i * i * I_WEIGHT + q * q * Q_WEIGHT) >> (2 * SHIFT)
}

/// `channel * coefficient` for every channel value: the three products of an
/// opaque pixel's luma, looked up instead of converted and multiplied.
const fn luma_table(coefficient: f64) -> [f64; 256] {
  let mut table = [0.0; 256];
  let mut channel = 0;
  while channel < 256 {
    table[channel] = channel as f64 * coefficient;
    channel += 1;
  }
  table
}

static LUMA_R: [f64; 256] = luma_table(Y_R_F);
static LUMA_G: [f64; 256] = luma_table(Y_G_F);
static LUMA_B: [f64; 256] = luma_table(Y_B_F);

/// Luma of a pixel blended over white, in floating point as odiff computes
/// it for anti-aliasing: the sums are kept in the same order, so two pixels
/// compare the same way here as there.
#[inline(always)]
pub(crate) fn brightness(pixel: u32) -> f64 {
  let [r, g, b, a] = pixel.to_ne_bytes();
  match a {
    255 => LUMA_R[r as usize] + LUMA_G[g as usize] + LUMA_B[b as usize],
    0 => 255.0 * Y_R_F + 255.0 * Y_G_F + 255.0 * Y_B_F,
    _ => {
      let alpha = a as f64 / 255.0;
      let blend = |channel: u8| 255.0 + (channel as f64 - 255.0) * alpha;
      blend(r) * Y_R_F + blend(g) * Y_G_F + blend(b) * Y_B_F
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    u32::from_ne_bytes([r, g, b, a])
  }

  /// The metric in floating point, straight from the paper.
  fn reference_delta(a: u32, b: u32) -> f64 {
    fn blended(pixel: u32) -> [f64; 3] {
      let [r, g, b, a] = pixel.to_ne_bytes();
      let alpha = a as f64 / 255.0;
      [r, g, b].map(|channel| 255.0 + (channel as f64 - 255.0) * alpha)
    }
    let (a, b) = (blended(a), blended(b));
    let component = |coefficients: [f64; 3]| {
      (0..3)
        .map(|index| coefficients[index] * (a[index] - b[index]))
        .sum::<f64>()
    };
    let y = component([0.29889531, 0.58662247, 0.11448223]);
    let i = component([0.59597799, -0.27417610, -0.32180189]);
    let q = component([0.21147017, -0.52261711, 0.31114694]);
    0.5053 * y * y + 0.299 * i * i + 0.1957 * q * q
  }

  #[test]
  fn fixed_point_constants() {
    let fixed = |value: f64| (value * ONE as f64) as i64;
    assert_eq!(
      [Y_R, Y_G, Y_B],
      [fixed(0.29889531), fixed(0.58662247), fixed(0.11448223)]
    );
    assert_eq!(
      [I_R, I_G, I_B],
      [fixed(0.59597799), fixed(-0.27417610), fixed(-0.32180189)]
    );
    assert_eq!(
      [Q_R, Q_G, Q_B],
      [fixed(0.21147017), fixed(-0.52261711), fixed(0.31114694)]
    );
    assert_eq!(
      [Y_WEIGHT, I_WEIGHT, Q_WEIGHT],
      [fixed(0.5053), fixed(0.299), fixed(0.1957)]
    );
  }

  #[test]
  fn identical_pixels_have_no_delta() {
    for pixel in [
      rgba(0, 0, 0, 255),
      rgba(12, 200, 99, 255),
      rgba(40, 50, 60, 70),
    ] {
      assert_eq!(color_delta(pixel, pixel), 0);
    }
  }

  #[test]
  fn black_to_white_stays_below_the_maximum() {
    let delta = color_delta(rgba(0, 0, 0, 255), rgba(255, 255, 255, 255));
    let expected = 0.5053 * 255.0 * 255.0 * ONE as f64;
    assert!((delta as f64 - expected).abs() / expected < 2e-3, "{delta}");
    // A threshold of 1 therefore accepts every pixel.
    assert!(delta <= max_delta(1.0));
  }

  #[test]
  fn transparent_pixels_blend_to_white() {
    let white = rgba(255, 255, 255, 255);
    assert_eq!(color_delta(rgba(0, 0, 0, 0), white), 0);
    assert_eq!(color_delta(rgba(9, 9, 9, 0), rgba(200, 1, 1, 0)), 0);
    assert_eq!(brightness(rgba(0, 0, 0, 0)), brightness(white));
  }

  #[test]
  fn brightness_matches_the_formula_for_every_alpha() {
    let mut state = 0x13198a2e03707344u64;
    for _ in 0..20_000 {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      let [r, g, b, a] = (state as u32).to_ne_bytes();
      // Blended over white, then weighted, summed left to right.
      let alpha = a as f64 / 255.0;
      let blend = |channel: u8| match a {
        255 => channel as f64,
        0 => 255.0,
        _ => 255.0 + (channel as f64 - 255.0) * alpha,
      };
      let expected = blend(r) * Y_R_F + blend(g) * Y_G_F + blend(b) * Y_B_F;
      assert_eq!(brightness(rgba(r, g, b, a)), expected);
      assert_eq!(
        brightness(rgba(r, g, b, 255)),
        r as f64 * Y_R_F + g as f64 * Y_G_F + b as f64 * Y_B_F
      );
    }
  }

  #[test]
  fn is_symmetric() {
    let (a, b) = (rgba(10, 20, 30, 255), rgba(200, 100, 50, 128));
    assert_eq!(color_delta(a, b), color_delta(b, a));
  }

  #[test]
  fn opaque_fast_path_matches_the_general_one() {
    let mut state = 0x9e3779b97f4a7c15u64;
    let mut next = || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state
    };
    for _ in 0..20_000 {
      let a = (next() as u32).to_ne_bytes();
      let b = (next() as u32).to_ne_bytes();
      let (a, b) = (rgba(a[0], a[1], a[2], 255), rgba(b[0], b[1], b[2], 255));
      let (ay, ai, aq) = yiq(a);
      let (by, bi, bq) = yiq(b);
      let (y, i, q) = (ay - by, ai - bi, aq - bq);
      let general = (y * y * Y_WEIGHT + i * i * I_WEIGHT + q * q * Q_WEIGHT) >> (2 * SHIFT);
      assert_eq!(color_delta(a, b), general);
    }
  }

  #[test]
  fn tracks_the_floating_point_metric() {
    let mut state = 0x243f6a8885a308d3u64;
    let mut next = || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state as u32
    };
    for _ in 0..20_000 {
      let (a, b) = (next(), next());
      let fixed = color_delta(a, b) as f64 / ONE as f64;
      let float = reference_delta(a, b);
      assert!(
        (fixed - float).abs() <= 0.004 * float + 1.0,
        "{a:08x} {b:08x}: {fixed} vs {float}"
      );
    }
  }

  #[test]
  fn agrees_with_odiff_on_a_pixel_near_the_threshold() {
    // Taken from a screenshot: over the 0.15 threshold in floating point,
    // and under it with odiff 4.5.0's truncated coefficients.
    let delta = color_delta(rgba(39, 52, 71, 255), rgba(83, 90, 106, 255));
    assert_eq!(delta, 3_242_323);
    assert!(delta <= max_delta(0.15));
  }

  #[test]
  fn threshold_scales_quadratically() {
    assert_eq!(max_delta(0.0), 0);
    assert_eq!(
      max_delta(0.1),
      (35215.0f64 * (0.1f64 * 0.1) * 4096.0) as i64
    );
    assert_eq!(max_delta(1.0), 35215 * 4096);
  }
}
