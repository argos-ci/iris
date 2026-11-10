use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek};
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
    match Format::of(bytes)? {
      Format::Png => decode_png(Cursor::new(bytes)),
      Format::Jpeg => decode_jpeg(bytes),
    }
  }

  /// Read and decode a file. A PNG is decoded as it is read, without holding
  /// the file in memory.
  pub fn open(path: impl AsRef<Path>) -> Result<Self> {
    let path = path.as_ref();
    let io_error = |source| Error::Io {
      path: path.to_path_buf(),
      source,
    };
    let mut reader = BufReader::new(File::open(path).map_err(io_error)?);
    match Format::of(reader.fill_buf().map_err(io_error)?)? {
      Format::Png => decode_png(reader),
      Format::Jpeg => {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map_err(io_error)?;
        decode_jpeg(&bytes)
      }
    }
  }

  /// Width and height of an encoded image, read from its header alone.
  pub fn dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    match Format::of(bytes)? {
      Format::Png => png_dimensions(Cursor::new(bytes)),
      Format::Jpeg => jpeg_dimensions(bytes),
    }
  }

  /// Width and height of an image file, read from its header alone.
  pub fn file_dimensions(path: impl AsRef<Path>) -> Result<(u32, u32)> {
    let path = path.as_ref();
    let io_error = |source| Error::Io {
      path: path.to_path_buf(),
      source,
    };
    let mut reader = BufReader::new(File::open(path).map_err(io_error)?);
    match Format::of(reader.fill_buf().map_err(io_error)?)? {
      Format::Png => png_dimensions(reader),
      Format::Jpeg => {
        // A JPEG header can sit after any number of metadata segments.
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map_err(io_error)?;
        jpeg_dimensions(&bytes)
      }
    }
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
}

enum Format {
  Png,
  Jpeg,
}

impl Format {
  fn of(bytes: &[u8]) -> Result<Self> {
    if bytes.starts_with(&PNG_SIGNATURE) {
      Ok(Format::Png)
    } else if bytes.starts_with(&JPEG_SIGNATURE) {
      Ok(Format::Jpeg)
    } else {
      Err(Error::UnsupportedFormat)
    }
  }
}

fn pixel_count(width: u32, height: u32) -> Result<usize> {
  (width as usize)
    .checked_mul(height as usize)
    .filter(|count| count.checked_mul(4).is_some())
    .ok_or_else(|| Error::Decode(format!("{width}x{height} does not fit in memory")))
}

fn png_error(error: png::DecodingError) -> Error {
  Error::Decode(error.to_string())
}

fn png_dimensions(reader: impl BufRead + Seek) -> Result<(u32, u32)> {
  let reader = png::Decoder::new(reader).read_info().map_err(png_error)?;
  let info = reader.info();
  Ok((info.width, info.height))
}

fn decode_png(reader: impl BufRead + Seek) -> Result<Image> {
  let mut decoder = png::Decoder::new(reader);
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
  let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut pixels);
  match color {
    png::ColorType::Rgba => {
      reader.next_frame(bytes).map_err(png_error)?;
    }
    // `EXPAND | ALPHA` leaves greyscale images with two channels. They are
    // decoded into the first half of the buffer and widened in place, from
    // the end: pixel `i` lands on bytes `4i..4i + 4`, past the two it is read
    // from, so no sample is overwritten before it is read.
    png::ColorType::GrayscaleAlpha => {
      reader
        .next_frame(&mut bytes[..count * 2])
        .map_err(png_error)?;
      for index in (0..count).rev() {
        let (grey, alpha) = (bytes[2 * index], bytes[2 * index + 1]);
        bytes[4 * index..4 * index + 4].copy_from_slice(&[grey, grey, grey, alpha]);
      }
    }
    other => {
      return Err(Error::Decode(format!("unexpected color type {other:?}")));
    }
  }

  Image::new(width, height, pixels)
}

#[cfg(feature = "jpeg")]
fn jpeg_error(error: zune_jpeg::errors::DecodeErrors) -> Error {
  Error::Decode(error.to_string())
}

#[cfg(feature = "jpeg")]
fn jpeg_decoder(
  bytes: &[u8],
) -> zune_jpeg::JpegDecoder<zune_jpeg::zune_core::bytestream::ZCursor<&[u8]>> {
  use zune_jpeg::zune_core::bytestream::ZCursor;
  use zune_jpeg::zune_core::colorspace::ColorSpace;
  use zune_jpeg::zune_core::options::DecoderOptions;

  let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
  zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options)
}

#[cfg(feature = "jpeg")]
fn jpeg_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
  let mut decoder = jpeg_decoder(bytes);
  decoder.decode_headers().map_err(jpeg_error)?;
  let (width, height) = decoder
    .dimensions()
    .ok_or_else(|| Error::Decode("missing JPEG header".into()))?;
  Ok((width as u32, height as u32))
}

#[cfg(feature = "jpeg")]
fn decode_jpeg(bytes: &[u8]) -> Result<Image> {
  let (width, height) = jpeg_dimensions(bytes)?;
  let mut pixels = vec![0u32; pixel_count(width, height)?];
  // Straight into the pixels: `decode` would return a byte vector to copy.
  jpeg_decoder(bytes)
    .decode_into(bytemuck::cast_slice_mut(&mut pixels))
    .map_err(jpeg_error)?;
  Image::new(width, height, pixels)
}

#[cfg(not(feature = "jpeg"))]
fn jpeg_dimensions(_bytes: &[u8]) -> Result<(u32, u32)> {
  Err(Error::UnsupportedFormat)
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
  fn reads_dimensions_from_the_header() {
    let image = Image::from_rgba8(3, 2, &[7; 24]).unwrap();
    let mut png = Vec::new();
    {
      let mut encoder = png::Encoder::new(&mut png, 3, 2);
      encoder.set_color(png::ColorType::Rgba);
      encoder.set_depth(png::BitDepth::Eight);
      let mut writer = encoder.write_header().unwrap();
      writer.write_image_data(image.as_rgba8()).unwrap();
      writer.finish().unwrap();
    }
    assert_eq!(Image::dimensions(&png).unwrap(), (3, 2));
    assert!(matches!(
      Image::dimensions(b"GIF89a"),
      Err(Error::UnsupportedFormat)
    ));
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
