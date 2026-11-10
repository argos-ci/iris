//! Screenshot comparison in a single pass.
//!
//! Two images go in; out come a score, the mask of the pixels that differ, a
//! fingerprint that is stable across diffs that look alike, and a report of
//! the content that moved rather than changed. Every sensitivity tier is
//! evaluated in the same scan of the pixels, anti-aliasing is detected once,
//! and the fingerprint is computed from the mask in memory — nothing is
//! decoded twice and no intermediate image is written.
//!
//! ```no_run
//! use iris::{compare_files, Options};
//!
//! let result = compare_files("base.png", "head.png", &Options::default())?;
//! if let Some(mask) = &result.mask {
//!   std::fs::write("diff.png", mask.to_png()?)?;
//!   println!("{:.2}% differs, {:?}", result.score * 100.0, result.fingerprint);
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod antialias;
mod compare;
mod delta;
mod error;
mod fingerprint;
mod image;
mod mask;
mod options;
mod shift;
mod subpixel;
mod view;

use std::path::Path;

pub use compare::{compare, Comparison, TierResult};
pub use error::{Error, Result};
pub use fingerprint::EMPTY_FINGERPRINT;
pub use image::Image;
pub use mask::{Mask, Rect};
pub use options::{Antialiasing, Options, Tier, MAX_TIERS};
pub use shift::{ShiftReport, ShiftSegment};

/// Run two jobs at once, the second on the calling thread.
fn join<A: Send, B: Send>(first: impl FnOnce() -> A + Send, second: impl FnOnce() -> B) -> (A, B) {
  std::thread::scope(|scope| {
    let handle = scope.spawn(first);
    let second = second();
    let first = handle
      .join()
      .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    (first, second)
  })
}

/// Decode two encoded images, on two threads, and compare them. Decoding is
/// most of the work, so this is close to twice as fast as doing it in turn.
pub fn compare_buffers(base: &[u8], head: &[u8], options: &Options) -> Result<Comparison> {
  if options.max_pixels.is_some() {
    compare::check_canvas(options, Image::dimensions(base)?, Image::dimensions(head)?)?;
  }
  let (base, head) = join(|| Image::decode(base), || Image::decode(head));
  compare(&base?, &head?, options)
}

/// Read, decode and compare two image files.
pub fn compare_files(
  base: impl AsRef<Path>,
  head: impl AsRef<Path>,
  options: &Options,
) -> Result<Comparison> {
  let (base, head) = (base.as_ref(), head.as_ref());
  if options.max_pixels.is_some() {
    compare::check_canvas(
      options,
      Image::file_dimensions(base)?,
      Image::file_dimensions(head)?,
    )?;
  }
  let (base, head) = join(|| Image::open(base), || Image::open(head));
  compare(&base?, &head?, options)
}

/// Fingerprint of a diff mask stored as a PNG. See [`Mask::from_png`].
pub fn fingerprint_png(bytes: &[u8]) -> Result<String> {
  Ok(Mask::from_png(bytes)?.fingerprint())
}
