//! Layout-shift detection: telling content that moved from content that
//! changed.
//!
//! When an element above grows by twenty pixels, everything below it moves
//! down and a pixel diff lights up the whole page, although only one thing
//! changed. This module explains as much of a diff as it can by vertical
//! movement, and reports what is left.
//!
//! It works on rows. Within a column strip, every row of both images is
//! hashed; a run of head rows whose hashes equal a run of base rows at another
//! height is content that moved. What makes this cheap is that it only looks
//! where the images differ: the columns without a single differing pixel are
//! left out, so a sidebar that stayed put does not stop the page next to it
//! from matching, and rows above and below the first and last difference are
//! never hashed.
//!
//! Matching is exact — a moved row has to be pixel-identical — which means a
//! shift is never invented, at the price of missing content that was both
//! moved and re-rendered. Horizontal movement is not looked for.

use std::collections::HashMap;

use crate::antialias::View;
use crate::mask::{Mask, Rect};

/// Columns without a difference narrower than this do not split a strip:
/// the gap between two words is not the gap between two page columns.
const STRIP_GAP: usize = 16;
/// Past this many strips the diff is scattered; treat it as one.
const MAX_STRIPS: usize = 8;
/// Positions tried for a row that appears several times in the base image.
const MAX_CANDIDATES: usize = 16;

/// A block of rows found at another height in the head image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShiftSegment {
  /// Column strip the block belongs to.
  pub x: u32,
  pub width: u32,
  /// Top of the block in the base image, and in the head image.
  pub base_y: u32,
  pub head_y: u32,
  pub height: u32,
}

impl ShiftSegment {
  /// Vertical displacement, positive when the block moved down.
  pub fn dy(&self) -> i64 {
    self.head_y as i64 - self.base_y as i64
  }
}

/// How much of a diff is movement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShiftReport {
  /// Blocks that moved, top to bottom.
  pub segments: Vec<ShiftSegment>,
  /// Head rows, in strips that hold a shift, with no counterpart in base:
  /// what was added or changed.
  pub inserted: Vec<Rect>,
  /// Base rows, in strips that hold a shift, with no counterpart in head:
  /// what was removed or changed.
  pub removed: Vec<Rect>,
  /// Pixels of the mask that differ only because content moved: both what
  /// used to be there and what is there now exist elsewhere, untouched.
  pub shifted_pixels: u64,
}

pub(crate) struct Input<'a> {
  pub base: View<'a>,
  pub head: View<'a>,
  /// Size of each image before padding to the common canvas.
  pub base_size: (usize, usize),
  pub head_size: (usize, usize),
  /// First and last row where the images differ.
  pub raw_rows: (usize, usize),
  /// Bitset of the columns where the images differ.
  pub raw_columns: &'a [u64],
  pub mask: &'a Mask,
  pub min_rows: usize,
}

pub(crate) fn detect(input: &Input) -> Option<ShiftReport> {
  let mut report = ShiftReport::default();
  for (x0, x1) in strips(input.raw_columns, input.base.width) {
    analyse_strip(input, x0, x1, &mut report);
  }
  if report.segments.is_empty() {
    return None;
  }
  report
    .segments
    .sort_by_key(|segment| (segment.head_y, segment.x));
  Some(report)
}

/// Group the differing columns into strips, as inclusive ranges.
fn strips(columns: &[u64], width: usize) -> Vec<(usize, usize)> {
  let is_set = |x: usize| columns[x / 64] & (1 << (x % 64)) != 0;
  let mut strips: Vec<(usize, usize)> = Vec::new();
  for x in (0..width).filter(|&x| is_set(x)) {
    match strips.last_mut() {
      Some((_, end)) if x - *end <= STRIP_GAP => *end = x,
      _ => strips.push((x, x)),
    }
  }
  if strips.len() > MAX_STRIPS {
    let (first, last) = (strips[0].0, strips[strips.len() - 1].1);
    return vec![(first, last)];
  }
  strips
}

/// A row reduced to what matching needs.
#[derive(Clone, Copy)]
struct Row {
  hash: u64,
  /// One colour across the strip. Flat rows are everywhere, so they can
  /// travel with a block but never anchor one.
  flat: bool,
  /// Past the bottom or the right edge of its image: padding, not content.
  void: bool,
}

fn signature(pixels: &[u32], void: bool) -> Row {
  const MULTIPLIER: u64 = 0x517cc1b727220a95;
  let first = pixels[0];
  let mut varies = 0u32;
  let mut hash = 0x9e3779b97f4a7c15u64 ^ pixels.len() as u64;
  // Two pixels per multiplication.
  let (pairs, rest) = pixels.as_chunks::<2>();
  for &[left, right] in pairs {
    varies |= (left ^ first) | (right ^ first);
    hash = (hash.rotate_left(5) ^ (left as u64 | (right as u64) << 32)).wrapping_mul(MULTIPLIER);
  }
  for &pixel in rest {
    varies |= pixel ^ first;
    hash = (hash.rotate_left(5) ^ pixel as u64).wrapping_mul(MULTIPLIER);
  }
  hash ^= hash >> 32;
  hash = hash.wrapping_mul(0xd6e8feb86659fd93);
  hash ^= hash >> 32;
  Row {
    hash,
    flat: varies == 0,
    void,
  }
}

/// Up to `MAX_CANDIDATES` entries of a sorted list, around `target`.
fn nearest(positions: &[usize], target: usize) -> &[usize] {
  if positions.len() <= MAX_CANDIDATES {
    return positions;
  }
  let pivot = positions.partition_point(|&position| position < target);
  let start = pivot
    .saturating_sub(MAX_CANDIDATES / 2)
    .min(positions.len() - MAX_CANDIDATES);
  &positions[start..start + MAX_CANDIDATES]
}

#[derive(Clone, Copy, PartialEq)]
enum HeadRow {
  Unmatched,
  InPlace,
  Shifted,
}

/// Columns `x0..=x1` of row `y`.
fn strip_row<'a>(view: &View<'a>, x0: usize, x1: usize, y: usize) -> &'a [u32] {
  let offset = y * view.width;
  &view.pixels[offset + x0..=offset + x1]
}

fn analyse_strip<'a>(input: &Input<'a>, x0: usize, x1: usize, report: &mut ShiftReport) {
  let base_row = |y: usize| strip_row(&input.base, x0, x1, y);
  let head_row = |y: usize| strip_row(&input.head, x0, x1, y);

  // Rows that differ within this strip. Everything outside is identical in
  // place, so it can neither have moved nor hold a pixel to explain.
  let (first_row, last_row) = input.raw_rows;
  let mut differing = (first_row..=last_row).filter(|&y| base_row(y) != head_row(y));
  let Some(top) = differing.next() else {
    return;
  };
  let bottom = differing.next_back().unwrap_or(top);
  let rows = bottom - top + 1;
  if rows < input.min_rows {
    return;
  }

  let signatures = |row: &dyn Fn(usize) -> &'a [u32], size: (usize, usize)| -> Vec<Row> {
    (top..=bottom)
      .map(|y| signature(row(y), y >= size.1 || x0 >= size.0))
      .collect()
  };
  let base = signatures(&base_row, input.base_size);
  let head = signatures(&head_row, input.head_size);

  let mut positions: HashMap<u64, Vec<usize>> = HashMap::new();
  for (index, row) in base.iter().enumerate() {
    if !row.flat && !row.void {
      positions.entry(row.hash).or_default().push(index);
    }
  }

  let same = |head_index: usize, base_index: usize| {
    let (head_row, base_row) = (head[head_index], base[base_index]);
    !head_row.void && !base_row.void && head_row.hash == base_row.hash
  };
  let run_length = |head_index: usize, base_index: usize| {
    (0..rows - head_index.max(base_index))
      .take_while(|&offset| same(head_index + offset, base_index + offset))
      .count()
  };
  // Hashes can collide; a shift is only reported once the pixels agree.
  let pixels_agree = |head_index: usize, base_index: usize, length: usize| {
    (0..length)
      .all(|offset| head_row(top + head_index + offset) == base_row(top + base_index + offset))
  };

  let mut head_rows = vec![HeadRow::Unmatched; rows];
  let mut base_used = vec![false; rows];
  let mut segments: Vec<ShiftSegment> = Vec::new();

  let mut index = 0;
  while index < rows {
    if head[index].void {
      index += 1;
      continue;
    }

    // Staying in place wins a tie: only a strictly longer run is a shift.
    let in_place = run_length(index, index);
    let mut best = (in_place, index);
    if !head[index].flat {
      if let Some(candidates) = positions.get(&head[index].hash) {
        for &candidate in nearest(candidates, index) {
          let length = run_length(index, candidate);
          if length > best.0 {
            best = (length, candidate);
          }
        }
      }
    }

    let (length, source) = best;
    if source != index {
      // Flat rows travel with a block, upwards too: take the unclaimed rows
      // above that line up with it.
      let mut lead = 0;
      while lead < index.min(source)
        && head_rows[index - lead - 1] == HeadRow::Unmatched
        && same(index - lead - 1, source - lead - 1)
      {
        lead += 1;
      }
      let (head_start, base_start, total) = (index - lead, source - lead, length + lead);
      let textured = (head_start..head_start + total)
        .filter(|&row| !head[row].flat)
        .count();

      if textured >= input.min_rows && pixels_agree(head_start, base_start, total) {
        head_rows[head_start..head_start + total].fill(HeadRow::Shifted);
        base_used[base_start..base_start + total].fill(true);
        segments.push(ShiftSegment {
          x: x0 as u32,
          width: (x1 - x0 + 1) as u32,
          base_y: (top + base_start) as u32,
          head_y: (top + head_start) as u32,
          height: total as u32,
        });
        index += length;
        continue;
      }
    }

    if in_place > 0 {
      head_rows[index..index + in_place].fill(HeadRow::InPlace);
      base_used[index..index + in_place].fill(true);
      index += in_place;
    } else {
      index += 1;
    }
  }

  if segments.is_empty() {
    return;
  }

  let band = |start: usize, end: usize| Rect {
    x: x0 as u32,
    y: (top + start) as u32,
    width: (x1 - x0 + 1) as u32,
    height: (end - start) as u32,
  };
  let head_unmatched = |row: usize| !head[row].void && head_rows[row] == HeadRow::Unmatched;
  let base_unmatched = |row: usize| !base[row].void && !base_used[row];

  report
    .inserted
    .extend(runs(rows, head_unmatched).map(|(start, end)| band(start, end)));
  report
    .removed
    .extend(runs(rows, base_unmatched).map(|(start, end)| band(start, end)));
  report.shifted_pixels += (0..rows)
    .filter(|&row| !head_unmatched(row) && !base_unmatched(row))
    .map(|row| input.mask.count_in_row(top + row, x0, x1))
    .sum::<u64>();
  report.segments.extend(segments);
}

/// Maximal runs of indices in `0..length` satisfying `predicate`, as
/// half-open ranges.
fn runs(length: usize, predicate: impl Fn(usize) -> bool) -> impl Iterator<Item = (usize, usize)> {
  let mut index = 0;
  std::iter::from_fn(move || {
    while index < length && !predicate(index) {
      index += 1;
    }
    if index == length {
      return None;
    }
    let start = index;
    while index < length && predicate(index) {
      index += 1;
    }
    Some((start, index))
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn groups_columns_into_strips() {
    let bitset = |columns: &[usize]| {
      let mut words = vec![0u64; 4];
      for &x in columns {
        words[x / 64] |= 1 << (x % 64);
      }
      words
    };
    assert_eq!(strips(&bitset(&[]), 200), []);
    assert_eq!(strips(&bitset(&[5]), 200), [(5, 5)]);
    // A gap of sixteen columns is still one strip, seventeen is two.
    assert_eq!(
      strips(&bitset(&[5, 21, 100, 117]), 200),
      [(5, 21), (100, 100), (117, 117)]
    );
    assert_eq!(strips(&bitset(&[63, 64, 65]), 200), [(63, 65)]);
  }

  #[test]
  fn merges_a_scattered_diff_into_one_strip() {
    let mut words = vec![0u64; 8];
    for strip in 0..=MAX_STRIPS {
      let x = strip * 40;
      words[x / 64] |= 1 << (x % 64);
    }
    assert_eq!(strips(&words, 500), [(0, MAX_STRIPS * 40)]);
  }

  #[test]
  fn finds_runs() {
    let flags = [false, true, true, false, true, false, false, true];
    let found: Vec<_> = runs(flags.len(), |index| flags[index]).collect();
    assert_eq!(found, [(1, 3), (4, 5), (7, 8)]);
    assert_eq!(runs(0, |_| true).count(), 0);
    assert_eq!(runs(3, |_| true).collect::<Vec<_>>(), [(0, 3)]);
  }

  #[test]
  fn picks_candidates_around_the_target() {
    let positions: Vec<usize> = (0..100).map(|index| index * 10).collect();
    assert_eq!(nearest(&positions[..5], 20), &positions[..5]);
    let around = nearest(&positions, 500);
    assert_eq!(around.len(), MAX_CANDIDATES);
    assert!(around.contains(&490) && around.contains(&500) && around.contains(&510));
    assert_eq!(nearest(&positions, 0), &positions[..MAX_CANDIDATES]);
    assert_eq!(
      nearest(&positions, 5000),
      &positions[100 - MAX_CANDIDATES..]
    );
  }

  #[test]
  fn signatures_tell_rows_apart() {
    let a = signature(&[1, 2, 3, 4], false);
    let b = signature(&[1, 2, 4, 3], false);
    let flat = signature(&[7, 7, 7, 7], false);
    assert_ne!(a.hash, b.hash);
    assert_eq!(a.hash, signature(&[1, 2, 3, 4], false).hash);
    assert!(!a.flat && flat.flat);
    // The same pixels over another width are another row.
    assert_ne!(signature(&[7, 7], false).hash, flat.hash);
  }
}
