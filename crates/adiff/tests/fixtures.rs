//! The screenshot pairs the Argos backend is tested with, and the verdict it
//! expects for each. These are real cases: font rendering that differs
//! between two runs, JPEG-like compression noise, shadows, and changes small
//! enough to be mistaken for either.

mod support;

use adiff::{compare_files, Comparison, Options};
use support::fixtures;

/// Name, sensitivity, and whether the two images differ.
const CASES: [(&str, f64, bool); 19] = [
  ("alphaBackground", 0.5, false),
  ("alphaSmallGraph", 0.2, true),
  ("big-images", 0.5, true),
  ("border", 0.5, false),
  ("boxShadow", 0.5, false),
  ("fontAliasing", 0.5, false),
  ("imageCompression1", 0.5, false),
  ("imageCompression2", 0.5, false),
  ("imageCompression3", 0.5, false),
  ("imageCompression4", 0.5, false),
  ("imageCompression5", 0.5, false),
  ("imageCompression6", 0.5, false),
  ("imageCompression7", 0.5, false),
  ("simple", 0.5, true),
  ("tableAlpha", 0.5, true),
  ("minorChange1", 0.5, true),
  ("minorChange2", 0.5, false),
  ("minorChange3", 0.0, true),
  ("different-dimensions", 0.0, true),
];

fn run(name: &str, options: &Options) -> Comparison {
  let directory = fixtures().join("pairs").join(name);
  compare_files(
    directory.join("base.png"),
    directory.join("compare.png"),
    options,
  )
  .unwrap()
}

#[test]
fn reaches_the_expected_verdict() {
  for (name, sensitivity, differs) in CASES {
    let result = run(name, &Options::sensitivity(sensitivity));
    assert_eq!(
      result.score > 0.0,
      differs,
      "{name}: score {}",
      result.score
    );
    assert_eq!(result.mask.is_some(), differs, "{name}");
    assert_eq!(result.fingerprint.is_some(), differs, "{name}");
    if let Some(mask) = &result.mask {
      assert_eq!(
        (mask.width(), mask.height()),
        (result.width, result.height),
        "{name}"
      );
      assert_eq!(mask.count(), result.diff_pixels, "{name}");
    }
  }
}

#[test]
fn exact_scores_report_what_the_rounding_hides() {
  // 42 pixels of a 1920x1080 screenshot differ: 0.002%, which the `odiff`
  // command line prints as 0.00. The backend expects "no change" here, and
  // gets it from that rounding rather than from the 20 pixel cap.
  let rounded = run("minorChange2", &Options::default());
  assert_eq!(rounded.tiers[0].diff_pixels, 42);
  assert_eq!((rounded.score, rounded.tier), (0.0, None));

  let exact = run(
    "minorChange2",
    &Options {
      percent_decimals: None,
      ..Options::default()
    },
  );
  assert_eq!(exact.tiers[0].diff_pixels, 42);
  assert_eq!(exact.score, 42.0 / (1920.0 * 1080.0));
  assert_eq!(exact.tier, Some(0));

  // Every other verdict is the same either way.
  for (name, sensitivity, differs) in CASES {
    if name == "minorChange2" {
      continue;
    }
    let options = Options {
      percent_decimals: None,
      ..Options::sensitivity(sensitivity)
    };
    let result = run(name, &options);
    assert_eq!(
      result.score > 0.0,
      differs,
      "{name}: score {}",
      result.score
    );
  }
}

#[test]
fn is_deterministic() {
  let options = Options::default();
  let first = run("minorChange1", &options);
  let second = run("minorChange1", &options);
  assert_eq!(first, second);
  assert_eq!(
    first.mask.unwrap().to_png().unwrap(),
    second.mask.unwrap().to_png().unwrap()
  );
}

#[test]
fn the_mask_round_trips_through_png() {
  let result = run("tableAlpha", &Options::default());
  let mask = result.mask.unwrap();
  let png = mask.to_png().unwrap();
  let decoded = adiff::Mask::from_png(&png).unwrap();
  assert_eq!(decoded, mask);
  assert_eq!(decoded.fingerprint(), result.fingerprint.unwrap());
}
