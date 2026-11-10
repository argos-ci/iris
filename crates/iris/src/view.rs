use crate::image::Image;

/// An image placed on the canvas a comparison runs on: the larger width and
/// the larger height of the two images. What lies beyond the image is
/// transparent, which is what padding it would have produced, without the
/// copy — on two screenshots of different sizes, that copy was most of the
/// memory a comparison took.
#[derive(Clone, Copy)]
pub(crate) struct View<'a> {
  pixels: &'a [u32],
  /// Size of the image itself: `pixels` holds `stride * rows` pixels.
  stride: usize,
  rows: usize,
  /// Size of the canvas.
  pub width: usize,
  pub height: usize,
}

impl<'a> View<'a> {
  pub fn new(image: &'a Image, width: usize, height: usize) -> Self {
    debug_assert!(image.width() as usize <= width && image.height() as usize <= height);
    Self {
      pixels: image.pixels(),
      stride: image.width() as usize,
      rows: image.height() as usize,
      width,
      height,
    }
  }

  /// A view of packed pixels filling the whole canvas.
  #[cfg(test)]
  pub fn whole(pixels: &'a [u32], width: usize) -> Self {
    Self {
      pixels,
      stride: width,
      rows: pixels.len() / width,
      width,
      height: pixels.len() / width,
    }
  }

  /// Width and height of the image itself.
  pub fn image_size(&self) -> (usize, usize) {
    (self.stride, self.rows)
  }

  /// The pixel at `(x, y)` of the canvas, transparent outside the image.
  #[inline(always)]
  pub fn at(&self, x: usize, y: usize) -> u32 {
    if x < self.stride && y < self.rows {
      self.pixels[y * self.stride + x]
    } else {
      0
    }
  }

  /// Whether the 3x3 block centred on `(x, y)` lies inside the image, where
  /// it can be read without checking each pixel.
  #[inline(always)]
  pub fn surrounds(&self, x: usize, y: usize) -> bool {
    x >= 1 && y >= 1 && x + 1 < self.stride && y + 1 < self.rows
  }

  /// Three pixels of row `y`, from column `x`. The caller has checked the
  /// image holds them, through [`View::surrounds`].
  #[inline(always)]
  pub fn three(&self, x: usize, y: usize) -> [u32; 3] {
    let start = y * self.stride + x;
    self.pixels[start..start + 3]
      .try_into()
      .expect("three pixels")
  }

  /// Columns `x0..=x1` of row `y`.
  pub fn row(&self, y: usize, x0: usize, x1: usize) -> Row<'a> {
    let length = x1 + 1 - x0;
    if y >= self.rows || x0 >= self.stride {
      return Row {
        pixels: &[],
        zeros: length,
      };
    }
    let end = (x1 + 1).min(self.stride);
    let start = y * self.stride;
    Row {
      pixels: &self.pixels[start + x0..start + end],
      zeros: length - (end - x0),
    }
  }

  /// Row `y` across the whole canvas, borrowed when the image covers it and
  /// copied into `scratch` when it does not.
  pub fn full_row<'s>(&self, y: usize, scratch: &'s mut Vec<u32>) -> &'s [u32]
  where
    'a: 's,
  {
    let row = self.row(y, 0, self.width - 1);
    if row.zeros == 0 {
      return row.pixels;
    }
    scratch.clear();
    scratch.extend_from_slice(row.pixels);
    scratch.resize(self.width, 0);
    scratch
  }
}

/// Part of a row of the canvas: pixels of the image, then transparent ones.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Row<'a> {
  pub pixels: &'a [u32],
  pub zeros: usize,
}

impl Row<'_> {
  pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
    self
      .pixels
      .iter()
      .copied()
      .chain(std::iter::repeat_n(0, self.zeros))
  }
}

/// Rows of equal length are equal when their pixels are, wherever each one's
/// image ends.
impl PartialEq for Row<'_> {
  fn eq(&self, other: &Self) -> bool {
    debug_assert_eq!(
      self.pixels.len() + self.zeros,
      other.pixels.len() + other.zeros
    );
    let (short, long) = if self.pixels.len() <= other.pixels.len() {
      (self, other)
    } else {
      (other, self)
    };
    let (shared, rest) = long.pixels.split_at(short.pixels.len());
    short.pixels == shared && rest.iter().all(|&pixel| pixel == 0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn numbered(width: u32, height: u32) -> Image {
    let pixels = (1..=width * height).collect();
    Image::new(width, height, pixels).unwrap()
  }

  #[test]
  fn reads_transparent_pixels_past_the_image() {
    let image = numbered(3, 2);
    let view = View::new(&image, 5, 4);
    assert_eq!((view.at(0, 0), view.at(2, 1)), (1, 6));
    assert_eq!((view.at(3, 0), view.at(0, 2), view.at(4, 3)), (0, 0, 0));
    assert!(!view.surrounds(1, 1));
    assert_eq!(view.image_size(), (3, 2));
  }

  #[test]
  fn cuts_rows_where_the_image_ends() {
    let image = numbered(3, 2);
    let view = View::new(&image, 5, 4);
    let collect = |row: Row| row.iter().collect::<Vec<_>>();
    assert_eq!(collect(view.row(1, 0, 4)), [4, 5, 6, 0, 0]);
    assert_eq!(collect(view.row(1, 2, 3)), [6, 0]);
    assert_eq!(collect(view.row(0, 3, 4)), [0, 0]);
    assert_eq!(collect(view.row(3, 0, 4)), [0; 5]);

    let mut scratch = Vec::new();
    assert_eq!(view.full_row(0, &mut scratch), [1, 2, 3, 0, 0]);
    let whole = numbered(5, 1);
    let whole = View::new(&whole, 5, 1);
    assert_eq!(whole.full_row(0, &mut scratch), [1, 2, 3, 4, 5]);
  }

  #[test]
  fn compares_rows_by_their_pixels() {
    let row = |pixels: &'static [u32], zeros| Row { pixels, zeros };
    assert_eq!(row(&[1, 2, 0], 1), row(&[1, 2], 2));
    assert_eq!(row(&[], 3), row(&[0, 0, 0], 0));
    assert_ne!(row(&[1, 2, 3], 0), row(&[1, 2], 1));
    assert_ne!(row(&[1, 0, 3], 0), row(&[1], 2));
  }
}
