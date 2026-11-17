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

use crate::antialias::Noise;
use crate::delta::within;
use crate::mask::{Mask, Rect};
use crate::view::{chunk_is_equal, Row, View, CHUNK};

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
  /// The detector that left rendering noise out of the mask.
  pub noise: Noise,
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
  let mut pass =
    |x: usize, pixel: u32, other: u32| within(other, pixel, max_delta) || otherwise(x, pixel);
  let shared = now.pixels.len().min(before.pixels.len());
  let chunks = now.pixels[..shared]
    .chunks_exact(CHUNK)
    .zip(before.pixels[..shared].chunks_exact(CHUNK));
  for (index, (now, before)) in chunks.enumerate() {
    if !chunk_is_equal(now, before)
      && !(0..CHUNK).all(|at| pass(index * CHUNK + at, now[at], before[at]))
    {
      return false;
    }
  }
  let width = now.pixels.len() + now.zeros;
  (shared - shared % CHUNK..width).all(|x| pass(x, now.get(x), before.get(x)))
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

/// A distance content was seen to move by, and the head rows that saw it.
struct Displacement {
  offset: isize,
  voters: Vec<usize>,
}

/// How many telling rows lie in any range of rows: the evidence a move
/// needs to count there.
struct Evidence(Vec<usize>);

impl Evidence {
  fn new(telling: &[bool]) -> Self {
    let mut total = 0;
    let before = telling.iter().map(|&telling| {
      total += usize::from(telling);
      total
    });
    Self(std::iter::once(0).chain(before).collect())
  }

  fn within(&self, start: usize, end: usize) -> usize {
    self.0[end] - self.0[start]
  }
}

/// The columns of a strip, from the row above its first difference to the
/// row below its last.
struct Strip<'i, 'a> {
  input: &'i Input<'a>,
  x0: usize,
  x1: usize,
  top: usize,
  base: Vec<Signature>,
  head: Vec<Signature>,
}

impl<'a> Strip<'_, 'a> {
  fn rows(&self) -> usize {
    self.head.len()
  }

  fn base_row(&self, index: usize) -> Row<'a> {
    self.input.base.row(self.top + index, self.x0, self.x1)
  }

  fn head_row(&self, index: usize) -> Row<'a> {
    self.input.head.row(self.top + index, self.x0, self.x1)
  }

  /// Head rows that stayed in place: the mask holds nothing of them. Rows
  /// past the end of either image are padding, never in place.
  fn in_place(&self) -> Vec<bool> {
    (0..self.rows())
      .map(|index| {
        !self.head[index].void
          && !self.base[index].void
          && self
            .input
            .mask
            .count_in_row(self.top + index, self.x0, self.x1)
            == 0
      })
      .collect()
  }

  /// Head rows that say where they are: not in place, not one colour, and
  /// not like the rows on either side. A row of background between a
  /// card's two borders looks like every other row of that card, so it fits
  /// wherever the card is: it can follow a block that moved, never tell
  /// that one did.
  fn telling(&self, in_place: &[bool]) -> Vec<bool> {
    let (_, height) = self.input.head.image_size();
    let row = |y: usize| self.input.head.row(y, self.x0, self.x1);
    (0..self.rows())
      .map(|index| {
        let signature = self.head[index];
        if in_place[index] || signature.flat || signature.void {
          return false;
        }
        let y = self.top + index;
        // Rows that hash alike are alike; a collision only makes a row
        // less telling.
        let like = |other: usize| {
          let hashed_alike = other
            .checked_sub(self.top)
            .and_then(|other| self.head.get(other))
            .is_some_and(|other| other.hash == signature.hash);
          other < height
            && (hashed_alike || matches(row(y), row(other), self.input.max_delta, |_, _| false))
        };
        !y.checked_sub(1).is_some_and(like) && !like(y + 1)
      })
      .collect()
  }

  /// Distances content was seen to move by: a telling head row equal to
  /// base rows elsewhere votes for the distances to them, one vote shared
  /// between all the places it may have come from. The distances with
  /// `min_rows` votes, most voted first, at most `MAX_DISPLACEMENTS` of them.
  fn displacements(&self, telling: &[bool]) -> Vec<Displacement> {
    let mut positions: HashMap<u64, Vec<usize>> = HashMap::new();
    for (index, row) in self.base.iter().enumerate() {
      if !row.flat && !row.void {
        positions.entry(row.hash).or_default().push(index);
      }
    }
    let sources = |index: usize| -> &[usize] {
      match positions.get(&self.head[index].hash) {
        Some(found) if telling[index] => nearest(found, index),
        _ => &[],
      }
    };
    let offset = |index: usize, source: usize| index as isize - source as isize;

    let mut votes: HashMap<isize, f64> = HashMap::new();
    for index in 0..self.rows() {
      let sources = sources(index);
      for &source in sources {
        *votes.entry(offset(index, source)).or_default() += 1.0 / sources.len() as f64;
      }
    }
    let mut kept: Vec<(isize, f64)> = votes
      .into_iter()
      .filter(|&(_, votes)| votes >= self.input.min_rows as f64)
      .collect();
    // Between equal votes, the shorter move first, then the upward one: any
    // fixed order will do, as long as it is not the map's.
    kept.sort_by(|a, b| {
      b.1
        .total_cmp(&a.1)
        .then((a.0.abs(), a.0).cmp(&(b.0.abs(), b.0)))
    });
    kept.truncate(MAX_DISPLACEMENTS);

    let mut found: Vec<Displacement> = kept
      .into_iter()
      .map(|(offset, _)| Displacement {
        offset,
        voters: Vec::new(),
      })
      .collect();
    for index in 0..self.rows() {
      for &source in sources(index) {
        let offset = offset(index, source);
        if let Some(found) = found.iter_mut().find(|found| found.offset == offset) {
          found.voters.push(index);
        }
      }
    }
    found
  }

  /// Which explanations fit each head row, as a bitset: bit 0 when the row
  /// stayed in place, bit `s` when it is what became of the base row
  /// `found[s - 1].offset` rows above it. A move is only tried around the
  /// rows that voted for it, from which it spreads to the rows above and
  /// below for as long as it explains them: each row is tried once per move
  /// that reaches it, not once per move.
  ///
  /// Browsers lay content out at fractional positions, snap each piece to
  /// the pixel grid on its own and render it anew: a block that moves by
  /// 23.5 rows lands some of its pieces 23 rows down and others 24, with its
  /// gradients dithered and its edges anti-aliased afresh. So a row is
  /// explained by a move when none of its pixels would be in the mask: each
  /// is within the mask's threshold of the pixel it moved from or of the one
  /// just above or below that, or is noise to the detector that made the
  /// mask. Staying in place gets no such slack: a row that moved by one
  /// pixel did not stay.
  fn explain(&self, in_place: &[bool], found: &[Displacement]) -> Vec<u32> {
    let input = self.input;
    let rows = self.rows();
    let base_row =
      |source: usize| (source < rows && !self.base[source].void).then(|| self.base_row(source));
    let explains = |index: usize, offset: isize| -> bool {
      let (Some(source), false) = (index.checked_add_signed(-offset), self.head[index].void) else {
        return false;
      };
      let Some(before) = base_row(source) else {
        return false;
      };
      let now = self.head_row(index);
      if self.head[index].hash == self.base[source].hash && now == before {
        return true;
      }
      let (above, below) = (
        source.checked_sub(1).and_then(base_row),
        base_row(source + 1),
      );
      let (base, head) = aligned(input.base, input.head, offset);
      let y = self.top + index.min(source);
      matches(now, before, input.max_delta, |x, pixel| {
        let near = |row: Row| within(row.get(x), pixel, input.max_delta);
        above.is_some_and(near)
          || below.is_some_and(near)
          || input.noise.at(&base, &head, self.x0 + x, y)
      })
    };

    let mut explained: Vec<u32> = in_place.iter().map(|&stayed| u32::from(stayed)).collect();
    for (state, found) in (1..).zip(found) {
      let bit = 1 << state;
      let fits = |index: usize| explains(index, found.offset);
      for &seed in &found.voters {
        if explained[seed] & bit != 0 || !fits(seed) {
          continue;
        }
        explained[seed] |= bit;
        let mut index = seed;
        while index > 0 && explained[index - 1] & bit == 0 && fits(index - 1) {
          index -= 1;
          explained[index] |= bit;
        }
        let mut index = seed;
        while index + 1 < rows && explained[index + 1] & bit == 0 && fits(index + 1) {
          index += 1;
          explained[index] |= bit;
        }
      }
    }
    explained
  }
}

/// A run of rows one move explains, and the evidence it holds.
struct Stretch {
  evidence: usize,
  state: usize,
  start: usize,
  end: usize,
}

/// Clear, in `explained`, every move where it has no evidence of its own.
///
/// A move counts in a stretch of rows it explains when `min_rows` of them
/// are telling. Elsewhere it explains nothing, however many plain rows it
/// happens to fit.
///
/// And each base row went to one place: the stretches with the most
/// evidence claim the rows they came from first. Rows of a stretch whose
/// source is already claimed are a copy of what moved, not where it went —
/// new content that looks like old — and what is left of the stretch needs
/// evidence of its own.
fn keep_evidenced(explained: &mut [u32], evidence: &Evidence, offsets: &[isize], min_rows: usize) {
  let rows = explained.len();
  let mut stretches: Vec<Stretch> = Vec::new();
  for state in 1..offsets.len() {
    let bit = 1 << state;
    let found: Vec<(usize, usize)> = runs(rows, |index| explained[index] & bit != 0).collect();
    for (start, end) in found {
      explained[start..end]
        .iter_mut()
        .for_each(|bits| *bits &= !bit);
      let evidence = evidence.within(start, end);
      if evidence >= min_rows {
        stretches.push(Stretch {
          evidence,
          state,
          start,
          end,
        });
      }
    }
  }
  stretches.sort_by_key(|stretch| {
    (
      Reverse(stretch.evidence),
      Reverse(stretch.end - stretch.start),
      stretch.state,
      stretch.start,
    )
  });
  let mut claimed = vec![false; rows];
  for Stretch {
    state, start, end, ..
  } in stretches
  {
    let source = |index: usize| (index as isize - offsets[state]) as usize;
    let free: Vec<(usize, usize)> = runs(end - start, |at| !claimed[source(start + at)]).collect();
    for (from, to) in free
      .into_iter()
      .map(|(from, to)| (start + from, start + to))
    {
      if evidence.within(from, to) >= min_rows {
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
fn assign(
  explained: &[u32],
  padding: impl Fn(usize) -> bool,
  states: usize,
  start: u32,
) -> Vec<usize> {
  const NEVER: (u32, u32) = (u32::MAX / 4, u32::MAX / 4);
  let none = states;
  let cheapest = |costs: &[(u32, u32)]| {
    costs
      .iter()
      .copied()
      .zip(0..)
      .min()
      .expect("one state at least")
  };
  // Above the strip, everything is in place.
  let mut previous = vec![NEVER; states + 1];
  previous[0] = (0, 0);
  let mut current = previous.clone();
  let mut from = vec![0u8; explained.len() * (states + 1)];
  for (row, &bits) in explained.iter().enumerate() {
    let (best, best_state) = cheapest(&previous);
    current.fill(NEVER);
    for state in 0..=none {
      // What the row costs in that state, the rank that breaks ties, and
      // what entering the state costs.
      let (cost, rank, entering) = match state {
        0 if bits & 1 != 0 => (0, 0, 1),
        state if state == none => (u32::from(!padding(row)), 0, 0),
        state if state != 0 && bits & 1 << state != 0 => (0, state as u32, start),
        _ => continue,
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
    std::mem::swap(&mut previous, &mut current);
  }
  let (_, mut state) = cheapest(&previous);
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
    base: signatures(&input.base),
    head: signatures(&input.head),
  };

  // Content is only ever looked for at distances whole rows were found at,
  // pixel for pixel: a shift is never invented from rows that merely look
  // alike.
  let in_place = strip.in_place();
  let telling = strip.telling(&in_place);
  let found = strip.displacements(&telling);
  if found.is_empty() {
    return;
  }
  let offsets: Vec<isize> = std::iter::once(0)
    .chain(found.iter().map(|found| found.offset))
    .collect();
  let mut explained = strip.explain(&in_place, &found);
  let evidence = Evidence::new(&telling);
  keep_evidenced(&mut explained, &evidence, &offsets, input.min_rows);

  // One move per stretch of rows: a row that several moves explain — a flat
  // one, one of a repeated pattern, one of a block that moved by 23 rows
  // next to one that moved by 24 — stays with its neighbours. A block is
  // worth starting for `min_rows` rows; one the choice cuts down to less
  // evidence than that is not one, and the rows are given out again
  // without that move.
  let none = offsets.len();
  let assigned = loop {
    let assigned = assign(
      &explained,
      |row| strip.head[row].void,
      offsets.len(),
      input.min_rows as u32 - 1,
    );
    let short: Vec<(usize, usize, usize)> = blocks(&assigned)
      .filter(|&(state, start, end)| {
        state != 0 && state != none && evidence.within(start, end) < input.min_rows
      })
      .collect();
    if short.is_empty() {
      break assigned;
    }
    for (state, start, end) in short {
      explained[start..end]
        .iter_mut()
        .for_each(|bits| *bits &= !(1 << state));
    }
  };

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
    let content = |_| false;
    // Rows 3.. fit move 1, and rows 5..7 move 2 as well: one block.
    let explained = [1, 1, 1, 2, 2, 6, 6, 2, 2, 2];
    assert_eq!(
      assign(&explained, content, 3, 4),
      [0, 0, 0, 1, 1, 1, 1, 1, 1, 1]
    );
    // A block costs more than the two rows it would explain, less than four.
    let explained = [1, 2, 2, 1, 1];
    assert_eq!(assign(&explained, content, 2, 3), [0, 2, 2, 0, 0]);
    // And padding left unexplained costs nothing.
    let explained = [1, 2, 2, 2, 2, 0, 0, 0];
    assert_eq!(
      assign(&explained, |row| row >= 5, 2, 3),
      [0, 1, 1, 1, 1, 2, 2, 2]
    );
  }

  #[test]
  fn a_move_counts_where_it_has_evidence_of_its_own() {
    let offsets = [0, 2, 5];
    // Move 1 explains rows 2.., from base rows 0..; move 2 rows 5..8, from
    // base rows 0..3, which move 1 has more evidence for.
    let mut explained: Vec<u32> = (0..10)
      .map(|row| u32::from(row >= 2) << 1 | u32::from((5..8).contains(&row)) << 2)
      .collect();
    keep_evidenced(&mut explained, &Evidence::new(&[true; 10]), &offsets, 2);
    let rows_of = |state: u32| {
      explained
        .iter()
        .filter(|&&bits| bits >> state & 1 == 1)
        .count()
    };
    assert_eq!((rows_of(1), rows_of(2)), (8, 0));

    // Without rows that say where they are, a move explains nothing.
    let mut explained: Vec<u32> = (0..10).map(|row| u32::from(row >= 2) << 1).collect();
    keep_evidenced(&mut explained, &Evidence::new(&[false; 10]), &offsets, 2);
    assert!(explained.iter().all(|&bits| bits == 0));
  }
}
