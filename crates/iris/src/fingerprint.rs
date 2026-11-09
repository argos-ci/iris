//! A fingerprint that is equal for diffs that look alike.
//!
//! The mask is dilated by one pixel, cropped to its bounding box, centred in a
//! square, and reduced to a 16x16 grid whose cells are quantised by how full
//! they are. The grid is then hashed. Position and scale drop out, so the same
//! change reported at another offset or zoom level keeps its fingerprint.
//!
//! This is format `v1` of `@argos-ci/mask-fingerprint`, and fingerprints are
//! stored: ignore rules and flakiness statistics are keyed by them. The output
//! therefore has to be identical bit for bit, including where the original
//! streaming implementation departs from a textbook dilation:
//!
//! - row 0 is never emitted on its own, only as the neighbour of row 1;
//! - the row before last is emitted twice, the second time without the row
//!   above it;
//! - the last row is emitted without the row above it.
//!
//! `emissions` encodes exactly that, and the test suite checks the result
//! against a copy of the original algorithm on thousands of random masks.

use crate::mask::Mask;

const GRID: usize = 16;
const DENSITY_STEPS: [f32; 3] = [0.002, 0.02, 0.08];
const PREFIX: &str = "v1:g16:d1:t0.002,0.02,0.08";

/// Fingerprint of a mask without any differing pixel.
pub const EMPTY_FINGERPRINT: &str = "empty";

/// One row of the dilated mask as the original implementation produced it:
/// `label` is the row it is attributed to, `rows` the mask rows merged into it.
struct Emission {
  label: usize,
  rows: (usize, usize),
}

/// Every emission that can touch the mask rows `top..=bottom`.
fn emissions(height: usize, top: usize, bottom: usize) -> Vec<Emission> {
  let mut list = Vec::with_capacity(bottom - top + 5);
  if height >= 3 {
    let first = top.saturating_sub(1).max(1);
    let last = (bottom + 1).min(height - 2);
    for label in first..=last {
      list.push(Emission {
        label,
        rows: (label - 1, label + 1),
      });
    }
  }
  if height >= 2 && bottom >= height - 2 {
    list.push(Emission {
      label: height - 2,
      rows: (height - 2, height - 1),
    });
  }
  if bottom == height - 1 {
    list.push(Emission {
      label: height - 1,
      rows: (height - 1, height - 1),
    });
  }
  list
}

impl Mask {
  /// The fingerprint of this mask, or [`EMPTY_FINGERPRINT`].
  pub fn fingerprint(&self) -> String {
    let Some(tight) = self.bounds() else {
      return EMPTY_FINGERPRINT.to_string();
    };
    let width = self.width() as usize;
    let height = self.height() as usize;
    let (left, right) = (tight.x as usize, (tight.x + tight.width - 1) as usize);
    let (top, bottom) = (tight.y as usize, (tight.y + tight.height - 1) as usize);

    let row_is_set: Vec<bool> = (top..=bottom)
      .map(|y| self.row(y).iter().any(|&byte| byte != 0))
      .collect();
    let touches_mask = |emission: &Emission| {
      let (first, last) = (emission.rows.0.max(top), emission.rows.1.min(bottom));
      (first..=last).any(|y| row_is_set[y - top])
    };
    let emissions: Vec<Emission> = emissions(height, top, bottom)
      .into_iter()
      .filter(touches_mask)
      .collect();

    // Bounding box of the dilated mask. Every mask row is merged into at least
    // one emission, so horizontally it is the tight box grown by one pixel.
    let box_left = left.saturating_sub(1);
    let box_right = (right + 1).min(width - 1);
    let box_top = emissions.iter().map(|e| e.label).min().unwrap_or(top);
    let box_bottom = emissions.iter().map(|e| e.label).max().unwrap_or(bottom);
    let box_width = box_right - box_left + 1;
    let box_height = box_bottom - box_top + 1;

    let side = box_width.max(box_height);
    let offset_x = (side - box_width) / 2;
    let offset_y = (side - box_height) / 2;
    let cell_of = |position: usize| (position * GRID / side).min(GRID - 1);

    // Rows are handled 64 columns at a time. Bit `n` of `merged` is column
    // `origin + n`; the mask packs its rows the other way round, most
    // significant bit first, so each byte is reversed on the way in.
    let first_byte = box_left / 8;
    let origin = first_byte * 8;
    let words = (box_right + 1 - origin).div_ceil(64);
    let mut merged = vec![0u64; words];
    let mut dilated = vec![0u64; words];

    // Columns of the box where each grid column starts, as bits of `dilated`.
    let mut cell_starts = [box_width; GRID + 1];
    for index in (0..box_width).rev() {
      cell_starts[cell_of(index + offset_x)] = index;
    }
    for cell in (0..GRID).rev() {
      cell_starts[cell] = cell_starts[cell].min(cell_starts[cell + 1]);
    }
    let cell_bits = cell_starts.map(|start| start + box_left - origin);

    let mut counts = [0u32; GRID * GRID];
    for emission in &emissions {
      merged.fill(0);
      let (first, last) = (emission.rows.0.max(top), emission.rows.1.min(bottom));
      for y in (first..=last).filter(|&y| row_is_set[y - top]) {
        let row = &self.row(y)[first_byte..];
        for (word, bytes) in merged.iter_mut().zip(row.chunks(8)) {
          let mut loaded = [0u8; 8];
          for (slot, byte) in loaded.iter_mut().zip(bytes) {
            *slot = byte.reverse_bits();
          }
          *word |= u64::from_le_bytes(loaded);
        }
      }

      // One pixel to the left and to the right, across word boundaries.
      // What spills outside the box is never counted.
      for index in 0..words {
        let word = merged[index];
        let from_previous = if index > 0 {
          merged[index - 1] >> 63
        } else {
          0
        };
        let from_next = if index + 1 < words {
          merged[index + 1] << 63
        } else {
          0
        };
        dilated[index] = word | word << 1 | word >> 1 | from_previous | from_next;
      }

      let grid_row = cell_of(emission.label - box_top + offset_y) * GRID;
      for cell in 0..GRID {
        counts[grid_row + cell] += count_bits(&dilated, cell_bits[cell], cell_bits[cell + 1]);
      }
    }

    format!("{PREFIX}:{:016x}", hash_densities(&counts, side))
  }
}

/// Number of bits set in `start..end` of a little-endian bit vector.
fn count_bits(words: &[u64], start: usize, end: usize) -> u32 {
  if start >= end {
    return 0;
  }
  let (first, last) = (start / 64, (end - 1) / 64);
  let from_start = !0u64 << (start % 64);
  let to_end = !0u64 >> (63 - (end - 1) % 64);
  if first == last {
    return (words[first] & from_start & to_end).count_ones();
  }
  let inner: u32 = words[first + 1..last]
    .iter()
    .map(|word| word.count_ones())
    .sum();
  (words[first] & from_start).count_ones() + inner + (words[last] & to_end).count_ones()
}

/// FNV-1a over the grid, two bits per cell.
fn hash_densities(counts: &[u32; GRID * GRID], side: usize) -> u64 {
  const OFFSET: u64 = 0xcbf29ce484222325;
  const PRIME: u64 = 0x00000100000001b3;

  let cell_size = |cell: usize| (((cell + 1) * side) / GRID - (cell * side) / GRID).max(1);
  let quantize = |density: f32| {
    DENSITY_STEPS
      .iter()
      .position(|&step| density < step)
      .unwrap_or(DENSITY_STEPS.len()) as u8
  };

  let mut hash = OFFSET;
  for row in 0..GRID {
    for quad in 0..GRID / 4 {
      let mut packed = 0u8;
      for slot in 0..4 {
        let column = quad * 4 + slot;
        let area = (cell_size(column) * cell_size(row)) as f32;
        let density = counts[row * GRID + column] as f32 / area;
        packed |= quantize(density) << (slot * 2);
      }
      hash = (hash ^ packed as u64).wrapping_mul(PRIME);
    }
  }
  hash
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn counts_bits_in_a_range() {
    let words = [0xffff_0000_ffff_0000u64, 0x1, 0x8000_0000_0000_0001];
    let naive = |start: usize, end: usize| {
      (start..end)
        .filter(|&bit| words[bit / 64] >> (bit % 64) & 1 == 1)
        .count() as u32
    };
    for start in (0..192).step_by(5) {
      for end in (start..=192).step_by(7) {
        assert_eq!(
          count_bits(&words, start, end),
          naive(start, end),
          "{start}..{end}"
        );
      }
    }
    assert_eq!(count_bits(&words, 0, 192), 32 + 1 + 2);
    assert_eq!(count_bits(&words, 64, 64), 0);
  }

  #[test]
  fn an_empty_mask_is_empty() {
    assert_eq!(Mask::from_fn(8, 8, |_, _| false).fingerprint(), "empty");
  }

  #[test]
  fn has_the_v1_shape() {
    let fingerprint = Mask::from_fn(32, 32, |x, y| x == y).fingerprint();
    let hash = fingerprint
      .strip_prefix("v1:g16:d1:t0.002,0.02,0.08:")
      .unwrap();
    assert_eq!(hash.len(), 16);
    assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
  }

  #[test]
  fn ignores_where_the_change_is() {
    let shape = |x: u32, y: u32| x < 40 && y < 30 && (x / 5 + y / 3).is_multiple_of(2);
    let near = Mask::from_fn(200, 200, |x, y| x >= 10 && y >= 10 && shape(x - 10, y - 10));
    let far = Mask::from_fn(200, 200, |x, y| {
      x >= 120 && y >= 90 && shape(x - 120, y - 90)
    });
    assert_eq!(near.fingerprint(), far.fingerprint());
  }

  #[test]
  fn tells_different_shapes_apart() {
    let block = Mask::from_fn(100, 100, |x, y| {
      (20..60).contains(&x) && (20..60).contains(&y)
    });
    let frame = Mask::from_fn(100, 100, |x, y| {
      (20..60).contains(&x)
        && (20..60).contains(&y)
        && !((24..56).contains(&x) && (24..56).contains(&y))
    });
    assert_ne!(block.fingerprint(), frame.fingerprint());
  }

  #[test]
  fn enumerates_the_original_emissions() {
    let labels = |height, top, bottom| -> Vec<(usize, (usize, usize))> {
      emissions(height, top, bottom)
        .iter()
        .map(|emission| (emission.label, emission.rows))
        .collect()
    };
    assert_eq!(labels(1, 0, 0), [(0, (0, 0))]);
    assert_eq!(labels(2, 0, 0), [(0, (0, 1))]);
    assert_eq!(labels(2, 0, 1), [(0, (0, 1)), (1, (1, 1))]);
    // Row 0 only ever shows up as the neighbour of row 1.
    assert_eq!(labels(5, 0, 0), [(1, (0, 2))]);
    // The row before last comes twice, the last one without its neighbour.
    assert_eq!(labels(5, 4, 4), [(3, (2, 4)), (3, (3, 4)), (4, (4, 4))]);
    assert_eq!(labels(9, 4, 4), [(3, (2, 4)), (4, (3, 5)), (5, (4, 6))]);
  }
}
