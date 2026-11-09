//! Sub-pixel anti-aliasing detection.
//!
//! The classic detector asks a local question — "does this pixel look like a
//! blend, next to something flat?" — and answers it per image. That misses
//! rendering noise wherever nothing is exactly flat (text over a gradient or a
//! photo), misses thin strokes altogether (a stem one pixel wide has no darker
//! neighbour), and forgives real changes that happen to land on an edge: when
//! a label changes colour, every blended pixel of every glyph is still "a
//! blend next to something flat".
//!
//! This detector asks the question the other way round: **could the two
//! images be two rasterisations of the same shape, less than a pixel apart?**
//! Along each of the four axes through the pixel, it looks for two pixels, one
//! or two steps away on either side, that did not change between the images.
//! Those are the fixed points; whatever lies between them must then be
//! explained by one of two shapes:
//!
//! - an **edge**: the two sides have different colours, and every pixel in
//!   between is a mix of the two in both images. Summing the mix ratios gives
//!   the position of the edge to a fraction of a pixel, in each image.
//! - a **stroke**: the two sides are the same background, and what lies in
//!   between is ink of one colour over it. The ink has an amount and a centre
//!   of mass, in each image.
//!
//! The difference is rendering noise when the edge, or the stroke's centre,
//! moved by no more than [`MAX_SHIFT`] pixels and the stroke kept its amount
//! of ink. A whole-pixel move, a stroke that got bolder, ink of another
//! colour, a pixel that is not a mix of its two sides, a side that changed:
//! none of them fit, and all of them are reported.

use crate::antialias::View;
use crate::delta::color_delta;

/// Furthest an edge or a stroke may move, in pixels, and still be the same
/// shape rendered differently. A layout change moves things by whole pixels.
const MAX_SHIFT: f32 = 0.6;
/// Share of its ink a stroke may gain or lose. Rasterisers disagree on the
/// weight of a hairline by a little; a bolder font disagrees by a lot.
const MASS_TOLERANCE: f32 = 0.25;
/// How far a pixel may stray from the line between its two sides, as a share
/// of the distance between them. Zero would demand a perfect mix; rounding,
/// gamma-aware blending and LCD fringes all bend it slightly.
const OFF_AXIS: f32 = 0.3;
/// How far the colour of a stroke's ink may turn between the two images, as
/// the sine of the angle. A stroke that changed colour is not the same stroke.
const INK_TURN: f32 = 0.2;
/// The same allowances in absolute terms, in 8-bit levels, for low contrast.
const SLACK: f32 = 4.0;
/// How far past either side a mix ratio may land before it is not a mix.
const OVERSHOOT: f32 = 0.1;

const AXES: [(isize, isize); 4] = [(1, 0), (0, 1), (1, 1), (1, -1)];
/// How far to look for a fixed point on each side: next to the pixel first,
/// then one further, which covers an edge whose blend spills over two pixels.
const REACHES: [(isize, isize); 4] = [(1, 1), (1, 2), (2, 1), (2, 2)];

type Rgb = [f32; 3];

/// Colour of a pixel over a white background, as the comparison sees it.
#[inline(always)]
fn rgb(pixel: u32) -> Rgb {
  let [r, g, b, a] = pixel.to_ne_bytes();
  if a == 255 {
    return [r as f32, g as f32, b as f32];
  }
  let alpha = a as f32 / 255.0;
  [r, g, b].map(|channel| 255.0 + (channel as f32 - 255.0) * alpha)
}

#[inline(always)]
fn sub(a: Rgb, b: Rgb) -> Rgb {
  [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

#[inline(always)]
fn dot(a: Rgb, b: Rgb) -> f32 {
  a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Component of `vector` along `direction` (a unit vector), and how far off
/// it the rest lies.
#[inline(always)]
fn decompose(vector: Rgb, direction: Rgb) -> (f32, f32) {
  let along = dot(vector, direction);
  let off = (dot(vector, vector) - along * along).max(0.0).sqrt();
  (along, off)
}

/// The pixels strictly between the two fixed points, in both images.
struct Span {
  base: [Rgb; 3],
  head: [Rgb; 3],
  length: usize,
}

/// An edge between `from` and `to`: each pixel of the span is a mix of the
/// two, and the sum of the mix ratios is where the edge sits.
fn is_same_edge(span: &Span, from: Rgb, to: Rgb) -> bool {
  let across = sub(to, from);
  let contrast = dot(across, across).sqrt();
  let direction = across.map(|channel| channel / contrast);
  let stray = OFF_AXIS * contrast + SLACK;

  let mut shift = 0.0;
  for index in 0..span.length {
    let (base_along, base_off) = decompose(sub(span.base[index], from), direction);
    let (head_along, head_off) = decompose(sub(span.head[index], from), direction);
    if base_off > stray || head_off > stray {
      return false;
    }
    let (base_ratio, head_ratio) = (base_along / contrast, head_along / contrast);
    let mixes = -OVERSHOOT..=1.0 + OVERSHOOT;
    if !mixes.contains(&base_ratio) || !mixes.contains(&head_ratio) {
      return false;
    }
    let moved = base_ratio.clamp(0.0, 1.0) - head_ratio.clamp(0.0, 1.0);
    // No single pixel changes by more than the edge moved.
    if moved.abs() > MAX_SHIFT {
      return false;
    }
    shift += moved;
  }
  shift.abs() <= MAX_SHIFT
}

/// A stroke over `background`: the span holds ink of one colour, in the same
/// amount and around the same centre in both images.
fn is_same_stroke(span: &Span, background: Rgb) -> bool {
  let add = |a: Rgb, b: Rgb| [a[0] + b[0], a[1] + b[1], a[2] + b[2]];
  let (mut base_ink, mut head_ink) = ([0.0f32; 3], [0.0f32; 3]);
  for index in 0..span.length {
    base_ink = add(base_ink, sub(span.base[index], background));
    head_ink = add(head_ink, sub(span.head[index], background));
  }
  let total = add(base_ink, head_ink);
  let strength = dot(total, total).sqrt();
  if strength < 1.0 {
    return false;
  }
  let ink = total.map(|channel| channel / strength);

  // The ink has the same colour in both images.
  for image_ink in [base_ink, head_ink] {
    let (along, off) = decompose(image_ink, ink);
    if off > INK_TURN * along + SLACK {
      return false;
    }
  }

  let mut amounts = [(0.0f32, 0.0f32); 3];
  let mut peak = 0.0f32;
  let mut furthest_off = 0.0f32;
  for (index, amount) in amounts.iter_mut().enumerate().take(span.length) {
    let (base_amount, base_off) = decompose(sub(span.base[index], background), ink);
    let (head_amount, head_off) = decompose(sub(span.head[index], background), ink);
    // Ink the other way — lighter than the background where the rest is
    // darker — is something else than this stroke.
    if base_amount < -SLACK || head_amount < -SLACK {
      return false;
    }
    *amount = (base_amount.max(0.0), head_amount.max(0.0));
    peak = peak.max(base_amount).max(head_amount);
    furthest_off = furthest_off.max(base_off).max(head_off);
  }
  if furthest_off > OFF_AXIS * peak + SLACK {
    return false;
  }

  let (mut base_mass, mut head_mass, mut base_moment, mut head_moment) = (0.0, 0.0, 0.0, 0.0);
  for (index, &(base_amount, head_amount)) in amounts[..span.length].iter().enumerate() {
    base_mass += base_amount;
    head_mass += head_amount;
    base_moment += index as f32 * base_amount;
    head_moment += index as f32 * head_amount;
  }
  let (less, more) = (base_mass.min(head_mass), base_mass.max(head_mass));
  if less <= 0.0 || less < (1.0 - MASS_TOLERANCE) * more {
    return false;
  }
  (base_moment / base_mass - head_moment / head_mass).abs() <= MAX_SHIFT
}

/// Whether the difference at `(x, y)` is one shape rendered twice, less than
/// a pixel apart. `same` is the largest colour distance at which two pixels
/// still count as unchanged.
pub(crate) fn is_subpixel_shift(base: &View, head: &View, x: usize, y: usize, same: i64) -> bool {
  let (width, height) = (base.width as isize, base.height as isize);
  let index_at = |dx: isize, dy: isize, step: isize| -> Option<usize> {
    let (nx, ny) = (x as isize + dx * step, y as isize + dy * step);
    (nx >= 0 && ny >= 0 && nx < width && ny < height).then(|| (ny * width + nx) as usize)
  };
  let unchanged = |index: usize| color_delta(base.pixels[index], head.pixels[index]) <= same;

  for (before, after) in REACHES {
    for (dx, dy) in AXES {
      let (Some(from), Some(to)) = (index_at(dx, dy, -before), index_at(dx, dy, after)) else {
        continue;
      };
      if !unchanged(from) || !unchanged(to) {
        continue;
      }

      let mut span = Span {
        base: [[0.0; 3]; 3],
        head: [[0.0; 3]; 3],
        length: 0,
      };
      for step in 1 - before..after {
        // Between two points inside the image, so inside it too.
        let Some(index) = index_at(dx, dy, step) else {
          continue;
        };
        span.base[span.length] = rgb(base.pixels[index]);
        span.head[span.length] = rgb(head.pixels[index]);
        span.length += 1;
      }

      let (from_pixel, to_pixel) = (base.pixels[from], base.pixels[to]);
      let explained = if color_delta(from_pixel, to_pixel) > same {
        is_same_edge(&span, rgb(from_pixel), rgb(to_pixel))
      } else {
        let (from_color, to_color) = (rgb(from_pixel), rgb(to_pixel));
        let background = [0, 1, 2].map(|channel| (from_color[channel] + to_color[channel]) / 2.0);
        is_same_stroke(&span, background)
      };
      if explained {
        return true;
      }
    }
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;

  fn grey(level: f32) -> Rgb {
    [level; 3]
  }

  fn span(base: &[f32], head: &[f32]) -> Span {
    let mut span = Span {
      base: [[0.0; 3]; 3],
      head: [[0.0; 3]; 3],
      length: base.len(),
    };
    for index in 0..base.len() {
      span.base[index] = grey(base[index]);
      span.head[index] = grey(head[index]);
    }
    span
  }

  const WHITE: f32 = 255.0;
  const BLACK: f32 = 0.0;

  #[test]
  fn an_edge_may_move_by_a_fraction_of_a_pixel() {
    let edge =
      |base: &[f32], head: &[f32]| is_same_edge(&span(base, head), grey(WHITE), grey(BLACK));
    // The mix ratio changes: the edge moved by 0.3 px.
    assert!(edge(&[153.0], &[76.5]));
    // The blend spills from one pixel into the next: still 0.3 px.
    assert!(edge(&[255.0, 51.0], &[229.5, 0.0]));
    // Just under the limit of 0.6 px, and just over.
    assert!(edge(&[255.0], &[105.0]));
    assert!(!edge(&[255.0], &[95.0]));
  }

  #[test]
  fn an_edge_that_moved_a_whole_pixel_is_a_change() {
    let edge =
      |base: &[f32], head: &[f32]| is_same_edge(&span(base, head), grey(WHITE), grey(BLACK));
    // Hard edge, one pixel over.
    assert!(!edge(&[255.0], &[0.0]));
    // Anti-aliased edge, one pixel over.
    assert!(!edge(&[153.0, 0.0], &[255.0, 153.0]));
    // Two pixels swapping sides cancel out in the sum, not one by one.
    assert!(!edge(&[255.0, 0.0], &[0.0, 255.0]));
  }

  #[test]
  fn a_pixel_that_is_not_a_mix_of_its_sides_is_a_change() {
    let between = |base: Rgb, head: Rgb| {
      let span = Span {
        base: [base, [0.0; 3], [0.0; 3]],
        head: [head, [0.0; 3], [0.0; 3]],
        length: 1,
      };
      is_same_edge(&span, grey(WHITE), grey(BLACK))
    };
    // Grey to slightly warmer grey: a fringe.
    assert!(between(grey(128.0), [150.0, 128.0, 110.0]));
    // Grey to pure red: not something black and white add up to.
    assert!(!between(grey(128.0), [255.0, 0.0, 0.0]));
    // Brighter than the bright side.
    let dim =
      |base: f32, head: f32| is_same_edge(&span(&[base], &[head]), grey(200.0), grey(100.0));
    assert!(dim(150.0, 170.0));
    assert!(!dim(150.0, 240.0));
  }

  #[test]
  fn a_stroke_may_be_snapped_to_another_pixel_grid() {
    let stroke = |base: &[f32], head: &[f32]| is_same_stroke(&span(base, head), grey(WHITE));
    // A stem straddling two pixels in one render, sitting on one in the other.
    assert!(stroke(&[127.5, 127.5], &[0.0, 255.0]));
    assert!(stroke(&[127.5, 127.5], &[255.0, 0.0]));
    // The same over three pixels.
    assert!(stroke(&[191.0, 0.0, 191.0], &[255.0, 0.0, 127.5]));
    // A hairline a little lighter.
    assert!(stroke(&[100.0], &[125.0]));
  }

  #[test]
  fn a_stroke_that_moved_changed_weight_or_colour_is_a_change() {
    let stroke = |base: &[f32], head: &[f32]| is_same_stroke(&span(base, head), grey(WHITE));
    // One pixel over.
    assert!(!stroke(&[0.0, 255.0], &[255.0, 0.0]));
    // Twice the ink.
    assert!(!stroke(&[0.0, 255.0], &[0.0, 0.0]));
    // Appeared.
    assert!(!stroke(&[255.0], &[0.0]));
    // Much lighter.
    assert!(!stroke(&[0.0], &[120.0]));

    // Same weight, other colour.
    let recoloured = Span {
      base: [[60.0, 60.0, 60.0], [0.0; 3], [0.0; 3]],
      head: [[60.0, 60.0, 250.0], [0.0; 3], [0.0; 3]],
      length: 1,
    };
    assert!(!is_same_stroke(&recoloured, grey(WHITE)));
    // Lighter than its background in one image, darker in the other.
    assert!(!is_same_stroke(&span(&[100.0], &[160.0]), grey(128.0)));
  }

  #[test]
  fn blends_translucent_pixels_over_white() {
    assert_eq!(
      rgb(u32::from_ne_bytes([10, 20, 30, 255])),
      [10.0, 20.0, 30.0]
    );
    assert_eq!(rgb(u32::from_ne_bytes([10, 20, 30, 0])), [255.0; 3]);
    let half = rgb(u32::from_ne_bytes([0, 0, 0, 128]));
    assert!((half[0] - 127.0).abs() < 0.01, "{half:?}");
  }
}
