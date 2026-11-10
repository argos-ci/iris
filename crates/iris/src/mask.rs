use std::io::Cursor;

use crate::error::{Error, Result};
use crate::image::Image;

/// An axis-aligned rectangle, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
  pub x: u32,
  pub y: u32,
  pub width: u32,
  pub height: u32,
}

/// Which pixels differ: one bit per pixel of the comparison canvas.
///
/// Rows are packed most significant bit first and padded to a whole byte,
/// which is the layout of a 1-bit PNG, so encoding is a straight copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mask {
  width: u32,
  height: u32,
  stride: usize,
  bits: Vec<u8>,
  bounds: Option<Rect>,
  count: u64,
}

impl Mask {
  /// Build a mask from a predicate, one call per pixel.
  pub fn from_fn(width: u32, height: u32, mut is_set: impl FnMut(u32, u32) -> bool) -> Self {
    let stride = (width as usize).div_ceil(8);
    let mut bits = vec![0u8; stride * height as usize];
    let mut extent = Extent::default();
    for y in 0..height {
      let row = &mut bits[y as usize * stride..][..stride];
      for x in 0..width {
        if is_set(x, y) {
          row[x as usize / 8] |= 0x80 >> (x % 8);
          extent.include(x as usize, y as usize);
        }
      }
    }
    Self::assemble(width, height, bits, extent)
  }

  /// Wrap packed rows, `extent` describing the bits set in them.
  pub(crate) fn from_bits(width: u32, height: u32, bits: Vec<u8>, extent: Extent) -> Self {
    debug_assert_eq!(bits.len(), (width as usize).div_ceil(8) * height as usize);
    Self::assemble(width, height, bits, extent)
  }

  /// Read a mask back from a PNG: this crate's own output, or a legacy RGBA
  /// mask where differing pixels are opaque red on a transparent background.
  ///
  /// A file cut short is read up to its last complete row and treated as an
  /// image of that height, which is what `@argos-ci/mask-fingerprint` does:
  /// such masks exist in storage and already have fingerprints.
  pub fn from_png(bytes: &[u8]) -> Result<Self> {
    let png_error = |error: png::DecodingError| Error::Decode(error.to_string());

    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(
      png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder.read_info().map_err(png_error)?;
    let (width, height, interlaced) = {
      let info = reader.info();
      (info.width, info.height, info.interlaced)
    };
    // Interlaced rows do not arrive top to bottom; take the slow road.
    if interlaced || reader.output_color_type().0 != png::ColorType::Rgba {
      let image = Image::decode(bytes)?;
      let pixels = image.pixels();
      return Ok(Self::from_fn(width, height, |x, y| {
        is_marked(pixels[(y * width + x) as usize].to_ne_bytes())
      }));
    }

    let stride = (width as usize).div_ceil(8);
    let mut bits = Vec::with_capacity(stride * height as usize);
    let mut extent = Extent::default();
    let mut rows = 0usize;
    loop {
      let row = match reader.next_row() {
        Ok(Some(row)) => row,
        Ok(None) => break,
        Err(error) if is_truncation(&error) => break,
        Err(error) => return Err(png_error(error)),
      };
      bits.resize(bits.len() + stride, 0);
      let packed = &mut bits[rows * stride..];
      let (pixels, _) = row.data().as_chunks::<4>();
      for (x, &pixel) in pixels.iter().enumerate() {
        if is_marked(pixel) {
          packed[x / 8] |= 0x80 >> (x % 8);
          extent.include(x, rows);
        }
      }
      rows += 1;
    }

    if rows == 0 {
      return Ok(Self::from_fn(width, height, |_, _| false));
    }
    Ok(Self::assemble(width, rows as u32, bits, extent))
  }

  fn assemble(width: u32, height: u32, bits: Vec<u8>, extent: Extent) -> Self {
    Self {
      width,
      height,
      stride: (width as usize).div_ceil(8),
      bits,
      bounds: extent.rect(),
      count: extent.count,
    }
  }

  pub fn width(&self) -> u32 {
    self.width
  }

  pub fn height(&self) -> u32 {
    self.height
  }

  /// Number of differing pixels.
  pub fn count(&self) -> u64 {
    self.count
  }

  /// Smallest rectangle holding every differing pixel, `None` when empty.
  pub fn bounds(&self) -> Option<Rect> {
    self.bounds
  }

  pub fn is_set(&self, x: u32, y: u32) -> bool {
    x < self.width
      && y < self.height
      && self.bits[y as usize * self.stride + x as usize / 8] & (0x80 >> (x % 8)) != 0
  }

  /// One packed row.
  pub(crate) fn row(&self, y: usize) -> &[u8] {
    &self.bits[y * self.stride..][..self.stride]
  }

  /// Number of differing pixels of row `y` within columns `x0..=x1`.
  pub(crate) fn count_in_row(&self, y: usize, x0: usize, x1: usize) -> u64 {
    let row = self.row(y);
    let (first, last) = (x0 / 8, x1 / 8);
    let head = 0xffu8 >> (x0 % 8);
    let tail = 0xffu8 << (7 - x1 % 8);
    if first == last {
      return (row[first] & head & tail).count_ones() as u64;
    }
    let inner: u64 = row[first + 1..last]
      .iter()
      .map(|byte| byte.count_ones() as u64)
      .sum();
    (row[first] & head).count_ones() as u64 + inner + (row[last] & tail).count_ones() as u64
  }

  /// Encode as a 1-bit indexed PNG: transparent where the images agree, opaque
  /// red where they differ. An 8-megapixel mask is a megabyte before
  /// compression instead of the 32 an RGBA one would be, and it renders the
  /// same.
  pub fn to_png(&self) -> Result<Vec<u8>> {
    let png_error = |error: png::EncodingError| Error::Encode(error.to_string());

    let mut output = Vec::with_capacity(1024 + self.bits.len() / 64);
    let mut encoder = png::Encoder::new(&mut output, self.width, self.height);
    encoder.set_color(png::ColorType::Indexed);
    encoder.set_depth(png::BitDepth::One);
    encoder.set_palette(&[0u8, 0, 0, 255, 0, 0][..]);
    encoder.set_trns(&[0u8, 255][..]);
    // Filters predict a byte from its neighbours, which does nothing for
    // packed bits; skipping them also skips the per-row heuristic.
    encoder.set_filter(png::Filter::NoFilter);
    // A stronger setting makes the file a third smaller and takes fifteen
    // times as long, which on a small change is most of the comparison.
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(png_error)?;
    writer.write_image_data(&self.bits).map_err(png_error)?;
    writer.finish().map_err(png_error)?;
    Ok(output)
  }
}

/// Whether a pixel of a stored mask marks a difference: opaque enough, and
/// red. The tolerances are those of `@argos-ci/mask-fingerprint`.
fn is_marked([r, g, b, a]: [u8; 4]) -> bool {
  r >= 200 && g <= 90 && b <= 90 && a >= 16
}

fn is_truncation(error: &png::DecodingError) -> bool {
  match error {
    png::DecodingError::IoError(io) => io.kind() == std::io::ErrorKind::UnexpectedEof,
    other => other
      .to_string()
      .to_lowercase()
      .contains("unexpected end of file"),
  }
}

/// Where the set pixels of a mask are, and how many there are.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Extent {
  min_x: usize,
  min_y: usize,
  max_x: usize,
  max_y: usize,
  count: u64,
}

impl Default for Extent {
  fn default() -> Self {
    Self {
      min_x: usize::MAX,
      min_y: usize::MAX,
      max_x: 0,
      max_y: 0,
      count: 0,
    }
  }
}

impl Extent {
  #[inline(always)]
  pub(crate) fn include(&mut self, x: usize, y: usize) {
    self.min_x = self.min_x.min(x);
    self.max_x = self.max_x.max(x);
    self.min_y = self.min_y.min(y);
    self.max_y = self.max_y.max(y);
    self.count += 1;
  }

  pub(crate) fn merge(&mut self, other: &Extent) {
    if other.count == 0 {
      return;
    }
    self.min_x = self.min_x.min(other.min_x);
    self.max_x = self.max_x.max(other.max_x);
    self.min_y = self.min_y.min(other.min_y);
    self.max_y = self.max_y.max(other.max_y);
    self.count += other.count;
  }

  fn rect(&self) -> Option<Rect> {
    (self.count > 0).then(|| Rect {
      x: self.min_x as u32,
      y: self.min_y as u32,
      width: (self.max_x - self.min_x + 1) as u32,
      height: (self.max_y - self.min_y + 1) as u32,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tracks_count_and_bounds() {
    let mask = Mask::from_fn(20, 10, |x, y| (3..=17).contains(&x) && (2..=4).contains(&y));
    assert_eq!(mask.count(), 15 * 3);
    assert_eq!(
      mask.bounds(),
      Some(Rect {
        x: 3,
        y: 2,
        width: 15,
        height: 3
      })
    );
    assert!(mask.is_set(3, 2) && mask.is_set(17, 4));
    assert!(!mask.is_set(2, 2) && !mask.is_set(18, 4) && !mask.is_set(3, 5));
    assert!(!mask.is_set(20, 0) && !mask.is_set(0, 10));
  }

  #[test]
  fn an_empty_mask_has_no_bounds() {
    let mask = Mask::from_fn(9, 9, |_, _| false);
    assert_eq!((mask.count(), mask.bounds()), (0, None));
  }

  #[test]
  fn counts_bits_within_a_column_range() {
    let mask = Mask::from_fn(40, 1, |x, _| x % 3 == 0);
    for x0 in 0..40 {
      for x1 in x0..40 {
        let expected = (x0..=x1).filter(|x| x % 3 == 0).count() as u64;
        assert_eq!(mask.count_in_row(0, x0, x1), expected, "{x0}..={x1}");
      }
    }
  }

  #[test]
  fn merges_extents() {
    let mut extent = Extent::default();
    let (mut left, mut right) = (Extent::default(), Extent::default());
    left.include(2, 9);
    right.include(7, 1);
    right.include(4, 3);
    extent.merge(&left);
    extent.merge(&Extent::default());
    extent.merge(&right);
    assert_eq!(extent.count, 3);
    assert_eq!(
      extent.rect(),
      Some(Rect {
        x: 2,
        y: 1,
        width: 6,
        height: 9
      })
    );
    assert_eq!(Extent::default().rect(), None);
  }

  #[test]
  fn survives_a_png_round_trip() {
    // 37 is not a multiple of 8: the last byte of each row is partly padding.
    let mask = Mask::from_fn(37, 23, |x, y| (x * 7 + y * 13) % 5 == 0);
    let png = mask.to_png().unwrap();
    assert_eq!(Mask::from_png(&png).unwrap(), mask);
  }

  #[test]
  fn reads_a_truncated_png_up_to_its_last_complete_row() {
    let mask = Mask::from_fn(64, 400, |x, y| (x * 3 + y * 5) % 11 == 0);
    let png = mask.to_png().unwrap();
    let truncated = Mask::from_png(&png[..png.len() * 2 / 3]).unwrap();
    assert_eq!(truncated.width(), 64);
    assert!(truncated.height() < 400, "{}", truncated.height());
    for y in 0..truncated.height() {
      for x in 0..64 {
        assert_eq!(truncated.is_set(x, y), mask.is_set(x, y), "({x}, {y})");
      }
    }
  }

  #[test]
  fn encodes_red_on_transparent() {
    let mask = Mask::from_fn(2, 1, |x, _| x == 1);
    let image = Image::decode(&mask.to_png().unwrap()).unwrap();
    assert_eq!(image.as_rgba8(), &[0, 0, 0, 0, 255, 0, 0, 255]);
  }
}
