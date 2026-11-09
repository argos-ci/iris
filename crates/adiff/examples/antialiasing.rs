//! Compare the anti-aliasing detectors on the screenshot fixtures: how many
//! differing pixels each one leaves, at both of the default tiers.
//!
//!   cargo run --release -p adiff --example antialiasing
use std::path::PathBuf;

use adiff::{compare_files, Antialiasing, Options};

fn main() {
  let pairs = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/pairs");
  let mut names: Vec<_> = std::fs::read_dir(&pairs)
    .unwrap()
    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
    .collect();
  names.sort();

  println!(
    "{:<22} {:>10} {:>10} {:>10}   {:>10} {:>10} {:>10}",
    "fixture", "off", "classic", "subpixel", "off", "classic", "subpixel"
  );
  println!(
    "{:<22} {:^32}   {:^32}",
    "", "tolerant tier (0.15)", "sensitive tier (0.0225)"
  );
  for name in names {
    if name == "different-dimensions" {
      continue;
    }
    let directory = pairs.join(&name);
    let run = |antialiasing| {
      let options = Options {
        antialiasing,
        shifts: false,
        fingerprint: false,
        ..Options::default()
      };
      let result = compare_files(
        directory.join("base.png"),
        directory.join("compare.png"),
        &options,
      )
      .unwrap();
      (result.tiers[0].diff_pixels, result.tiers[1].diff_pixels)
    };
    let (off, classic, subpixel) = (
      run(Antialiasing::Off),
      run(Antialiasing::Classic),
      run(Antialiasing::Subpixel),
    );
    println!(
      "{:<22} {:>10} {:>10} {:>10}   {:>10} {:>10} {:>10}",
      name, off.0, classic.0, subpixel.0, off.1, classic.1, subpixel.1
    );
  }
}
