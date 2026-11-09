#![allow(dead_code)]

use std::path::PathBuf;

use iris::Image;

pub mod reference_fingerprint;

/// The repository's `fixtures` directory.
pub fn fixtures() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// A small deterministic generator: the tests must fail the same way twice.
pub struct Random(u64);

impl Random {
  pub fn new(seed: u64) -> Self {
    Self(seed.wrapping_mul(0x9e3779b97f4a7c15) | 1)
  }

  pub fn next(&mut self) -> u64 {
    self.0 ^= self.0 << 13;
    self.0 ^= self.0 >> 7;
    self.0 ^= self.0 << 17;
    self.0
  }

  /// A value in `0..bound`.
  pub fn below(&mut self, bound: u64) -> u64 {
    (self.next() >> 11) % bound
  }

  /// A value in `low..=high`.
  pub fn between(&mut self, low: u64, high: u64) -> u64 {
    low + self.below(high - low + 1)
  }

  /// True with probability `numerator / denominator`.
  pub fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
    self.below(denominator) < numerator
  }
}

/// Encode an image as an RGBA PNG.
pub fn encode_rgba(image: &Image) -> Vec<u8> {
  let mut output = Vec::new();
  let mut encoder = png::Encoder::new(&mut output, image.width(), image.height());
  encoder.set_color(png::ColorType::Rgba);
  encoder.set_depth(png::BitDepth::Eight);
  encoder.set_compression(png::Compression::Fastest);
  let mut writer = encoder.write_header().unwrap();
  writer.write_image_data(image.as_rgba8()).unwrap();
  writer.finish().unwrap();
  output
}

/// Build an image from a function of the coordinates.
pub fn image_from_fn(width: u32, height: u32, mut pixel: impl FnMut(u32, u32) -> [u8; 4]) -> Image {
  let mut rgba = Vec::with_capacity((width * height * 4) as usize);
  for y in 0..height {
    for x in 0..width {
      rgba.extend_from_slice(&pixel(x, y));
    }
  }
  Image::from_rgba8(width, height, &rgba).unwrap()
}
