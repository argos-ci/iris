use std::fmt;
use std::path::PathBuf;

/// Everything that can go wrong while comparing two images.
#[derive(Debug)]
pub enum Error {
  /// A file could not be read or written.
  Io {
    path: PathBuf,
    source: std::io::Error,
  },
  /// The bytes are neither a PNG nor a JPEG.
  UnsupportedFormat,
  /// The image is malformed.
  Decode(String),
  /// The mask could not be encoded.
  Encode(String),
  /// The comparison canvas exceeds [`Options::max_pixels`](crate::Options::max_pixels).
  TooLarge {
    width: u32,
    height: u32,
    max_pixels: u64,
  },
  /// The options are inconsistent.
  InvalidOptions(String),
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
      Error::UnsupportedFormat => write!(f, "unsupported image format, expected PNG or JPEG"),
      Error::Decode(message) => write!(f, "image decode error: {message}"),
      Error::Encode(message) => write!(f, "mask encode error: {message}"),
      Error::TooLarge {
        width,
        height,
        max_pixels,
      } => write!(
        f,
        "comparison canvas {width}x{height} exceeds the limit of {max_pixels} pixels"
      ),
      Error::InvalidOptions(message) => write!(f, "invalid options: {message}"),
    }
  }
}

impl std::error::Error for Error {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Error::Io { source, .. } => Some(source),
      _ => None,
    }
  }
}

pub type Result<T> = std::result::Result<T, Error>;
