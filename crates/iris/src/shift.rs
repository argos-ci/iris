//! Layout-shift detection: telling content that moved from content that
//! changed.
//!
//! When an element above grows by twenty pixels, everything below it moves
//! down and a pixel diff lights up the whole page, although only one thing
//! changed. This module explains as much of a diff as it can by vertical
//! movement, and reports what is left.
//!
//! It works on rows, and only where the images differ: the columns without a
//! single differing pixel are left out, so a sidebar that stayed put does not
//! stop the page next to it from matching, and rows above and below the first
//! and last difference are never looked at. Within a strip of columns:
//!
//! 1. Rows are hashed, and a head row that differs in place and is equal to
//!    base rows elsewhere votes for the distances to them. Only distances seen
//!    this way, pixel for pixel, are ever tried: a shift is never invented
//!    from rows that merely look alike. Rows that say nothing of where they
//!    are — one colour, or like the rows on either side, as the background
//!    between a card's borders is — do not vote.
//! 2. From the rows that voted for it, each distance spreads up and down for
//!    as long as it explains the rows it reaches: for as long as none of
//!    their pixels would be in the mask, compared with the rows they would
//!    have come from or the ones just above and below those. Browsers lay
//!    content out at fractional positions and render it anew where it lands:
//!    a block that moves by 23.5 rows ends up partly 23 and partly 24 rows
//!    down, its gradients dithered and its edges anti-aliased afresh. Exact
//!    matching cut it into pieces and lost the rows that no longer matched.
//! 3. A distance only counts in stretches where rows that say where they
//!    are show it, and a base row went to one place only: a copy of content
//!    that is still where it was is new, not moved.
//! 4. Each row gets one explanation — in place, moved by one of the
//!    distances, or none — so as to leave the fewest rows unexplained in the
//!    fewest blocks.
//!
//! Horizontal movement is not looked for.

use std::cmp::Reverse;
use std::collections::HashMap;

use crate::antialias::is_antialiased;
use crate::delta::color_delta;
use crate::mask::{Mask, Rect};
use crate::options::Antialiasing;
use crate::subpixel::is_subpixel_shift;
use crate::view::{Row, View};

/// Columns without a difference narrower than this do not split a strip:
/// the gap between two words is not the gap between two page columns.
const STRIP_GAP: usize = 16;
/// Past this many strips the diff is scattered; treat it as one.
const MAX_STRIPS: usize = 8;
/// Positions tried for a row that appears several times in the base image.
const MAX_CANDIDATES: usize = 16;
/// Distances a strip's content may have moved by, at most.
const MAX_DISPLACEMENTS: usize = 15;

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
  /// used to be there and what is there now are found elsewhere, near
  /// enough that the mask would not tell them apart.
  pub shifted_pixels: u64,
}

pub(crate) struct Input<'a> {
  pub base: View<'a>,
  pub head: View<'a>,
  /// First and last row where the images differ.
  pub raw_rows: (usize, usize),
  /// Bitset of the columns where the images differ.
  pub raw_columns: &'a [u64],
  pub mask: &'a Mask,
  /// Largest colour delta the mask leaves out: pixels this close are the
  /// same content.
  pub max_delta: i64,
  /// The detector that left anti-aliasing out of the mask, and what
  /// "unchanged" means to the sub-pixel one.
  pub antialiasing: Antialiasing,
  pub same: i64,
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
struct Signature {
  hash: u64,
  /// One colour across the strip. Flat rows are everywhere, so they can
  /// travel with a block but never anchor one.
  flat: bool,
  /// Past the bottom or the right edge of its image: padding, not content.
  void: bool,
}

fn signature(row: Row, void: bool) -> Signature {
  const MULTIPLIER: u64 = 0x517cc1b727220a95;
  let length = row.pixels.len() + row.zeros;
  let mut pixels = row.iter();
  let first = row.pixels.first().copied().unwrap_or(0);
  let mut varies = 0u32;
  let mut hash = 0x9e3779b97f4a7c15u64 ^ length as u64;
  // Two pixels per multiplication, paired the same way wherever the image
  // ends, so equal rows hash equal.
  while let Some(left) = pixels.next() {
    let word = match pixels.next() {
      Some(right) => {
        varies |= (left ^ first) | (right ^ first);
        left as u64 | (right as u64) << 32
      }
      None => {
        varies |= left ^ first;
        left as u64
      }
    };
    hash = (hash.rotate_left(5) ^ word).wrapping_mul(MULTIPLIER);
  }
  hash ^= hash >> 32;
  hash = hash.wrapping_mul(0xd6e8feb86659fd93);
  hash ^= hash >> 32;
  Signature {
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

/// Pixels compared at once when looking for the first that differs.
const CHUNK: usize = 8;

/// Whether every pixel of `now` is within `max_delta` of the one at its
/// column in `before`, or else passes `otherwise`, given that column and
/// its colour. Equal pixels are skipped a chunk at a time.
#[inline]
fn matches(
  now: Row,
  before: Row,
  max_delta: i64,
  mut otherwise: impl FnMut(usize, u32) -> bool,
) -> bool {
  let mut pass = |x: usize, pixel: u32, other: u32| {
    pixel == other || color_delta(other, pixel) <= max_delta || otherwise(x, pixel)
  };
  let shared = now.pixels.len().min(before.pixels.len());
  let (now_pixels, before_pixels) = (&now.pixels[..shared], &before.pixels[..shared]);
  let mut x = 0;
  while x < shared {
    let end = (x + CHUNK).min(shared);
    if now_pixels[x..end] != before_pixels[x..end]
      && !(x..end).all(|x| pass(x, now_pixels[x], before_pixels[x]))
    {
      return false;
    }
    x = end;
  }
  let width = now.pixels.len() + now.zeros;
  (shared..width).all(|x| pass(x, now.get(x), before.get(x)))
}

/// The two images lined up for a move of `offset` rows down: row `y` of the
/// first is the base row that row `y` of the second came from.
fn aligned<'a>(base: View<'a>, head: View<'a>, offset: isize) -> (View<'a>, View<'a>) {
  let (mut base, mut head) = if offset >= 0 {
    (base, head.skip_rows(offset as usize))
  } else {
    (base.skip_rows(offset.unsigned_abs()), head)
  };
  let height = base.height.min(head.height);
  (base.height, head.height) = (height, height);
  (base, head)
}

/// The columns of a strip, from the row above its first difference to the
/// row below its last.
struct Strip<'i, 'a> {
  input: &'i Input<'a>,
  x0: usize,
  x1: usize,
  top: usize,
  rows: usize,
  base: Vec<Signature>,
  head: Vec<Signature>,
}

impl<'a> Strip<'_, 'a> {
  fn base_row(&self, index: usize) -> Row<'a> {
    self.input.base.row(self.top + index, self.x0, self.x1)
  }

  fn head_row(&self, index: usize) -> Row<'a> {
    self.input.head.row(self.top + index, self.x0, self.x1)
  }

  /// Whether every pixel of `a` is within the mask's threshold of the one
  /// below it in `b`.
  fn close(&self, a: Row, b: Row) -> bool {
    matches(a, b, self.input.max_delta, |_, _| false)
  }

  /// Head rows that say where they are: not one colour, and not like the
  /// rows on either side. A row of background between a card's two borders
  /// looks like every other row of that card, so it fits wherever the card
  /// is: it can follow a block that moved, never tell that one did.
  fn telling(&self) -> Vec<bool> {
    let (_, height) = self.input.head.image_size();
    let row = |y: usize| self.input.head.row(y, self.x0, self.x1);
    // Whether each row looks like the next, from the row above the strip,
    // when there is one, to its last row.
    let like: Vec<bool> = (self.top.saturating_sub(1)..self.top + self.rows)
      .map(|y| y + 1 < height && self.close(row(y), row(y + 1)))
      .collect();
    let skipped = usize::from(self.top == 0);
    (0..self.rows)
      .map(|index| {
        let above = index + 1 > skipped && like[index - skipped];
        let below = like[index + 1 - skipped];
        let row = self.head[index];
        !row.flat && !row.void && !above && !below
      })
      .collect()
  }

  /// Distances content was seen to move by: a telling head row that differs
  /// in place and is equal to base rows elsewhere votes for the distances to
  /// them, one vote shared between all the places it may have come from. The
  /// distances with `min_rows` votes, most voted first, at most
  /// `MAX_DISPLACEMENTS` of them, each with the rows that voted for it.
  fn displacements(&self, telling: &[bool]) -> Vec<(isize, Vec<usize>)> {
    let mut positions: HashMap<u64, Vec<usize>> = HashMap::new();
    for (index, row) in self.base.iter().enumerate() {
      if !row.flat && !row.void {
        positions.entry(row.hash).or_default().push(index);
      }
    }
    let mut votes: HashMap<isize, (f64, Vec<usize>)> = HashMap::new();
    for (index, row) in self.head.iter().enumerate() {
      let base = self.base[index];
      if !telling[index] || !base.void && base.hash == row.hash {
        continue;
      }
      let Some(sources) = positions.get(&row.hash) else {
        continue;
      };
      let sources = nearest(sources, index);
      let share = 1.0 / sources.len() as f64;
      for &source in sources {
        let (count, voters) = votes.entry(index as isize - source as isize).or_default();
        *count += share;
        voters.push(index);
      }
    }
    let mut found: Vec<(isize, (f64, Vec<usize>))> = votes
      .into_iter()
      .filter(|(_, (votes, _))| *votes >= self.input.min_rows as f64)
      .collect();
    // Between equal votes, the shorter move first, then the upward one: any
    // fixed order will do, as long as it is not the map's.
    found.sort_by(|a, b| {
      (b.1)
        .0
        .total_cmp(&(a.1).0)
        .then(a.0.abs().cmp(&b.0.abs()))
        .then(a.0.cmp(&b.0))
    });
    found.truncate(MAX_DISPLACEMENTS);
    found
      .into_iter()
      .map(|(offset, (_, voters))| (offset, voters))
      .collect()
  }

  /// Which of `offsets` explain each head row, as a bitset: bit `s` when
  /// the row is what became of the base row `offsets[s]` rows above it.
  /// Staying in place is tried on every row; a move only around the rows
  /// found at its distance exactly, from which it spreads to the rows above
  /// and below for as long as it explains them. Each row is tried once per
  /// move that reaches it, not once per move.
  ///
  /// Browsers lay content out at fractional positions, snap each piece to
  /// the pixel grid on its own and render it anew: a block that moves by
  /// 23.5 rows lands some of its pieces 23 rows down and others 24, with its
  /// gradients dithered and its edges anti-aliased afresh. So a row is
  /// explained by a move when none of its pixels would be in the mask: each
  /// is within the mask's threshold of the pixel it moved from or of the one
  /// just above or below that, or is anti-aliasing to the detector that made
  /// the mask. Staying in place gets no such slack: a row that moved by one
  /// pixel did not stay.
  fn explain(&self, offsets: &[isize], seeds: &[&[usize]]) -> Vec<u32> {
    let input = self.input;
    let views: Vec<(View, View)> = offsets
      .iter()
      .map(|&offset| aligned(input.base, input.head, offset))
      .collect();
    let explains = |index: usize, state: usize| -> bool {
      let offset = offsets[state];
      let Some(source) = index
        .checked_add_signed(-offset)
        .filter(|&source| source < self.rows)
      else {
        return false;
      };
      if self.head[index].void || self.base[source].void {
        return false;
      }
      let (now, before) = (self.head_row(index), self.base_row(source));
      if now == before {
        return true;
      }
      let around = |source: Option<usize>| {
        source
          .filter(|&source| offset != 0 && source < self.rows && !self.base[source].void)
          .map(|source| self.base_row(source))
      };
      let (above, below) = (around(source.checked_sub(1)), around(Some(source + 1)));
      let (base, head) = &views[state];
      let y = self.top + index.min(source);
      matches(now, before, input.max_delta, |x, pixel| {
        let close = |row: Row| {
          let other = row.get(x);
          other == pixel || color_delta(other, pixel) <= input.max_delta
        };
        above.is_some_and(close)
          || below.is_some_and(close)
          || match input.antialiasing {
            Antialiasing::Off => false,
            Antialiasing::Classic => is_antialiased(base, head, self.x0 + x, y),
            Antialiasing::Subpixel => is_subpixel_shift(base, head, self.x0 + x, y, input.same),
          }
      })
    };
    let mut explained: Vec<u32> = (0..self.rows)
      .map(|index| u32::from(explains(index, 0)))
      .collect();
    for (state, seeds) in seeds.iter().enumerate().skip(1) {
      let bit = 1 << state;
      for &seed in seeds.iter() {
        if explained[seed] & bit != 0 || !explains(seed, state) {
          continue;
        }
        explained[seed] |= bit;
        let mut index = seed;
        while index > 0 && explained[index - 1] & bit == 0 && explains(index - 1, state) {
          index -= 1;
          explained[index] |= bit;
        }
        let mut index = seed;
        while index + 1 < self.rows && explained[index + 1] & bit == 0 && explains(index + 1, state)
        {
          index += 1;
          explained[index] |= bit;
        }
      }
    }
    explained
  }
}

/// Clear, in `explained`, every move where it has no evidence of its own.
///
/// A move counts in a stretch of rows it explains when `min_rows` of them
/// are anchors: telling rows that staying in place does not explain.
/// Elsewhere it explains nothing, however many plain rows it happens to fit.
///
/// And each base row went to one place: the stretches with the most
/// evidence claim the rows they came from first. Rows of a stretch whose
/// source is already claimed are a copy of what moved, not where it went —
/// new content that looks like old — and what is left of the stretch needs
/// evidence of its own.
fn keep_evidenced(explained: &mut [u32], anchors: &[bool], offsets: &[isize], min_rows: usize) {
  let rows = explained.len();
  let evidence =
    |start: usize, end: usize| anchors[start..end].iter().filter(|&&anchor| anchor).count();
  let mut stretches: Vec<(usize, usize, usize, usize)> = Vec::new();
  for state in 1..offsets.len() {
    let bit = 1 << state;
    let found: Vec<(usize, usize)> = runs(rows, |index| explained[index] & bit != 0).collect();
    for (start, end) in found {
      stretches.push((evidence(start, end), state, start, end));
      explained[start..end]
        .iter_mut()
        .for_each(|bits| *bits &= !bit);
    }
  }
  stretches.sort_by_key(|&(found, state, start, end)| {
    (Reverse(found), Reverse(end - start), state, start)
  });
  let mut claimed = vec![false; rows];
  for (found, state, start, end) in stretches {
    if found < min_rows {
      continue;
    }
    let source = |index: usize| (index as isize - offsets[state]) as usize;
    let free: Vec<(usize, usize)> = runs(end - start, |at| !claimed[source(start + at)]).collect();
    for (from, to) in free
      .into_iter()
      .map(|(from, to)| (start + from, start + to))
    {
      if evidence(from, to) >= min_rows {
        explained[from..to]
          .iter_mut()
          .for_each(|bits| *bits |= 1 << state);
        (from..to).for_each(|index| claimed[source(index)] = true);
      }
    }
  }
}

/// The cheapest way to give each row one of `states` explanations, or none,
/// where `explained[row]` has bit `s` set when explanation `s` fits the row.
/// A row left unexplained costs one, unless it is padding; going back in
/// place (explanation 0) costs one too, and starting a block of any other
/// explanation costs `start`. Between equal costs, rows take the lowest
/// explanations they can. Returns, per row, the explanation it got, `states`
/// for none.
fn assign(explained: &[u32], void: &[bool], states: usize, start: u32) -> Vec<usize> {
  const NEVER: (u32, u32) = (u32::MAX / 4, u32::MAX / 4);
  let none = states;
  // Above the strip, everything is in place.
  let mut previous = vec![NEVER; states + 1];
  previous[0] = (0, 0);
  let mut from = vec![0u8; explained.len() * (states + 1)];
  for (row, &bits) in explained.iter().enumerate() {
    let (best, best_state) = previous
      .iter()
      .copied()
      .zip(0..)
      .min()
      .expect("one state at least");
    let mut current = vec![NEVER; states + 1];
    for state in 0..=none {
      let (cost, rank) = match state {
        state if state == none => (u32::from(!void[row]), 0),
        state if bits & 1 << state != 0 => (0, state as u32),
        _ => continue,
      };
      let entering = match state {
        0 => 1,
        state if state == none => 0,
        _ => start,
      };
      let switched = (best.0 + entering, best.1);
      let (total, before) = if previous[state] <= switched {
        (previous[state], state)
      } else {
        (switched, best_state)
      };
      current[state] = (total.0 + cost, total.1 + rank);
      from[row * (states + 1) + state] = before as u8;
    }
    previous = current;
  }
  let (_, mut state) = previous
    .iter()
    .copied()
    .zip(0..)
    .min()
    .expect("one state at least");
  let mut assigned = vec![none; explained.len()];
  for row in (0..explained.len()).rev() {
    assigned[row] = state;
    state = from[row * (states + 1) + state] as usize;
  }
  assigned
}

/// Maximal runs of equal values in `values`, as `(value, start, end)`.
fn blocks(values: &[usize]) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
  let mut start = 0;
  std::iter::from_fn(move || {
    let value = *values.get(start)?;
    let end = start
      + values[start..]
        .iter()
        .take_while(|&&other| other == value)
        .count();
    let block = (value, start, end);
    start = end;
    Some(block)
  })
}

fn analyse_strip<'a>(input: &Input<'a>, x0: usize, x1: usize, report: &mut ShiftReport) {
  let base_row = |y: usize| input.base.row(y, x0, x1);
  let head_row = |y: usize| input.head.row(y, x0, x1);

  // Rows that differ within this strip. Everything outside is identical in
  // place, so it can neither have moved nor hold a pixel to explain.
  let (first_row, last_row) = input.raw_rows;
  let mut differing = (first_row..=last_row).filter(|&y| base_row(y) != head_row(y));
  let Some(top) = differing.next() else {
    return;
  };
  let bottom = differing.next_back().unwrap_or(top);
  if bottom - top + 1 < input.min_rows {
    return;
  }
  // And the unchanged row on either side, for content that moved by a row
  // and a fraction to have the row next to where it came from at hand.
  let (top, bottom) = (
    top.saturating_sub(1),
    (bottom + 1).min(input.base.height - 1),
  );
  let rows = bottom - top + 1;

  let signatures = |view: &View<'a>| -> Vec<Signature> {
    let (image_width, image_height) = view.image_size();
    (top..=bottom)
      .map(|y| signature(view.row(y, x0, x1), y >= image_height || x0 >= image_width))
      .collect()
  };
  let strip = Strip {
    input,
    x0,
    x1,
    top,
    rows,
    base: signatures(&input.base),
    head: signatures(&input.head),
  };

  // Content is only ever looked for at distances whole rows were found at,
  // pixel for pixel: a shift is never invented from rows that merely look
  // alike. Staying in place comes first.
  let telling = strip.telling();
  let found = strip.displacements(&telling);
  if found.is_empty() {
    return;
  }
  let offsets: Vec<isize> = std::iter::once(0)
    .chain(found.iter().map(|&(offset, _)| offset))
    .collect();
  let seeds: Vec<&[usize]> = std::iter::once(&[][..])
    .chain(found.iter().map(|(_, voters)| &voters[..]))
    .collect();

  let mut explained = strip.explain(&offsets, &seeds);
  let anchors: Vec<bool> = (0..rows)
    .map(|index| telling[index] && explained[index] & 1 == 0)
    .collect();
  keep_evidenced(&mut explained, &anchors, &offsets, input.min_rows);

  // One move per stretch of rows: a row that several moves explain — a flat
  // one, one of a repeated pattern, one of a block that moved by 23 rows
  // next to one that moved by 24 — stays with its neighbours. A block is
  // worth starting for `min_rows` rows, and one the choice cuts down to less
  // evidence than that is not one.
  let void: Vec<bool> = strip.head.iter().map(|row| row.void).collect();
  let none = offsets.len();
  let mut assigned = assign(&explained, &void, offsets.len(), input.min_rows as u32 - 1);
  let short: Vec<(usize, usize)> = blocks(&assigned)
    .filter(|&(state, start, end)| {
      state != 0
        && state != none
        && anchors[start..end].iter().filter(|&&anchor| anchor).count() < input.min_rows
    })
    .map(|(_, start, end)| (start, end))
    .collect();
  for (start, end) in short {
    for index in start..end {
      assigned[index] = if explained[index] & 1 != 0 { 0 } else { none };
    }
  }

  let mut base_used = vec![false; rows];
  let mut segments: Vec<ShiftSegment> = Vec::new();
  for (state, start, end) in blocks(&assigned) {
    if state == none {
      continue;
    }
    let offset = offsets[state];
    let source = (start as isize - offset) as usize;
    base_used[source..source + end - start].fill(true);
    if offset != 0 {
      segments.push(ShiftSegment {
        x: x0 as u32,
        width: (x1 - x0 + 1) as u32,
        base_y: (top + source) as u32,
        head_y: (top + start) as u32,
        height: (end - start) as u32,
      });
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
  let head_unmatched = |row: usize| !strip.head[row].void && assigned[row] == none;
  let base_unmatched = |row: usize| !strip.base[row].void && !base_used[row];

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
  use crate::delta::max_delta;

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

  fn row(pixels: &[u32], zeros: usize) -> Row<'_> {
    Row { pixels, zeros }
  }

  #[test]
  fn signatures_tell_rows_apart() {
    let a = signature(row(&[1, 2, 3, 4], 0), false);
    let b = signature(row(&[1, 2, 4, 3], 0), false);
    let flat = signature(row(&[7, 7, 7, 7], 0), false);
    assert_ne!(a.hash, b.hash);
    assert_eq!(a.hash, signature(row(&[1, 2, 3, 4], 0), false).hash);
    assert!(!a.flat && flat.flat);
    // The same pixels over another width are another row.
    assert_ne!(signature(row(&[7, 7], 0), false).hash, flat.hash);
  }

  #[test]
  fn equal_rows_hash_equal_wherever_their_image_ends() {
    let full = signature(row(&[5, 6, 7, 0, 0], 0), false);
    for cut in [row(&[5, 6, 7], 2), row(&[5, 6, 7, 0], 1)] {
      assert_eq!(signature(cut, false).hash, full.hash);
    }
    assert!(signature(row(&[], 4), false).flat);
    assert!(!signature(row(&[3], 3), false).flat);
  }

  #[test]
  fn finds_blocks_of_equal_values() {
    let found: Vec<_> = blocks(&[2, 2, 0, 5, 5, 5]).collect();
    assert_eq!(found, [(2, 0, 2), (0, 2, 3), (5, 3, 6)]);
    assert_eq!(blocks(&[]).count(), 0);
  }

  #[test]
  fn matches_rows_within_the_threshold() {
    let grey = |level: u8| u32::from_ne_bytes([level, level, level, 255]);
    let (light, lighter) = ([grey(240); 20], [grey(241); 20]);
    let never = |_, _| false;
    // A level apart: within any threshold but none at all.
    assert!(matches(
      row(&light, 0),
      row(&lighter, 0),
      max_delta(0.05),
      never
    ));
    assert!(!matches(row(&light, 0), row(&lighter, 0), 0, never));

    // A pixel that is not close is left to `otherwise`, with its column.
    let mut marked = light;
    marked[13] = grey(0);
    let mut asked = Vec::new();
    let answer = matches(row(&marked, 0), row(&light, 0), 0, |x, _| {
      asked.push(x);
      true
    });
    assert!(answer);
    assert_eq!(asked, [13]);

    // Past the end of an image, pixels are transparent.
    let cut = row(&light[..5], 15);
    assert!(matches(cut, cut, 0, never));
    assert!(!matches(cut, row(&light, 0), 0, never));
  }

  #[test]
  fn assignment_keeps_blocks_whole() {
    let void = [false; 10];
    // Rows 3.. fit move 1, and rows 5..7 move 2 as well: one block.
    let explained = [1, 1, 1, 2, 2, 6, 6, 2, 2, 2];
    assert_eq!(
      assign(&explained, &void, 3, 4),
      [0, 0, 0, 1, 1, 1, 1, 1, 1, 1]
    );
    // A block costs more than the two rows it would explain, less than four.
    let explained = [1, 2, 2, 1, 1];
    assert_eq!(assign(&explained, &void[..5], 2, 3), [0, 2, 2, 0, 0]);
    let explained = [1, 2, 2, 2, 2, 0, 0, 0];
    let void = [false, false, false, false, false, true, true, true];
    // And padding left unexplained costs nothing.
    assert_eq!(assign(&explained, &void, 2, 3), [0, 1, 1, 1, 1, 2, 2, 2]);
  }

  #[test]
  fn a_move_counts_where_it_has_evidence_of_its_own() {
    let offsets = [0, 2, 5];
    // Move 1 explains rows 2.., from base rows 0..; move 2 rows 5..8, from
    // base rows 0..3, which move 1 has more evidence for.
    let mut explained: Vec<u32> = (0..10)
      .map(|row| u32::from(row >= 2) << 1 | u32::from((5..8).contains(&row)) << 2)
      .collect();
    keep_evidenced(&mut explained, &[true; 10], &offsets, 2);
    let rows_of = |state: u32| {
      explained
        .iter()
        .filter(|&&bits| bits >> state & 1 == 1)
        .count()
    };
    assert_eq!((rows_of(1), rows_of(2)), (8, 0));

    // Without rows that say where they are, a move explains nothing.
    let mut explained: Vec<u32> = (0..10).map(|row| u32::from(row >= 2) << 1).collect();
    keep_evidenced(&mut explained, &[false; 10], &offsets, 2);
    assert!(explained.iter().all(|&bits| bits == 0));
  }
}
