use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek};
use std::path::Path;

use crate::error::{Error, Result};

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
const JPEG_SIGNATURE: [u8; 3] = [0xff, 0xd8, 0xff];

/// The inverse DCT a JPEG is decoded with. The two differ in the last bit of
/// many pixels, which is enough to move a pixel across a threshold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JpegDct {
  /// libjpeg-turbo's fast integer IDCT, what odiff decoded with unless the
  /// threshold was 0.
  #[default]
  Fast,
  /// The accurate integer IDCT, what odiff decoded with at threshold 0.
  Accurate,
}

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

  /// Decode a PNG, a JPEG or a WebP, told apart by their signature.
  pub fn decode(bytes: &[u8]) -> Result<Self> {
    Self::decode_with(bytes, JpegDct::default())
  }

  /// [`Image::decode`], choosing how a JPEG is decoded.
  pub fn decode_with(bytes: &[u8], dct: JpegDct) -> Result<Self> {
    match Format::of(bytes)? {
      Format::Png => decode_png(Cursor::new(bytes)),
      Format::Jpeg => decode_jpeg(bytes, dct),
      Format::Webp => decode_webp(bytes),
    }
  }

  /// Read and decode a file. A PNG is decoded as it is read, without holding
  /// the file in memory.
  pub fn open(path: impl AsRef<Path>) -> Result<Self> {
    Self::open_with(path, JpegDct::default())
  }

  /// [`Image::open`], choosing how a JPEG is decoded.
  pub fn open_with(path: impl AsRef<Path>, dct: JpegDct) -> Result<Self> {
    let path = path.as_ref();
    let io_error = |source| Error::Io {
      path: path.to_path_buf(),
      source,
    };
    let mut reader = BufReader::new(File::open(path).map_err(io_error)?);
    match Format::of(reader.fill_buf().map_err(io_error)?)? {
      Format::Png => decode_png(reader),
      format => {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map_err(io_error)?;
        match format {
          Format::Jpeg => decode_jpeg(&bytes, dct),
          _ => decode_webp(&bytes),
        }
      }
    }
  }

  /// Width and height of an encoded image, read from its header alone.
  pub fn dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    match Format::of(bytes)? {
      Format::Png => png_dimensions(Cursor::new(bytes)),
      Format::Jpeg => jpeg_dimensions(bytes),
      Format::Webp => webp_dimensions(bytes),
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
      format => {
        // A JPEG header can sit after any number of metadata segments, and
        // libwebp wants the whole file.
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map_err(io_error)?;
        match format {
          Format::Jpeg => jpeg_dimensions(&bytes),
          _ => webp_dimensions(&bytes),
        }
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
  Webp,
}

impl Format {
  fn of(bytes: &[u8]) -> Result<Self> {
    if bytes.starts_with(&PNG_SIGNATURE) {
      Ok(Format::Png)
    } else if bytes.starts_with(&JPEG_SIGNATURE) {
      Ok(Format::Jpeg)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
      Ok(Format::Webp)
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

/// A TurboJPEG decompressor, destroyed when dropped.
struct Turbo(turbojpeg_sys::tjhandle);

impl Turbo {
  fn new() -> Result<Self> {
    // SAFETY: no arguments; a null handle is the failure case, checked here.
    let handle = unsafe { turbojpeg_sys::tjInitDecompress() };
    if handle.is_null() {
      return Err(Error::Decode("cannot start the JPEG decoder".into()));
    }
    Ok(Self(handle))
  }

  fn error(&self) -> Error {
    // SAFETY: the handle is live; TurboJPEG returns a static or
    // handle-owned string, copied out before the handle can go.
    let message = unsafe { std::ffi::CStr::from_ptr(turbojpeg_sys::tjGetErrorStr2(self.0)) };
    Error::Decode(message.to_string_lossy().into_owned())
  }

  fn length(bytes: &[u8]) -> Result<std::ffi::c_ulong> {
    std::ffi::c_ulong::try_from(bytes.len())
      .map_err(|_| Error::Decode("JPEG data does not fit the decoder".into()))
  }

  fn dimensions(&self, bytes: &[u8]) -> Result<(u32, u32)> {
    let (mut width, mut height) = (0, 0);
    // SAFETY: `bytes` outlives the call and its length is passed with it;
    // the outputs point at live locals. The same call odiff makes.
    let status = unsafe {
      turbojpeg_sys::tjDecompressHeader(
        self.0,
        bytes.as_ptr().cast_mut(),
        Self::length(bytes)?,
        &mut width,
        &mut height,
      )
    };
    if status != 0 || width <= 0 || height <= 0 {
      return Err(self.error());
    }
    Ok((width as u32, height as u32))
  }

  /// Decompress into RGBA, with odiff's arguments: the header's size, the
  /// default pitch, and the DCT flag alone.
  fn decompress(
    &self,
    bytes: &[u8],
    pixels: &mut [u32],
    (width, height): (u32, u32),
    dct: JpegDct,
  ) -> Result<()> {
    let flags = match dct {
      JpegDct::Fast => turbojpeg_sys::TJFLAG_FASTDCT,
      JpegDct::Accurate => turbojpeg_sys::TJFLAG_ACCURATEDCT,
    };
    debug_assert_eq!(pixels.len(), width as usize * height as usize);
    // SAFETY: `pixels` holds width * height RGBA pixels, which is what
    // TJPF_RGBA with a 0 pitch writes for the dimensions read from this
    // very header.
    let status = unsafe {
      turbojpeg_sys::tjDecompress2(
        self.0,
        bytes.as_ptr(),
        Self::length(bytes)?,
        pixels.as_mut_ptr().cast(),
        width as std::ffi::c_int,
        0,
        height as std::ffi::c_int,
        turbojpeg_sys::TJPF_TJPF_RGBA,
        flags as std::ffi::c_int,
      )
    };
    // A warning is a failure too, as it was for odiff.
    if status != 0 {
      return Err(self.error());
    }
    Ok(())
  }
}

impl Drop for Turbo {
  fn drop(&mut self) {
    // SAFETY: the handle came from tjInitDecompress and is dropped once.
    unsafe { turbojpeg_sys::tjDestroy(self.0) };
  }
}

fn jpeg_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
  Turbo::new()?.dimensions(bytes)
}

fn decode_jpeg(bytes: &[u8], dct: JpegDct) -> Result<Image> {
  let decoder = Turbo::new()?;
  let (width, height) = decoder.dimensions(bytes)?;
  let mut pixels = vec![0u32; pixel_count(width, height)?];
  decoder.decompress(bytes, &mut pixels, (width, height), dct)?;
  Image::new(width, height, pixels)
}

fn webp_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
  let (mut width, mut height) = (0, 0);
  // SAFETY: `bytes` outlives the call and its length is passed with it.
  let ok =
    unsafe { libwebp_sys::WebPGetInfo(bytes.as_ptr(), bytes.len(), &mut width, &mut height) };
  if ok == 0 || width <= 0 || height <= 0 {
    return Err(Error::Decode("invalid WebP header".into()));
  }
  Ok((width as u32, height as u32))
}

fn decode_webp(bytes: &[u8]) -> Result<Image> {
  let (width, height) = webp_dimensions(bytes)?;
  let mut pixels = vec![0u32; pixel_count(width, height)?];
  let output: &mut [u8] = bytemuck::cast_slice_mut(&mut pixels);
  // SAFETY: `output` holds width * height RGBA pixels at a stride of
  // width * 4, the size and stride passed with it. The same call odiff makes.
  let written = unsafe {
    libwebp_sys::WebPDecodeRGBAInto(
      bytes.as_ptr(),
      bytes.len(),
      output.as_mut_ptr(),
      output.len(),
      (width * 4) as std::ffi::c_int,
    )
  };
  if written.is_null() {
    return Err(Error::Decode("invalid WebP data".into()));
  }
  Image::new(width, height, pixels)
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
