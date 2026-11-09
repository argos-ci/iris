//! Fingerprints are stored — ignore rules and flakiness statistics are keyed
//! by them — so the in-memory implementation has to return what
//! `@argos-ci/mask-fingerprint` returns for the same mask, always. These tests
//! hold it to a copy of that package's algorithm.

mod support;

use adiff::{fingerprint_png, Mask};
use support::reference_fingerprint::fingerprint as reference;
use support::{fixtures, Random};

fn mask_fixture(name: &str) -> Vec<u8> {
  std::fs::read(fixtures().join("masks").join(name)).unwrap()
}

#[track_caller]
fn assert_matches_reference(mask: &Mask, context: &str) {
  let png = mask.to_png().unwrap();
  assert_eq!(
    mask.fingerprint(),
    reference(&png).unwrap(),
    "{context} ({}x{}, {} pixels set)",
    mask.width(),
    mask.height(),
    mask.count()
  );
}

#[test]
fn returns_the_published_fingerprints() {
  // The values `@argos-ci/mask-fingerprint` is pinned to in the Argos backend.
  assert_eq!(
    fingerprint_png(&mask_fixture("no-end-1.png")).unwrap(),
    "v1:g16:d1:t0.002,0.02,0.08:ccb73aa8a5d0317b"
  );
  assert_eq!(
    fingerprint_png(&mask_fixture("no-end-2.png")).unwrap(),
    "v1:g16:d1:t0.002,0.02,0.08:badfb9bd5eea0beb"
  );
}

#[test]
fn groups_diffs_that_look_alike() {
  let fingerprint = |name| fingerprint_png(&mask_fixture(name)).unwrap();
  let a = fingerprint("diff-A1.png");
  assert_eq!(a, fingerprint("diff-A2.png"));
  assert_eq!(a, fingerprint("diff-A3.png"));
  assert_ne!(a, fingerprint("diff-B1.png"));
  assert_ne!(fingerprint("long-diff.png"), fingerprint("diff-B1.png"));
}

#[test]
fn matches_the_reference_on_every_fixture() {
  let mut checked = 0;
  for entry in std::fs::read_dir(fixtures().join("masks")).unwrap() {
    let path = entry.unwrap().path();
    if path.extension().is_none_or(|extension| extension != "png") {
      continue;
    }
    let png = std::fs::read(&path).unwrap();
    assert_eq!(
      fingerprint_png(&png).unwrap(),
      reference(&png).unwrap(),
      "{}",
      path.display()
    );
    checked += 1;
  }
  assert!(checked >= 9, "only {checked} fixtures found");
}

#[test]
fn matches_the_reference_on_every_small_mask() {
  // Every mask of every size up to 4x4: all the ways a change can sit against
  // the first row, the last two, and the left and right edges.
  for width in 1..=4u32 {
    for height in 1..=4u32 {
      let cells = width * height;
      for bits in 0..1u32 << cells {
        let mask = Mask::from_fn(width, height, |x, y| bits >> (y * width + x) & 1 == 1);
        assert_matches_reference(&mask, &format!("bits {bits:#b}"));
      }
    }
  }
}

#[test]
fn matches_the_reference_for_a_single_pixel_anywhere() {
  for (width, height) in [(1, 9), (9, 1), (2, 7), (7, 2), (3, 3), (11, 6), (33, 17)] {
    for y in 0..height {
      for x in 0..width {
        let mask = Mask::from_fn(width, height, |px, py| (px, py) == (x, y));
        assert_matches_reference(&mask, &format!("pixel ({x}, {y})"));
      }
    }
  }
}

#[test]
fn matches_the_reference_on_random_masks() {
  let mut random = Random::new(2024);
  for round in 0..3000 {
    let width = random.between(1, 140) as u32;
    let height = random.between(1, 140) as u32;
    let mut cells = vec![false; (width * height) as usize];
    let mut set = |x: u32, y: u32| cells[(y * width + x) as usize] = true;

    for _ in 0..random.between(1, 4) {
      match random.below(6) {
        // Scattered pixels, from a few to half of the image.
        0 => {
          let denominator = [2, 10, 100, 1000][random.below(4) as usize];
          for y in 0..height {
            for x in 0..width {
              if random.chance(1, denominator) {
                set(x, y);
              }
            }
          }
        }
        // A filled rectangle.
        1 => {
          let (x0, y0) = (
            random.below(width as u64) as u32,
            random.below(height as u64) as u32,
          );
          let x1 = random.between(x0 as u64, width as u64 - 1) as u32;
          let y1 = random.between(y0 as u64, height as u64 - 1) as u32;
          for y in y0..=y1 {
            for x in x0..=x1 {
              set(x, y);
            }
          }
        }
        // A full row, more often than not one the original treats specially.
        2 => {
          let special = [0, 1, height.saturating_sub(2), height - 1];
          let y = if random.chance(3, 4) {
            special[random.below(4) as usize].min(height - 1)
          } else {
            random.below(height as u64) as u32
          };
          for x in 0..width {
            set(x, y);
          }
        }
        // A full column, likewise.
        3 => {
          let x = if random.chance(1, 2) {
            [0, width - 1][random.below(2) as usize]
          } else {
            random.below(width as u64) as u32
          };
          for y in 0..height {
            set(x, y);
          }
        }
        // A diagonal stroke.
        4 => {
          let offset = random.below(width as u64) as u32;
          for y in 0..height {
            set((offset + y) % width, y);
          }
        }
        // One pixel.
        _ => set(
          random.below(width as u64) as u32,
          random.below(height as u64) as u32,
        ),
      }
    }

    let mask = Mask::from_fn(width, height, |x, y| cells[(y * width + x) as usize]);
    assert_matches_reference(&mask, &format!("round {round}"));
  }
}

#[test]
fn reads_legacy_rgba_masks() {
  // What odiff wrote: opaque red on transparent black, as RGBA.
  let mask = Mask::from_fn(40, 30, |x, y| (x + y) % 7 == 0);
  let rgba = support::image_from_fn(40, 30, |x, y| {
    if mask.is_set(x, y) {
      [255, 0, 0, 255]
    } else {
      [0, 0, 0, 0]
    }
  });
  let png = support::encode_rgba(&rgba);
  assert_eq!(Mask::from_png(&png).unwrap(), mask);
  assert_eq!(fingerprint_png(&png).unwrap(), mask.fingerprint());
  assert_eq!(fingerprint_png(&png).unwrap(), reference(&png).unwrap());
}
