#![deny(clippy::all)]

use std::panic::{catch_unwind, AssertUnwindSafe};

use iris::{
  compare_buffers, compare_files, fingerprint_png, Antialiasing, Comparison, Mask, Options, Rect,
  Tier,
};
use napi::bindgen_prelude::{AsyncTask, Buffer, Either};
use napi::{Env, Task};
use napi_derive::napi;
use sha2::{Digest, Sha256};

/* --------------------------------- options --------------------------------- */

/// One sensitivity level. Every tier is evaluated in the same scan.
#[napi(object, js_name = "Tier")]
pub struct JsTier {
  /// Colour distance a pixel must exceed to count: 0 is strict, 1 accepts
  /// everything.
  pub threshold: f64,
  /// Share of the image that must differ, between 0 and 1, for the tier to
  /// report anything. Defaults to 0.
  pub min_score: Option<f64>,
  /// Caps `minScore` at this many pixels.
  pub max_ignored_pixels: Option<f64>,
}

#[napi(object, js_name = "DiffOptions")]
pub struct JsDiffOptions {
  /// Sensitivity between 0 and 1: 0 is strict, 0.5 the default. Scales the
  /// two built-in tiers. Ignored when `tiers` is given.
  pub threshold: Option<f64>,
  /// Replace the built-in tiers.
  pub tiers: Option<Vec<JsTier>>,
  /// How rendering noise is told from change. `"classic"` (the default, also
  /// `true`) is the detector of pixelmatch and odiff: masks, and so
  /// fingerprints, are the ones Argos has always produced. `"subpixel"` asks
  /// whether both images can be the same edge or thin stroke rendered less
  /// than a pixel apart: it sees through noise the classic detector reports
  /// (text over a gradient, re-hinted fonts, compression artefacts) and
  /// reports changes it forgives (a recoloured label, a one pixel move).
  /// `false` counts every differing pixel.
  #[napi(ts_type = "boolean | 'classic' | 'subpixel'")]
  pub antialiasing: Option<Either<bool, String>>,
  /// Report scores as computed instead of rounding them to a hundredth of a
  /// percent first. Rounding is the default because it is what Argos has
  /// always done; it hides changes under 0.005% of the image.
  pub exact_score: Option<bool>,
  /// Compute the fingerprint of the mask. Defaults to true.
  pub fingerprint: Option<bool>,
  /// Look for content that moved rather than changed. Defaults to true.
  pub shifts: Option<bool>,
  /// Fewest consecutive rows that must move together to count as a shift.
  /// Defaults to 4.
  pub min_shift_rows: Option<u32>,
  /// Refuse to compare when the larger width times the larger height exceeds
  /// this many pixels.
  pub max_pixels: Option<f64>,
  /// Threads the comparison may use on top of the one decoding the second
  /// image. Defaults to up to four for images over a megapixel.
  pub threads: Option<u32>,
  /// Where to write the mask, as a PNG, when the images differ. Without it
  /// the PNG is returned in `mask.data`.
  pub output: Option<String>,
}

fn build_options(js: Option<JsDiffOptions>) -> napi::Result<(Options, Option<String>)> {
  let Some(js) = js else {
    return Ok((Options::default(), None));
  };

  let mut options = Options::sensitivity(js.threshold.unwrap_or(0.5));
  if let Some(tiers) = js.tiers {
    options.tiers = tiers
      .into_iter()
      .map(|tier| Tier {
        threshold: tier.threshold,
        min_score: tier.min_score.unwrap_or(0.0),
        max_ignored_pixels: tier.max_ignored_pixels,
      })
      .collect();
  }
  if let Some(antialiasing) = js.antialiasing {
    options.antialiasing = match antialiasing {
      Either::A(false) => Antialiasing::Off,
      Either::A(true) => Antialiasing::Classic,
      Either::B(name) => match name.as_str() {
        "classic" => Antialiasing::Classic,
        "subpixel" => Antialiasing::Subpixel,
        other => {
          return Err(napi::Error::from_reason(format!(
          "invalid options: unknown antialiasing \"{other}\", expected \"classic\" or \"subpixel\""
        )))
        }
      },
    };
  }
  if let Some(threads) = js.threads {
    options.threads = threads as usize;
  }
  if js.exact_score == Some(true) {
    options.percent_decimals = None;
  }
  if let Some(fingerprint) = js.fingerprint {
    options.fingerprint = fingerprint;
  }
  if let Some(shifts) = js.shifts {
    options.shifts = shifts;
  }
  if let Some(min_shift_rows) = js.min_shift_rows {
    options.min_shift_rows = min_shift_rows;
  }
  if let Some(max_pixels) = js.max_pixels {
    options.max_pixels = Some(max_pixels.max(0.0) as u64);
  }
  Ok((options, js.output))
}

/* ---------------------------------- result --------------------------------- */

#[napi(object, js_name = "Rect")]
pub struct JsRect {
  pub x: u32,
  pub y: u32,
  pub width: u32,
  pub height: u32,
}

impl From<Rect> for JsRect {
  fn from(rect: Rect) -> Self {
    Self {
      x: rect.x,
      y: rect.y,
      width: rect.width,
      height: rect.height,
    }
  }
}

#[napi(object, js_name = "TierResult")]
pub struct JsTierResult {
  pub threshold: f64,
  /// Pixels over the tier's colour threshold, anti-aliasing excluded.
  pub diff_pixels: f64,
  /// `diffPixels` as a share of the image.
  pub score: f64,
  /// Whether the score reaches the tier's floor.
  pub reported: bool,
}

#[napi(object, js_name = "DiffMask")]
pub struct JsMask {
  /// Where the PNG was written, when `output` was given.
  pub path: Option<String>,
  /// The PNG itself, when it was not.
  pub data: Option<Buffer>,
  /// SHA-256 of the PNG, in hexadecimal: its content address.
  pub sha256: String,
  /// Size of the PNG in bytes.
  pub byte_length: u32,
  /// Smallest rectangle holding every differing pixel.
  pub bounds: JsRect,
}

/// A block of rows found at another height in the head image.
#[napi(object, js_name = "ShiftSegment")]
pub struct JsShiftSegment {
  pub x: u32,
  pub width: u32,
  pub base_y: u32,
  pub head_y: u32,
  pub height: u32,
  /// `headY - baseY`: positive when the block moved down.
  pub dy: i32,
}

#[napi(object, js_name = "Shift")]
pub struct JsShift {
  /// Blocks that moved, top to bottom.
  pub segments: Vec<JsShiftSegment>,
  /// Rows of the head image with no counterpart in the base image.
  pub inserted: Vec<JsRect>,
  /// Rows of the base image with no counterpart in the head image.
  pub removed: Vec<JsRect>,
  /// Pixels of the mask that differ only because content moved.
  pub shifted_pixels: f64,
}

#[napi(object, js_name = "DiffResult")]
pub struct JsDiffResult {
  /// Share of the image that differs, 0 when the images are the same.
  pub score: f64,
  /// Size of the canvas the images were compared on, and of the mask: the
  /// larger width and the larger height.
  pub width: u32,
  pub height: u32,
  /// Number of pixels in the mask.
  pub diff_pixels: f64,
  /// Differing pixels left out for being anti-aliasing.
  pub antialiased_pixels: f64,
  /// Index of the tier the result comes from.
  pub tier: Option<u32>,
  /// What each tier saw, in the order they were given.
  pub tiers: Vec<JsTierResult>,
  /// Stable identifier of the change, equal for diffs that look alike.
  pub fingerprint: Option<String>,
  /// The pixels that differ, when the images differ.
  pub mask: Option<JsMask>,
  /// Content that moved rather than changed, when there is any.
  pub shift: Option<JsShift>,
}

/// A comparison with its mask already encoded, ready to cross into JavaScript.
pub struct Outcome {
  comparison: Comparison,
  png: Option<Vec<u8>>,
  sha256: Option<String>,
  output: Option<String>,
}

fn into_js(outcome: Outcome) -> JsDiffResult {
  let Outcome {
    comparison,
    png,
    sha256,
    output,
  } = outcome;

  let bounds = comparison.mask.as_ref().and_then(Mask::bounds);
  let mask = match (png, sha256, bounds) {
    (Some(png), Some(sha256), Some(bounds)) => Some(JsMask {
      byte_length: png.len() as u32,
      data: output.is_none().then(|| png.into()),
      path: output,
      sha256,
      bounds: bounds.into(),
    }),
    _ => None,
  };

  JsDiffResult {
    score: comparison.score,
    width: comparison.width,
    height: comparison.height,
    diff_pixels: comparison.diff_pixels as f64,
    antialiased_pixels: comparison.antialiased_pixels as f64,
    tier: comparison.tier.map(|tier| tier as u32),
    tiers: comparison
      .tiers
      .iter()
      .map(|tier| JsTierResult {
        threshold: tier.threshold,
        diff_pixels: tier.diff_pixels as f64,
        score: tier.score,
        reported: tier.reported,
      })
      .collect(),
    fingerprint: comparison.fingerprint,
    mask,
    shift: comparison.shift.map(|shift| JsShift {
      segments: shift
        .segments
        .iter()
        .map(|segment| JsShiftSegment {
          x: segment.x,
          width: segment.width,
          base_y: segment.base_y,
          head_y: segment.head_y,
          height: segment.height,
          dy: segment.dy() as i32,
        })
        .collect(),
      inserted: shift.inserted.into_iter().map(JsRect::from).collect(),
      removed: shift.removed.into_iter().map(JsRect::from).collect(),
      shifted_pixels: shift.shifted_pixels as f64,
    }),
  }
}

/* ----------------------------------- tasks ---------------------------------- */

fn to_napi(error: iris::Error) -> napi::Error {
  napi::Error::from_reason(error.to_string())
}

/// Run `work`, turning a panic into an exception instead of taking the
/// process down with it.
fn guarded<T>(work: impl FnOnce() -> napi::Result<T>) -> napi::Result<T> {
  catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|panic| {
    let message = panic
      .downcast_ref::<&str>()
      .map(|message| message.to_string())
      .or_else(|| panic.downcast_ref::<String>().cloned())
      .unwrap_or_else(|| "unknown error".to_string());
    Err(napi::Error::from_reason(format!(
      "iris panicked: {message}"
    )))
  })
}

pub enum Input {
  Path(String),
  Bytes(Buffer),
}

impl From<Either<String, Buffer>> for Input {
  fn from(input: Either<String, Buffer>) -> Self {
    match input {
      Either::A(path) => Input::Path(path),
      Either::B(bytes) => Input::Bytes(bytes),
    }
  }
}

impl Input {
  fn read(&self) -> napi::Result<std::borrow::Cow<'_, [u8]>> {
    match self {
      Input::Bytes(bytes) => Ok(std::borrow::Cow::Borrowed(&bytes[..])),
      Input::Path(path) => std::fs::read(path)
        .map(std::borrow::Cow::Owned)
        .map_err(|error| napi::Error::from_reason(format!("{path}: {error}"))),
    }
  }
}

pub struct DiffTask {
  base: Input,
  head: Input,
  options: Option<JsDiffOptions>,
}

impl Task for DiffTask {
  type Output = Outcome;
  type JsValue = JsDiffResult;

  fn compute(&mut self) -> napi::Result<Self::Output> {
    let (options, output) = build_options(self.options.take())?;
    guarded(|| {
      let comparison = match (&self.base, &self.head) {
        (Input::Path(base), Input::Path(head)) => compare_files(base, head, &options),
        (base, head) => compare_buffers(&base.read()?, &head.read()?, &options),
      }
      .map_err(to_napi)?;

      let png = comparison
        .mask
        .as_ref()
        .map(Mask::to_png)
        .transpose()
        .map_err(to_napi)?;
      let sha256 = png.as_ref().map(|png| hex(&Sha256::digest(png)));
      if let (Some(png), Some(path)) = (&png, &output) {
        std::fs::write(path, png)
          .map_err(|error| napi::Error::from_reason(format!("{path}: {error}")))?;
      }

      Ok(Outcome {
        comparison,
        png,
        sha256,
        output,
      })
    })
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> napi::Result<Self::JsValue> {
    Ok(into_js(output))
  }
}

fn hex(bytes: &[u8]) -> String {
  use std::fmt::Write;
  bytes
    .iter()
    .fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
      let _ = write!(hex, "{byte:02x}");
      hex
    })
}

/// Compare two images, given as file paths or as encoded buffers (PNG or
/// JPEG), in one pass: score, mask, fingerprint and layout shifts.
#[napi(ts_return_type = "Promise<DiffResult>")]
pub fn diff(
  base: Either<String, Buffer>,
  head: Either<String, Buffer>,
  options: Option<JsDiffOptions>,
) -> AsyncTask<DiffTask> {
  AsyncTask::new(DiffTask {
    base: base.into(),
    head: head.into(),
    options,
  })
}

pub struct FingerprintTask {
  input: Input,
}

impl Task for FingerprintTask {
  type Output = String;
  type JsValue = String;

  fn compute(&mut self) -> napi::Result<Self::Output> {
    guarded(|| fingerprint_png(&self.input.read()?).map_err(to_napi))
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> napi::Result<Self::JsValue> {
    Ok(output)
  }
}

/// Fingerprint of a diff mask stored as a PNG: one written by `diff`, or a
/// legacy mask with opaque red pixels on a transparent background. Returns
/// the same value as `@argos-ci/mask-fingerprint`.
#[napi(ts_return_type = "Promise<string>")]
pub fn fingerprint_diff(png: Either<String, Buffer>) -> AsyncTask<FingerprintTask> {
  AsyncTask::new(FingerprintTask { input: png.into() })
}
