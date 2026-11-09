//! Anti-aliasing detection, from "Anti-aliased Pixel and Intensity Slope
//! Detector" (Vyšniauskas, 2009) as implemented by pixelmatch and odiff.
//!
//! A pixel that differs between two renders of the same content is usually on
//! an edge that was rasterised slightly differently. Such a pixel sits between
//! a darker and a brighter neighbour, and at least one of those two belongs to
//! a flat area in both images. A pixel in the middle of a real change does not.
//!
//! This is odiff's variant, including where it departs from the paper — a
//! pixel on the border of the image starts with one "equal" neighbour, and
//! brightness is compared in floating point, ties going to the neighbour met
//! first — because the result has to match pixel for pixel.

use crate::delta::brightness;

/// A borrowed image, both images of a comparison sharing one size.
#[derive(Clone, Copy)]
pub(crate) struct View<'a> {
  pub pixels: &'a [u32],
  pub width: usize,
  pub height: usize,
}

/// The 3x3 block of pixels centred on a pixel, row by row. Slots that fall
/// outside the image are absent.
struct Neighbourhood {
  pixels: [u32; 9],
  /// Bit `n` is set when slot `n` is inside the image.
  present: u32,
}

const ALL_PRESENT: u32 = 0x1ff;
const CENTER: usize = 4;

impl View<'_> {
  #[inline(always)]
  fn neighbourhood(&self, x: usize, y: usize) -> Neighbourhood {
    let width = self.width;
    let index = y * width + x;
    if x >= 1 && y >= 1 && x + 1 < width && y + 1 < self.height {
      // Three fixed-size reads: the common case, with no per-pixel checks.
      let row = |start: usize| -> [u32; 3] {
        self.pixels[start..start + 3]
          .try_into()
          .expect("three pixels")
      };
      let (above, level, below) = (
        row(index - width - 1),
        row(index - 1),
        row(index + width - 1),
      );
      return Neighbourhood {
        pixels: [
          above[0], above[1], above[2], level[0], level[1], level[2], below[0], below[1], below[2],
        ],
        present: ALL_PRESENT,
      };
    }

    let mut pixels = [0u32; 9];
    let mut present = 0;
    for (slot, pixel) in pixels.iter_mut().enumerate() {
      let (nx, ny) = (x + slot % 3, y + slot / 3);
      if (1..=width).contains(&nx) && (1..=self.height).contains(&ny) {
        *pixel = self.pixels[(ny - 1) * width + nx - 1];
        present |= 1 << slot;
      }
    }
    Neighbourhood { pixels, present }
  }
}

impl Neighbourhood {
  /// Number of neighbours of exactly the centre's colour. A pixel on the
  /// border of the image gets one for free, as in odiff.
  #[inline(always)]
  fn equal_neighbours(&self) -> u32 {
    let center = self.pixels[CENTER];
    let mut same = 0;
    for slot in 0..9 {
      same += u32::from(self.pixels[slot] == center) & (self.present >> slot);
    }
    same - 1 + u32::from(self.present != ALL_PRESENT)
  }
}

/// Whether the pixel has at least three neighbours of its exact colour, which
/// puts it inside a flat area rather than on an edge.
#[inline]
fn has_many_siblings(image: &View, (x, y): (usize, usize)) -> bool {
  image.neighbourhood(x, y).equal_neighbours() >= 3
}

/// Whether the pixel at `(x, y)` of `image` is anti-aliasing, judged against
/// the same neighbourhood in `other`.
#[inline]
fn detect(image: &View, other: &View, x: usize, y: usize) -> bool {
  let around = image.neighbourhood(x, y);

  // Most differing pixels sit in a flat area of at least one image, and are
  // settled here, on integer compares alone. odiff interleaves this count
  // with the search below and stops searching at three; counting first gives
  // the same answer, because three equal neighbours end in `false` either way.
  if around.equal_neighbours() >= 3 {
    return false;
  }

  // Extremes of `center - neighbour`: the lowest is the brightest neighbour,
  // the highest the darkest one.
  let center = around.pixels[CENTER];
  let luma = brightness(center);
  let mut lowest = 0.0f64;
  let mut highest = 0.0f64;
  let mut lowest_slot = CENTER;
  let mut highest_slot = CENTER;
  for slot in 0..9 {
    let neighbour = around.pixels[slot];
    if neighbour == center || around.present & (1 << slot) == 0 {
      continue;
    }
    let delta = luma - brightness(neighbour);
    if delta < lowest {
      lowest = delta;
      lowest_slot = slot;
    } else if delta > highest {
      highest = delta;
      highest_slot = slot;
    }
  }

  // No neighbour on one side of it in brightness: an anti-aliased pixel is a
  // blend, so it has both.
  if lowest == 0.0 || highest == 0.0 {
    return false;
  }

  // A slot is only ever recorded when present, so these stay in the image.
  let position = |slot: usize| (x + slot % 3 - 1, y + slot / 3 - 1);
  let (lowest_at, highest_at) = (position(lowest_slot), position(highest_slot));
  (has_many_siblings(image, lowest_at) || has_many_siblings(image, highest_at))
    && (has_many_siblings(other, lowest_at) || has_many_siblings(other, highest_at))
}

/// Whether a differing pixel is anti-aliasing in either image.
#[inline]
pub(crate) fn is_antialiased(base: &View, head: &View, x: usize, y: usize) -> bool {
  detect(base, head, x, y) || detect(head, base, x, y)
}

#[cfg(test)]
mod tests {
  use super::*;

  const WHITE: u32 = u32::from_ne_bytes([255, 255, 255, 255]);
  const BLACK: u32 = u32::from_ne_bytes([0, 0, 0, 255]);
  const GREY: u32 = u32::from_ne_bytes([128, 128, 128, 255]);

  fn view(pixels: &[u32], width: usize) -> View<'_> {
    View {
      pixels,
      width,
      height: pixels.len() / width,
    }
  }

  /// A vertical black/white edge at `edge`, with `blend` in the column on it.
  fn edge(width: usize, height: usize, edge: usize, blend: u32) -> Vec<u32> {
    (0..width * height)
      .map(|index| match (index % width).cmp(&edge) {
        std::cmp::Ordering::Less => BLACK,
        std::cmp::Ordering::Equal => blend,
        std::cmp::Ordering::Greater => WHITE,
      })
      .collect()
  }

  #[test]
  fn flat_areas_are_not_antialiased() {
    let base = vec![WHITE; 25];
    let mut head = base.clone();
    head[12] = BLACK;
    // The changed pixel has no neighbour brighter than white.
    assert!(!is_antialiased(&view(&base, 5), &view(&head, 5), 2, 2));
  }

  #[test]
  fn a_blended_pixel_between_two_flat_areas_is_antialiased() {
    // A single grey pixel on an otherwise hard edge: it has darker and
    // brighter neighbours, and both of those sit in flat areas of both images.
    let base = edge(7, 7, 3, WHITE);
    let mut head = base.clone();
    head[3 * 7 + 3] = GREY;
    assert!(is_antialiased(&view(&base, 7), &view(&head, 7), 3, 3));
  }

  #[test]
  fn a_blended_edge_column_is_antialiased() {
    // What a re-rasterised edge looks like: a full column of blends between
    // two flat areas.
    let base = edge(7, 7, 3, WHITE);
    let head = edge(7, 7, 3, GREY);
    let (base, head) = (view(&base, 7), view(&head, 7));
    for y in 0..7 {
      assert!(is_antialiased(&base, &head, 3, y), "row {y}");
    }
  }

  #[test]
  fn a_solid_block_is_a_real_change() {
    let base = vec![WHITE; 49];
    let mut head = base.clone();
    for y in 2..5 {
      for x in 2..5 {
        head[y * 7 + x] = BLACK;
      }
    }
    let (base, head) = (view(&base, 7), view(&head, 7));
    for y in 2..5 {
      for x in 2..5 {
        assert!(!is_antialiased(&base, &head, x, y), "({x}, {y})");
      }
    }
  }

  #[test]
  fn is_symmetric() {
    let base = edge(7, 7, 3, WHITE);
    let mut head = base.clone();
    head[3 * 7 + 3] = GREY;
    let (base, head) = (view(&base, 7), view(&head, 7));
    assert_eq!(
      is_antialiased(&base, &head, 3, 3),
      is_antialiased(&head, &base, 3, 3)
    );
  }

  #[test]
  fn handles_one_pixel_images() {
    let (base, head) = ([WHITE], [BLACK]);
    assert!(!is_antialiased(&view(&base, 1), &view(&head, 1), 0, 0));
  }
}
