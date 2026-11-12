//! Damaged input must be refused, never crash. The comparison runs inside the
//! server process, where a panic, an abort or a crash in a decoder takes down
//! every job it holds — something a separate odiff process never could.

mod support;

use std::panic::{catch_unwind, AssertUnwindSafe};

use iris::{compare_buffers, fingerprint_png, Image, Options};
use support::{encode_rgba, fixtures, image_from_fn, Random};

/// A copy of `bytes` damaged in one of the ways files get damaged: cut short,
/// bits flipped, a stretch zeroed or overwritten. The signature is kept most
/// of the time, so the decoder behind it is what gets exercised.
fn damage(bytes: &[u8], random: &mut Random) -> Vec<u8> {
  let mut damaged = bytes.to_vec();
  let keep = if random.chance(4, 5) {
    12.min(bytes.len())
  } else {
    0
  };
  let position = |random: &mut Random| random.between(keep as u64, bytes.len() as u64 - 1) as usize;
  match random.below(4) {
    0 => damaged.truncate(position(random)),
    1 => {
      for _ in 0..random.between(1, 8) {
        let at = position(random);
        damaged[at] ^= 1 << random.below(8);
      }
    }
    2 => {
      let start = position(random);
      let end = (start + random.between(1, 64) as usize).min(damaged.len());
      damaged[start..end].fill(0);
    }
    _ => {
      let start = position(random);
      let end = (start + random.between(1, 16) as usize).min(damaged.len());
      for byte in &mut damaged[start..end] {
        *byte = random.next() as u8;
      }
    }
  }
  damaged
}

fn inputs() -> Vec<(&'static str, Vec<u8>)> {
  let small = image_from_fn(61, 47, |x, y| {
    [
      (x * 4) as u8,
      (y * 5) as u8,
      90,
      if (x + y) % 9 == 0 { 120 } else { 255 },
    ]
  });
  let format = |name: &str| std::fs::read(fixtures().join("formats/boxShadow").join(name)).unwrap();
  vec![
    ("png", encode_rgba(&small)),
    ("jpeg", format("jpeg-baseline-base.jpg")),
    ("jpeg progressive", format("jpeg-progressive-base.jpg")),
    ("webp lossy", format("webp-lossy-base.webp")),
    ("webp lossless", format("webp-lossless-base.webp")),
  ]
}

#[test]
fn damaged_images_are_refused_without_crashing() {
  let options = Options {
    // The cap a server sets, so a damaged size field cannot ask for memory
    // the machine does not have.
    max_pixels: Some(50_000_000),
    ..Options::default()
  };
  let mut random = Random::new(99);

  for (format, original) in inputs() {
    let (mut refused, mut compared) = (0, 0);
    for round in 0..300 {
      let damaged = damage(&original, &mut random);
      let outcome = catch_unwind(AssertUnwindSafe(|| {
        let dimensions = Image::dimensions(&damaged).map(|_| ());
        let decoded = Image::decode(&damaged).map(|_| ());
        let compared = compare_buffers(&damaged, &original, &options).map(|_| ());
        (dimensions, decoded, compared)
      }));
      let Ok((_, decoded, result)) = outcome else {
        panic!("{format}: round {round} panicked");
      };
      // A file that decodes on its own also compares.
      if decoded.is_ok() {
        assert!(
          result.is_ok(),
          "{format}: round {round} decodes but does not compare"
        );
        compared += 1;
      } else {
        refused += 1;
      }
    }
    // Both paths are exercised: damage that stops the decoder, and damage
    // it reads through.
    assert!(refused > 0, "{format}: nothing was refused");
    assert!(compared + refused == 300, "{format}");
  }
}

#[test]
fn damaged_masks_are_refused_without_crashing() {
  let mask = iris::Mask::from_fn(97, 61, |x, y| (x * 3 + y * 7) % 11 < 3);
  let png = mask.to_png().unwrap();
  let mut random = Random::new(7);
  for round in 0..500 {
    let damaged = damage(&png, &mut random);
    let outcome = catch_unwind(AssertUnwindSafe(|| fingerprint_png(&damaged)));
    assert!(outcome.is_ok(), "round {round} panicked");
  }
}

#[test]
fn empty_and_tiny_inputs_are_refused() {
  let options = Options::default();
  let valid = encode_rgba(&image_from_fn(4, 4, |_, _| [1, 2, 3, 255]));
  for bytes in [
    &b""[..],
    &b"\x89PNG"[..],
    &b"\x89PNG\r\n\x1a\n"[..],
    &b"\xff\xd8\xff"[..],
    &b"\xff\xd8\xff\xe0\0\x10JFIF\0"[..],
    &b"RIFF\0\0\0\0WEBP"[..],
    &b"RIFF\xff\xff\xff\xffWEBPVP8 "[..],
  ] {
    assert!(Image::decode(bytes).is_err(), "{bytes:?}");
    assert!(Image::dimensions(bytes).is_err(), "{bytes:?}");
    assert!(
      compare_buffers(bytes, &valid, &options).is_err(),
      "{bytes:?}"
    );
  }
}
