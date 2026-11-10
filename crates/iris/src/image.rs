use std::io::Cursor;
use std::path::Path;

use crate::error::{Error, Result};

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
const JPEG_SIGNATURE: [u8; 3] = [0xff, 0xd8, 0xff];

/// A decoded image, 8-bit RGBA, not premultiplied.
///
/// Each pixel is one `u32` whose bytes in memory are `[r, g, b, a]`, so two
/// pixels are identical exactly when their `u32` are — which is what lets the
/// comparison skip unchanged runs a word at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
  width: u32,
  height: u32,
  pixels: Vec<u32>,
}

impl Image {
  /// Wrap packed pixels. `pixels.len()` must be `width * height`.
  pub fn new(width: u32, height: u32, pixels: Vec<u32>) -> Result<Self> {
    if width == 0 || height == 0 {
      return Err(Error::Decode("image has no pixels".into()));
    }
    if pixels.len() != pixel_count(width, height)? {
      return Err(Error::Decode(format!(
        "expected {} pixels for {width}x{height}, got {}",
        width as u64 * height as u64,
        pixels.len()
      )));
    }
    Ok(Self {
      width,
      height,
      pixels,
    })
  }

  /// Build an image from `width * height * 4` RGBA bytes.
  pub fn from_rgba8(width: u32, height: u32, rgba: &[u8]) -> Result<Self> {
    let count = pixel_count(width, height)?;
    if rgba.len() != count * 4 {
      return Err(Error::Decode(format!(
        "expected {} bytes for {width}x{height}, got {}",
        count * 4,
        rgba.len()
      )));
    }
    let mut pixels = vec![0u32; count];
    bytemuck::cast_slice_mut::<u32, u8>(&mut pixels).copy_from_slice(rgba);
    Self::new(width, height, pixels)
  }

  /// Decode a PNG or a JPEG, told apart by their signature.
  pub fn decode(bytes: &[u8]) -> Result<Self> {
    if bytes.starts_with(&PNG_SIGNATURE) {
      return decode_png(bytes);
    }
    if bytes.starts_with(&JPEG_SIGNATURE) {
      return decode_jpeg(bytes);
    }
    Err(Error::UnsupportedFormat)
  }

  /// Read and decode a file.
  pub fn open(path: impl AsRef<Path>) -> Result<Self> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|source| Error::Io {
      path: path.to_path_buf(),
      source,
    })?;
    Self::decode(&bytes)
  }

  pub fn width(&self) -> u32 {
    self.width
  }

  pub fn height(&self) -> u32 {
    self.height
  }

  /// Packed pixels, row by row.
  pub fn pixels(&self) -> &[u32] {
    &self.pixels
  }

  /// RGBA bytes, row by row.
  pub fn as_rgba8(&self) -> &[u8] {
    bytemuck::cast_slice(&self.pixels)
  }

  /// Copy the image onto a larger transparent canvas, anchored top-left.
  pub(crate) fn padded(&self, width: u32, height: u32) -> Image {
    debug_assert!(width >= self.width && height >= self.height);
    let mut pixels = vec![0u32; width as usize * height as usize];
    let src_width = self.width as usize;
    for (src, dst) in self
      .pixels
      .chunks_exact(src_width)
      .zip(pixels.chunks_exact_mut(width as usize))
    {
      dst[..src_width].copy_from_slice(src);
    }
    Image {
      width,
      height,
      pixels,
    }
  }
}

fn pixel_count(width: u32, height: u32) -> Result<usize> {
  (width as usize)
    .checked_mul(height as usize)
    .filter(|count| count.checked_mul(4).is_some())
    .ok_or_else(|| Error::Decode(format!("{width}x{height} does not fit in memory")))
}

fn decode_png(bytes: &[u8]) -> Result<Image> {
  let png_error = |error: png::DecodingError| Error::Decode(error.to_string());

  let mut decoder = png::Decoder::new(Cursor::new(bytes));
  // The default limit is 64 MiB of decoder-side allocations, which a large
  // interlaced screenshot exceeds. The pixel buffer itself is ours.
  decoder.set_limits(png::Limits {
    bytes: 1024 * 1024 * 1024,
  });
  decoder.set_transformations(
    png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
  );
  let mut reader = decoder.read_info().map_err(png_error)?;

  let (width, height) = {
    let info = reader.info();
    (info.width, info.height)
  };
  let count = pixel_count(width, height)?;
  let (color, depth) = reader.output_color_type();
  if depth != png::BitDepth::Eight {
    return Err(Error::Decode(format!("unexpected bit depth {depth:?}")));
  }

  let mut pixels = vec![0u32; count];
  match color {
    png::ColorType::Rgba => {
      reader
        .next_frame(bytemuck::cast_slice_mut(&mut pixels))
        .map_err(png_error)?;
    }
    // `EXPAND | ALPHA` leaves greyscale images with two channels.
    png::ColorType::GrayscaleAlpha => {
      let mut samples = vec![0u8; count * 2];
      reader.next_frame(&mut samples).map_err(png_error)?;
      let (samples, _) = samples.as_chunks::<2>();
      for (pixel, &[grey, alpha]) in pixels.iter_mut().zip(samples) {
        *pixel = u32::from_ne_bytes([grey, grey, grey, alpha]);
      }
    }
    other => {
      return Err(Error::Decode(format!("unexpected color type {other:?}")));
    }
  }

  Image::new(width, height, pixels)
}

#[cfg(feature = "jpeg")]
fn decode_jpeg(bytes: &[u8]) -> Result<Image> {
  use zune_jpeg::zune_core::bytestream::ZCursor;
  use zune_jpeg::zune_core::colorspace::ColorSpace;
  use zune_jpeg::zune_core::options::DecoderOptions;

  let jpeg_error = |error: zune_jpeg::errors::DecodeErrors| Error::Decode(error.to_string());

  let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
  let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
  let rgba = decoder.decode().map_err(jpeg_error)?;
  let info = decoder
    .info()
    .ok_or_else(|| Error::Decode("missing JPEG header".into()))?;
  Image::from_rgba8(info.width as u32, info.height as u32, &rgba)
}

#[cfg(not(feature = "jpeg"))]
fn decode_jpeg(_bytes: &[u8]) -> Result<Image> {
  Err(Error::UnsupportedFormat)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rejects_empty_and_mismatched_buffers() {
    assert!(Image::new(0, 4, vec![]).is_err());
    assert!(Image::new(2, 2, vec![0; 3]).is_err());
    assert!(Image::from_rgba8(2, 2, &[0; 15]).is_err());
  }

  #[test]
  fn keeps_byte_order() {
    let image = Image::from_rgba8(1, 1, &[1, 2, 3, 4]).unwrap();
    assert_eq!(image.pixels()[0].to_ne_bytes(), [1, 2, 3, 4]);
    assert_eq!(image.as_rgba8(), &[1, 2, 3, 4]);
  }

  #[test]
  fn pads_top_left_with_transparent_pixels() {
    let image = Image::from_rgba8(2, 1, &[1, 1, 1, 255, 2, 2, 2, 255]).unwrap();
    let padded = image.padded(3, 2);
    assert_eq!((padded.width(), padded.height()), (3, 2));
    assert_eq!(&padded.pixels()[..2], image.pixels());
    assert!(padded.pixels()[2..].iter().all(|&pixel| pixel == 0));
  }

  #[test]
  fn decodes_greyscale_pngs_as_rgba() {
    let encode = |color: png::ColorType, samples: &[u8]| {
      let mut output = Vec::new();
      let mut encoder = png::Encoder::new(&mut output, 2, 1);
      encoder.set_color(color);
      encoder.set_depth(png::BitDepth::Eight);
      let mut writer = encoder.write_header().unwrap();
      writer.write_image_data(samples).unwrap();
      writer.finish().unwrap();
      output
    };
    let grey = Image::decode(&encode(png::ColorType::Grayscale, &[10, 200])).unwrap();
    assert_eq!(grey.as_rgba8(), &[10, 10, 10, 255, 200, 200, 200, 255]);
    let translucent =
      Image::decode(&encode(png::ColorType::GrayscaleAlpha, &[10, 0, 200, 128])).unwrap();
    assert_eq!(translucent.as_rgba8(), &[10, 10, 10, 0, 200, 200, 200, 128]);
  }

  #[test]
  fn rejects_unknown_formats() {
    assert!(matches!(
      Image::decode(b"GIF89a"),
      Err(Error::UnsupportedFormat)
    ));
  }
}
