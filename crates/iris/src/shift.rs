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
//! 5. Plain rows next to explained ones that are more of the same, the
//!    background of a card that grew or shrank, are explained too.
//!
//! Distances are taken sideways as well as down: a row is hashed a second
//! time without the background on either side, so content that slid across
//! hashes the same, and where that content starts says by how much.
//!
//! Where a strip leaves pixels unexplained, it is cut down the gutters of
//! the head image around them and each column is looked at on its own: in a
//! row of cards, the one that grew moves its own content, not that of the
//! cards beside it. A row across them matches nowhere, its parts do.
//!
//! The report says which areas of the mask hold only movement: the rows of a
//! strip, or of a column, where both what is in the head image and what was
//! in the base image are accounted for.

use std::cmp::Reverse;
use std::collections::HashMap;

use crate::antialias::Noise;
use crate::delta::within;
use crate::image::Image;
use crate::mask::{Mask, Rect};
use crate::view::{Row, View, CHUNK};

/// Columns without a difference narrower than this do not split a strip:
/// the gap between two words is not the gap between two page columns.
const STRIP_GAP: usize = 16;
/// Past this many strips the diff is scattered; treat it as one.
const MAX_STRIPS: usize = 8;
/// Positions tried for a row that appears several times in the base image.
const MAX_CANDIDATES: usize = 16;
/// Distances a strip's content may have moved by, at most.
const MAX_DISPLACEMENTS: usize = 15;
/// One vote, in units a row can share out exactly between as many as
/// `MAX_CANDIDATES` places: the least common multiple of 1 to 16.
const VOTE: u64 = 720_720;
/// Times the rows of a strip are given out again after a block came out
/// with too little evidence, at most.
const MAX_SOLVES: usize = 8;
/// Explained rows between two unexplained ones that still leave them in one
/// cluster, to be split into columns together.
const CLUSTER_GAP: usize = 32;
/// Widest line down a gutter that is still part of it: a border.
const BORDER: usize = 2;
/// Pixels the noise detectors read on either side of the one they judge.
const WINDOW: usize = 3;

/// A block of rows found elsewhere in the head image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShiftSegment {
  /// Column strip the block belongs to.
  pub x: u32,
  pub width: u32,
  /// Top of the block in the base image, and in the head image.
  pub base_y: u32,
  pub head_y: u32,
  pub height: u32,
  /// Horizontal displacement, positive when the block moved right: in the
  /// base image, it was `dx` columns left of `x`.
  pub dx: i64,
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
  /// Areas of the mask that differ only because content moved, top to
  /// bottom, none overlapping another: leave them out of the mask and what
  /// is left is what changed.
  pub hidden: Vec<Rect>,
  /// Pixels of the mask in `hidden`: both what used to be there and what is
  /// there now are found elsewhere, near enough that the mask would not tell
  /// them apart.
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
  /// Largest colour delta the least sensitive tier leaves out: on an edge of
  /// content that moved sideways, pixels this close are the same edge drawn
  /// at another fraction of a column.
  pub edge_delta: i64,
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
  for rects in [
    &mut report.inserted,
    &mut report.removed,
    &mut report.hidden,
  ] {
    rects.sort_by_key(|rect| (rect.y, rect.x));
  }
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
  /// Hash of the row without the background on either side of it, and the
  /// column where what is left starts: content that moved sideways hashes
  /// the same, and the two starts say how far it went.
  content: u64,
  left: usize,
  /// One colour across the strip. Flat rows are everywhere, so they can
  /// travel with a block but never anchor one.
  flat: bool,
  /// Past the bottom or the right edge of its image: padding, not content.
  void: bool,
}

/// Hash `pixels`, starting from `seed`.
fn mix(seed: u64, mut pixels: impl Iterator<Item = u32>) -> u64 {
  const MULTIPLIER: u64 = 0x517cc1b727220a95;
  let mut hash = 0x9e3779b97f4a7c15u64 ^ seed;
  // Two pixels per multiplication, paired the same way wherever the image
  // ends, so equal rows hash equal.
  while let Some(left) = pixels.next() {
    let word = match pixels.next() {
      Some(right) => left as u64 | (right as u64) << 32,
      None => left as u64,
    };
    hash = (hash.rotate_left(5) ^ word).wrapping_mul(MULTIPLIER);
  }
  hash ^= hash >> 32;
  hash = hash.wrapping_mul(0xd6e8feb86659fd93);
  hash ^ hash >> 32
}

fn signature(row: Row, void: bool) -> Signature {
  let length = row.pixels.len() + row.zeros;
  let (first, last) = (row.get(0), row.get(length - 1));
  let left = row
    .iter()
    .position(|pixel| pixel != first)
    .unwrap_or(length);
  // Past the image, pixels are transparent, and so is the last one: the
  // padding goes with the rest of the background.
  let right = row
    .pixels
    .iter()
    .rposition(|&pixel| pixel != last)
    .map_or(0, |x| x + 1);
  let content = row.iter().skip(left).take(right.saturating_sub(left));
  Signature {
    hash: mix(length as u64, row.iter()),
    content: mix(first as u64 | (last as u64) << 32, content),
    left,
    flat: left == length,
    void,
  }
}

/// Whether every channel of `pixel` lies between those of `a` and `b`, give
/// or take a level.
fn between(pixel: u32, a: u32, b: u32) -> bool {
  let (pixel, a, b) = (pixel.to_ne_bytes(), a.to_ne_bytes(), b.to_ne_bytes());
  (0..4).all(|channel| {
    let (low, high) = (a[channel].min(b[channel]), a[channel].max(b[channel]));
    (low.saturating_sub(1)..=high.saturating_add(1)).contains(&pixel[channel])
  })
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
  let (now_chunks, _) = now.pixels[..shared].as_chunks::<CHUNK>();
  let (before_chunks, _) = before.pixels[..shared].as_chunks::<CHUNK>();
  for (index, (now, before)) in now_chunks.iter().zip(before_chunks).enumerate() {
    if now != before && !(0..CHUNK).all(|at| pass(index * CHUNK + at, now[at], before[at])) {
      return false;
    }
  }
  let width = now.pixels.len() + now.zeros;
  (shared - shared % CHUNK..width).all(|x| pass(x, now.get(x), before.get(x)))
}

/// The two images lined up for a move of `offset` rows down: row `y` of the
/// first is the base row that row `y` of the second came from.
fn aligned<'a>(base: View<'a>, head: View<'a>, offset: isize) -> (View<'a>, View<'a>) {
  let (base, head) = if offset >= 0 {
    (base, head.skip_rows(offset as usize))
  } else {
    (base.skip_rows(offset.unsigned_abs()), head)
  };
  let height = base.height.min(head.height);
  (base.crop(height), head.crop(height))
}

/// How far content moved: `dy` rows down and `dx` columns right.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct Offset {
  dy: isize,
  dx: isize,
}

/// A distance content was seen to move by, and the head rows that saw it.
struct Displacement {
  offset: Offset,
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

  fn at(&self, row: usize) -> bool {
    self.within(row, row + 1) == 1
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

  /// Distances content was seen to move by: a telling head row whose
  /// content is that of base rows elsewhere votes for the distances to them,
  /// down and sideways, one vote shared between all the places it may have
  /// come from. The distances with `min_rows` votes, most voted first, at
  /// most `MAX_DISPLACEMENTS` of them.
  fn displacements(&self, telling: &[bool]) -> Vec<Displacement> {
    let mut positions: HashMap<u64, Vec<usize>> = HashMap::new();
    for (index, row) in self.base.iter().enumerate() {
      if !row.flat && !row.void {
        positions.entry(row.content).or_default().push(index);
      }
    }
    let sources = |index: usize| -> &[usize] {
      match positions.get(&self.head[index].content) {
        Some(found) if telling[index] => nearest(found, index),
        _ => &[],
      }
    };
    let offset = |index: usize, source: usize| Offset {
      dy: index as isize - source as isize,
      dx: self.head[index].left as isize - self.base[source].left as isize,
    };

    let mut votes: HashMap<Offset, u64> = HashMap::new();
    for index in 0..self.rows() {
      let sources = sources(index);
      for &source in sources {
        *votes.entry(offset(index, source)).or_default() += VOTE / sources.len() as u64;
      }
    }
    let mut kept: Vec<(Offset, u64)> = votes
      .into_iter()
      .filter(|&(_, votes)| votes >= self.input.min_rows as u64 * VOTE)
      .collect();
    // Between equal votes, the shorter move first, then the upward one, then
    // the one to the left: any fixed order will do, as long as it is not the
    // map's.
    kept.sort_by_key(|&(offset, votes)| {
      (
        Reverse(votes),
        offset.dy.abs(),
        offset.dy,
        offset.dx.abs(),
        offset.dx,
      )
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

  /// Whether head row `index` is what became of the base row `offset` rows
  /// above it: none of its pixels would be in the mask, compared with that
  /// row. With `slack`, a pixel may also match the one just above or below
  /// the pixel it moved from.
  ///
  /// Browsers lay content out at fractional positions, snap each piece to
  /// the pixel grid on its own and render it anew: a block that moves by
  /// 23.5 rows lands some of its pieces 23 rows down and others 24, with its
  /// gradients dithered and its edges anti-aliased afresh. A pixel within
  /// the mask's threshold, or noise to the detector that made the mask, is
  /// the same content.
  fn explains(&self, index: usize, offset: Offset, slack: bool) -> bool {
    let input = self.input;
    let rows = self.rows();
    let (Some(source), false) = (index.checked_add_signed(-offset.dy), self.head[index].void)
    else {
      return false;
    };
    if source >= rows || self.base[source].void {
      return false;
    }
    if offset.dx != 0 {
      return self.explains_across(index, source, offset.dx, slack);
    }
    let base_row =
      |source: usize| (source < rows && !self.base[source].void).then(|| self.base_row(source));
    let before = self.base_row(source);
    let now = self.head_row(index);
    if self.head[index].hash == self.base[source].hash && now == before {
      return true;
    }
    let around = |source: Option<usize>| source.filter(|_| slack).and_then(base_row);
    let (above, below) = (around(source.checked_sub(1)), around(Some(source + 1)));
    let (base, head) = aligned(input.base, input.head, offset.dy);
    let y = self.top + index.min(source);
    matches(now, before, input.max_delta, |x, pixel| {
      let near = |row: Row| within(row.get(x), pixel, input.max_delta);
      above.is_some_and(near)
        || below.is_some_and(near)
        || input.noise.at(&base, &head, self.x0 + x, y)
    })
  }

  /// `explains`, for a base row `source` whose content moved `dx` columns
  /// right as well: the pixels are read where they came from. Content that
  /// moves sideways lands at fractional columns as it does at fractional
  /// rows, so with `slack` a pixel may also match the ones just left and
  /// right of where it came from, and the noise detector judges what is
  /// left on windows of both images lined up for the move.
  fn explains_across(&self, index: usize, source: usize, dx: isize, slack: bool) -> bool {
    let rows = self.rows();
    let base = self.input.base;
    // One column more on either side, for the slack.
    let shifted = |source: usize| -> Option<Vec<u32>> {
      (source < rows && !self.base[source].void).then(|| {
        (self.x0..=self.x1 + 2)
          .map(|x| {
            x.checked_add_signed(-dx - 1)
              .map_or(0, |x| base.at(x, self.top + source))
          })
          .collect()
      })
    };
    let Some(before) = shifted(source) else {
      return false;
    };
    let around = |source: Option<usize>| source.filter(|_| slack).and_then(shifted);
    let (above, below) = (around(source.checked_sub(1)), around(Some(source + 1)));
    let max_delta = self.input.max_delta;
    // An edge drawn at a fractional column is drawn anew: its pixels blend
    // the two on either side of it in proportions of their own, and a glyph
    // rendered at another phase shifts its ink by a few levels. With slack,
    // a pixel between two neighbours of the one it came from, channel by
    // channel, is that edge, and so is one the least sensitive tier would
    // take for one of them. A flat area has no two sides: a change of colour
    // there still shows.
    let edge_delta = self.input.edge_delta.max(max_delta);
    let near = |row: &[u32], x: usize, pixel: u32| {
      if !slack {
        return within(row[x + 1], pixel, max_delta);
      }
      let around = &row[x..x + 3];
      let delta = if around.iter().all(|&other| other == around[0]) {
        max_delta
      } else {
        edge_delta
      };
      around.iter().any(|&other| within(other, pixel, delta))
        || between(pixel, around[0], around[1])
        || between(pixel, around[1], around[2])
    };
    let now = self.head_row(index);
    let before_row = Row {
      pixels: &before[1..before.len() - 1],
      zeros: 0,
    };
    let close = |x: usize, pixel: u32| {
      [Some(&before), above.as_ref(), below.as_ref()]
        .into_iter()
        .flatten()
        .any(|row| near(row, x, pixel))
    };
    if matches(now, before_row, max_delta, close) {
      return true;
    }
    // The noise detectors read the pixels around the one they judge: they
    // get both images, lined up for the move, around the two rows.
    let width = self.x1 - self.x0 + 1 + 2 * WINDOW;
    let (base, head) = (
      self.window(self.input.base, self.top + source, dx),
      self.window(self.input.head, self.top + index, 0),
    );
    let height = 2 * WINDOW + 1;
    let (base, head) = (
      View::new(&base, width, height),
      View::new(&head, width, height),
    );
    matches(now, before_row, max_delta, |x, pixel| {
      close(x, pixel) || self.input.noise.at(&base, &head, x + WINDOW, WINDOW)
    })
  }

  /// Rows `y - WINDOW..=y + WINDOW` of `view`, over the strip's columns and
  /// `WINDOW` more on either side, read `dx` columns left.
  fn window(&self, view: View, y: usize, dx: isize) -> Image {
    let width = self.x1 - self.x0 + 1 + 2 * WINDOW;
    let pixels = (0..=2 * WINDOW)
      .flat_map(|row| {
        let y = (y + row).checked_sub(WINDOW);
        (0..width).map(move |column| {
          let x = (self.x0 + column).checked_add_signed(-dx - WINDOW as isize);
          match (x, y) {
            (Some(x), Some(y)) => view.at(x, y),
            _ => 0,
          }
        })
      })
      .collect();
    Image::new(width as u32, (2 * WINDOW + 1) as u32, pixels).expect("pixels for the window")
  }

  /// Which explanations fit each head row, as a bitset: bit 0 when the row
  /// stayed in place, bit `s` when it is what became of the base row
  /// `found[s - 1].offset` rows above it, with slack. Staying in place gets
  /// none: a row that moved by one pixel did not stay.
  ///
  /// A move is only tried around the rows that voted for it, from which it
  /// spreads up and down for as long as it explains the rows it reaches:
  /// each row is tried once per move that reaches it, not once per move.
  fn explain(&self, in_place: &[bool], found: &[Displacement]) -> Vec<u32> {
    let rows = self.rows();
    let mut explained: Vec<u32> = in_place.iter().map(|&stayed| u32::from(stayed)).collect();
    for (state, found) in (1..).zip(found) {
      let bit = 1 << state;
      let fits = |index: usize| self.explains(index, found.offset, true);
      for &seed in &found.voters {
        if explained[seed] & bit != 0 || !fits(seed) {
          continue;
        }
        explained[seed] |= bit;
        for step in [-1, 1] {
          let mut index = seed;
          while let Some(next) = index.checked_add_signed(step).filter(|&next| next < rows) {
            if explained[next] & bit != 0 || !fits(next) {
              break;
            }
            explained[next] |= bit;
            index = next;
          }
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
/// new content that looks like old — and so are rows for which
/// `copies(row, source)` says the source is still where it was, unless the
/// move explains the source row too: content that repeats every so many
/// rows, moved by as many, lands on a copy of itself. A copy takes
/// `min_rows` such rows together: one row alike, a border, proves nothing.
/// What is left of the stretch needs evidence of its own.
fn keep_evidenced(
  explained: &mut [u32],
  evidence: &Evidence,
  offsets: &[isize],
  min_rows: usize,
  copies: impl Fn(usize, usize) -> bool,
) {
  let rows = explained.len();
  let fitted = explained.to_vec();
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
    let is_copy = |row: usize| copies(row, source(row)) && fitted[source(row)] & 1 << state == 0;
    let mut copied = vec![false; end - start];
    for (from, to) in runs(end - start, |at| {
      !evidence.at(start + at) || is_copy(start + at)
    }) {
      let rows: Vec<usize> = (from..to).filter(|&at| is_copy(start + at)).collect();
      if rows.len() >= min_rows {
        rows.into_iter().for_each(|at| copied[at] = true);
      }
    }
    let free: Vec<(usize, usize)> = runs(end - start, |at| {
      !claimed[source(start + at)] && !copied[at]
    })
    .collect();
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

/// `assign`, until every block of a move holds `min_rows` of evidence: one
/// the choice cuts down to less is not a block, and its rows are given out
/// again without that move. After `MAX_SOLVES` attempts, the rows of a block
/// still short are left in place where they can be, unexplained otherwise.
fn solve(
  explained: &mut [u32],
  evidence: &Evidence,
  padding: impl Fn(usize) -> bool + Copy,
  states: usize,
  min_rows: usize,
) -> Vec<usize> {
  let none = states;
  let short = |assigned: &[usize]| -> Vec<(usize, usize, usize)> {
    blocks(assigned)
      .filter(|&(state, start, end)| {
        state != 0 && state != none && evidence.within(start, end) < min_rows
      })
      .collect()
  };
  let mut assigned = assign(explained, padding, states, min_rows as u32 - 1);
  for _ in 0..MAX_SOLVES {
    let short = short(&assigned);
    if short.is_empty() {
      return assigned;
    }
    for (state, start, end) in short {
      explained[start..end]
        .iter_mut()
        .for_each(|bits| *bits &= !(1 << state));
    }
    assigned = assign(explained, padding, states, min_rows as u32 - 1);
  }
  for (_, start, end) in short(&assigned) {
    for row in start..end {
      assigned[row] = if explained[row] & 1 != 0 { 0 } else { none };
    }
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

/// The first and last rows of `rows` that differ within columns `x0..=x1`.
/// Everything outside them is identical in place, so it can neither have
/// moved nor hold a pixel to explain.
fn differing_rows(
  input: &Input,
  x0: usize,
  x1: usize,
  (first, last): (usize, usize),
) -> Option<(usize, usize)> {
  let mut differing =
    (first..=last).filter(|&y| input.base.row(y, x0, x1) != input.head.row(y, x0, x1));
  let top = differing.next()?;
  Some((top, differing.next_back().unwrap_or(top)))
}

/// The rows of a strip, told apart: what each head row is, and whether
/// each base row went anywhere.
struct Analysis {
  x0: usize,
  x1: usize,
  /// Canvas row of index 0.
  top: usize,
  /// Per row, whether the head row is accounted for: in place, moved
  /// there, padding, or more of the plain rows next to one that is.
  head: Vec<bool>,
  /// Per row, whether the base row is: still there, moved elsewhere,
  /// padding, or more of the plain rows next to one that is.
  base: Vec<bool>,
  /// Largest distance a block of the strip moved by.
  reach: usize,
  segments: Vec<ShiftSegment>,
}

impl Analysis {
  fn bottom(&self) -> usize {
    self.top + self.head.len() - 1
  }

  /// Whether the head row at canvas row `y` is accounted for. Outside the
  /// strip, rows are identical in place.
  fn head_at(&self, y: usize) -> bool {
    y.checked_sub(self.top)
      .and_then(|index| self.head.get(index))
      .is_none_or(|&accounted| accounted)
  }

  fn base_at(&self, y: usize) -> bool {
    y.checked_sub(self.top)
      .and_then(|index| self.base.get(index))
      .is_none_or(|&accounted| accounted)
  }
}

/// Mark, in `accounted`, the rows that are more of the row next to them,
/// when that row is accounted for: space that grew or shrank.
///
/// A card that grows taller stretches its background, and the page below it
/// moves: the new rows of background have no counterpart in the base image,
/// and neither have the old ones a moved block covers now. Both are more of
/// what is still there. A change is not: a new line of text, a bar of
/// another colour, differs from the rows around it, and stops the run.
fn stretch(accounted: &mut [bool], alike: impl Fn(usize, usize) -> bool) {
  let rows = accounted.len();
  let gaps: Vec<(usize, usize)> = runs(rows, |row| !accounted[row]).collect();
  for (start, end) in gaps {
    let mut row = start;
    while row > 0 && row < end && alike(row, row - 1) {
      accounted[row] = true;
      row += 1;
    }
    let mut row = end;
    while row < rows && row > start && !accounted[row - 1] && alike(row - 1, row) {
      accounted[row - 1] = true;
      row -= 1;
    }
  }
}

/// Explain the rows `rows` of columns `x0..=x1` by movement, as far as they
/// go. `None` when they hold fewer than `min_rows` differing rows.
fn analyse(input: &Input, x0: usize, x1: usize, rows: (usize, usize)) -> Option<Analysis> {
  let (top, bottom) = differing_rows(input, x0, x1, rows)?;
  if bottom - top + 1 < input.min_rows {
    return None;
  }
  // And the unchanged row on either side, for content that moved by a row
  // and a fraction to have the row next to where it came from at hand.
  let (top, bottom) = (
    top.saturating_sub(1),
    (bottom + 1).min(input.base.height - 1),
  );
  let rows = bottom - top + 1;

  let signatures = |view: &View| -> Vec<Signature> {
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
  let offsets: Vec<Offset> = std::iter::once(Offset::default())
    .chain(found.iter().map(|found| found.offset))
    .collect();
  let rises: Vec<isize> = offsets.iter().map(|offset| offset.dy).collect();
  let none = offsets.len();
  let assigned = if found.is_empty() {
    in_place
      .iter()
      .map(|&stayed| if stayed { 0 } else { none })
      .collect()
  } else {
    let mut explained = strip.explain(&in_place, &found);
    let evidence = Evidence::new(&telling);
    // A telling row with the content of a base row that is still where it
    // was is a copy of it. Plain rows fit anywhere, so their sources prove
    // nothing.
    let copies = |row: usize, source: usize| {
      telling[row] && in_place[source] && strip.head[row].content == strip.base[source].content
    };
    keep_evidenced(&mut explained, &evidence, &rises, input.min_rows, copies);

    // One move per stretch of rows: a row that several moves explain — a
    // flat one, one of a repeated pattern, one of a block that moved by 23
    // rows next to one that moved by 24 — stays with its neighbours. A block
    // is worth starting for `min_rows` rows.
    solve(
      &mut explained,
      &evidence,
      |row| strip.head[row].void,
      offsets.len(),
      input.min_rows,
    )
  };

  let mut base_used = vec![false; rows];
  let mut segments: Vec<ShiftSegment> = Vec::new();
  let mut reach = 0;
  for (state, start, end) in blocks(&assigned) {
    if state == none {
      continue;
    }
    let offset = offsets[state];
    let source = (start as isize - offset.dy) as usize;
    base_used[source..source + end - start].fill(true);
    if offset != Offset::default() {
      // A block's first row may have come from the row above its source,
      // and its last from the row below: those rows are not gone either.
      if source > 0 && !strip.explains(start, offset, false) {
        base_used[source - 1] = true;
      }
      let after = source + end - start;
      if after < rows && !strip.explains(end - 1, offset, false) {
        base_used[after] = true;
      }
      reach = reach.max(offset.dy.unsigned_abs());
      segments.push(ShiftSegment {
        x: x0 as u32,
        width: (x1 - x0 + 1) as u32,
        base_y: (top + source) as u32,
        head_y: (top + start) as u32,
        height: (end - start) as u32,
        dx: offset.dx as i64,
      });
    }
  }

  let max_delta = input.max_delta;
  let mut head: Vec<bool> = (0..rows)
    .map(|row| strip.head[row].void || assigned[row] != none)
    .collect();
  stretch(&mut head, |row, other| {
    !strip.head[row].void
      && !strip.head[other].void
      && matches(
        strip.head_row(row),
        strip.head_row(other),
        max_delta,
        |_, _| false,
      )
  });
  let mut base: Vec<bool> = (0..rows)
    .map(|row| strip.base[row].void || base_used[row])
    .collect();
  stretch(&mut base, |row, other| {
    !strip.base[row].void
      && !strip.base[other].void
      && matches(
        strip.base_row(row),
        strip.base_row(other),
        max_delta,
        |_, _| false,
      )
  });

  Some(Analysis {
    x0,
    x1,
    top,
    head,
    base,
    reach,
    segments,
  })
}

/// Clusters of `rows`, sorted, that lie within `gap` rows of each other, as
/// inclusive ranges.
fn clusters(rows: &[usize], gap: usize) -> Vec<(usize, usize)> {
  let mut clusters: Vec<(usize, usize)> = Vec::new();
  for &row in rows {
    match clusters.last_mut() {
      Some((_, last)) if row - *last <= gap + 1 => *last = row,
      _ => clusters.push((row, row)),
    }
  }
  clusters
}

/// The columns `x0..=x1` of the head image, cut down the middle of each
/// gutter between rows `first` and `last`: `STRIP_GAP` columns or more that
/// keep one colour all the way down, as the space between two cards does.
/// One range when there is no gutter, none when there are more than
/// `MAX_STRIPS` columns.
fn columns(input: &Input, x0: usize, x1: usize, first: usize, last: usize) -> Vec<(usize, usize)> {
  let width = x1 - x0 + 1;
  let top = input.head.row(first, x0, x1);
  let mut uniform = vec![true; width];
  for y in first + 1..=last {
    let row = input.head.row(y, x0, x1);
    let mut left = false;
    for (x, uniform) in uniform.iter_mut().enumerate() {
      *uniform = *uniform && within(row.get(x), top.get(x), input.max_delta);
      left |= *uniform;
    }
    if !left {
      return vec![(x0, x1)];
    }
  }
  // A card's border is a line down the middle of the gutter, which moves
  // when the cards beside it are resized: it does not split the gutter.
  let lines: Vec<(usize, usize)> = runs(width, |x| !uniform[x])
    .filter(|&(start, end)| start > 0 && end < width && end - start <= BORDER)
    .collect();
  for (start, end) in lines {
    uniform[start..end].fill(true);
  }
  let cuts: Vec<usize> = runs(width, |x| uniform[x])
    .filter(|&(start, end)| start > 0 && end < width && end - start >= STRIP_GAP)
    .map(|(start, end)| x0 + (start + end) / 2)
    .collect();
  if cuts.len() >= MAX_STRIPS {
    return Vec::new();
  }
  let starts = std::iter::once(x0).chain(cuts.iter().copied());
  let ends = cuts.iter().map(|&cut| cut - 1).chain(std::iter::once(x1));
  starts.zip(ends).collect()
}

/// One column of a cluster of rows the strip leaves unexplained, analysed
/// on its own.
struct Column {
  x0: usize,
  x1: usize,
  analysis: Option<Analysis>,
}

/// Rows `first..=last` of a strip, where it falls into columns.
struct Split {
  first: usize,
  last: usize,
  columns: Vec<Column>,
}

fn analyse_strip(input: &Input, x0: usize, x1: usize, total: &mut ShiftReport) {
  let Some(strip) = analyse(input, x0, x1, input.raw_rows) else {
    return;
  };
  let mut report = ShiftReport::default();
  let pixels = |y: usize, x0: usize, x1: usize| input.mask.count_in_row(y, x0, x1);

  // Where the strip leaves pixels of the mask unexplained, its columns may
  // have moved apart: in a row of cards, the one that grew pushes its own
  // content down, and the row of cards below it, but not the cards next to
  // it. Rows that cross all the cards match nowhere, so each column is
  // looked at on its own, around the rows left unexplained. What a column
  // finds adds to what the strip found: both hold, pixel for pixel.
  let unexplained: Vec<usize> = (strip.top..=strip.bottom())
    .filter(|&y| !(strip.head_at(y) && strip.base_at(y)) && pixels(y, x0, x1) > 0)
    .collect();
  // Where nothing moved across the strip, nothing is looked for in its
  // columns either.
  let moved = strip.reach > 0 || strip.segments.iter().any(|segment| segment.dx != 0);
  let splits: Vec<Split> = clusters(&unexplained, CLUSTER_GAP)
    .into_iter()
    .filter(|_| moved)
    .filter_map(|(first, last)| {
      let columns = columns(input, x0, x1, first, last);
      if columns.len() < 2 {
        return None;
      }
      // Far enough around the rows to find where their content came from:
      // as far as the strip saw anything move.
      let margin = strip.reach + 2;
      let window = (
        first.saturating_sub(margin).max(strip.top),
        (last + margin).min(strip.bottom()),
      );
      let columns = columns
        .into_iter()
        .filter(|&(x0, x1)| (first..=last).any(|y| pixels(y, x0, x1) > 0))
        .map(|(x0, x1)| Column {
          x0,
          x1,
          analysis: analyse(input, x0, x1, window),
        })
        .collect();
      Some(Split {
        first,
        last,
        columns,
      })
    })
    .collect();

  let split_at = |y: usize| {
    splits
      .iter()
      .any(|split| (split.first..=split.last).contains(&y))
  };
  let outside: Vec<(usize, usize)> = runs(strip.bottom() + 1 - strip.top, |row| {
    !split_at(strip.top + row)
  })
  .map(|(start, end)| (strip.top + start, strip.top + end))
  .collect();

  for segment in &strip.segments {
    for &(start, end) in &outside {
      report.segments.extend(clip(segment, start, end));
    }
  }
  for &(start, end) in &outside {
    outline(
      input,
      (strip.x0, strip.x1),
      (start, end),
      |y| strip.head_at(y),
      |y| strip.base_at(y),
      &mut report,
    );
  }
  for split in &splits {
    for column in &split.columns {
      let found = column.analysis.as_ref();
      for segment in found.iter().flat_map(|found| &found.segments) {
        report
          .segments
          .extend(clip(segment, split.first, split.last + 1));
      }
      outline(
        input,
        (column.x0, column.x1),
        (split.first, split.last + 1),
        |y| strip.head_at(y) || found.is_some_and(|found| found.head_at(y)),
        |y| strip.base_at(y) || found.is_some_and(|found| found.base_at(y)),
        &mut report,
      );
    }
  }
  // A strip where nothing moved has nothing to say.
  if !report.segments.is_empty() {
    total.segments.append(&mut report.segments);
    total.inserted.append(&mut report.inserted);
    total.removed.append(&mut report.removed);
    total.hidden.append(&mut report.hidden);
    total.shifted_pixels += report.shifted_pixels;
  }
}

/// The part of `segment` whose head rows lie in `start..end`.
fn clip(segment: &ShiftSegment, start: usize, end: usize) -> Option<ShiftSegment> {
  let from = (segment.head_y as usize).max(start);
  let to = (segment.head_y as usize + segment.height as usize).min(end);
  (from < to).then(|| ShiftSegment {
    base_y: (from as i64 - segment.dy()) as u32,
    head_y: from as u32,
    height: (to - from) as u32,
    ..*segment
  })
}

/// Report rows `start..end` of columns `x0..=x1`: the head rows nothing
/// accounts for as inserted, the base rows as removed, and the mask as
/// hidden where both are.
fn outline(
  input: &Input,
  (x0, x1): (usize, usize),
  (start, end): (usize, usize),
  head: impl Fn(usize) -> bool,
  base: impl Fn(usize) -> bool,
  report: &mut ShiftReport,
) {
  let band = |from: usize, to: usize| Rect {
    x: x0 as u32,
    y: (start + from) as u32,
    width: (x1 - x0 + 1) as u32,
    height: (to - from) as u32,
  };
  let rows = end - start;
  let pixels: Vec<u64> = (start..end)
    .map(|y| input.mask.count_in_row(y, x0, x1))
    .collect();
  report
    .inserted
    .extend(runs(rows, |row| !head(start + row)).map(|(from, to)| band(from, to)));
  report
    .removed
    .extend(runs(rows, |row| !base(start + row)).map(|(from, to)| band(from, to)));
  let hidden = |row: usize| pixels[row] == 0 || (head(start + row) && base(start + row));
  for (from, to) in runs(rows, hidden) {
    // Rows without a pixel to hide need no hiding: trimmed off the ends.
    let Some(first) = (from..to).find(|&row| pixels[row] > 0) else {
      continue;
    };
    let last = (from..to)
      .rev()
      .find(|&row| pixels[row] > 0)
      .expect("a row with pixels");
    report.hidden.push(band(first, last + 1));
    report.shifted_pixels += pixels[first..=last].iter().sum::<u64>();
  }
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
    keep_evidenced(
      &mut explained,
      &Evidence::new(&[true; 10]),
      &offsets,
      2,
      |_, _| false,
    );
    let rows_of = |state: u32| {
      explained
        .iter()
        .filter(|&&bits| bits >> state & 1 == 1)
        .count()
    };
    assert_eq!((rows_of(1), rows_of(2)), (8, 0));

    // Without rows that say where they are, a move explains nothing.
    let mut explained: Vec<u32> = (0..10).map(|row| u32::from(row >= 2) << 1).collect();
    keep_evidenced(
      &mut explained,
      &Evidence::new(&[false; 10]),
      &offsets,
      2,
      |_, _| false,
    );
    assert!(explained.iter().all(|&bits| bits == 0));

    // Rows whose source `copies` says is still where it was are not moved:
    // rows 2 and 3, from base rows 0 and 1. Rows 4 and 5 come from rows 2 and
    // 3, which the move explains as well: content that repeats.
    let mut explained: Vec<u32> = (0..10).map(|row| u32::from(row >= 2) << 1).collect();
    keep_evidenced(
      &mut explained,
      &Evidence::new(&[true; 10]),
      &offsets,
      2,
      |row, _| row < 6,
    );
    let moved: Vec<usize> = (0..10).filter(|&row| explained[row] != 0).collect();
    assert_eq!(moved, [4, 5, 6, 7, 8, 9]);
  }

  #[test]
  fn votes_share_out_exactly() {
    assert!((1..=MAX_CANDIDATES as u64).all(|places| VOTE.is_multiple_of(places)));
    // Twenty-four rows found at six places each are four votes, not
    // 3.9999999999999982 of them.
    assert_eq!((0..24).map(|_| VOTE / 6).sum::<u64>(), 4 * VOTE);
  }

  #[test]
  fn a_block_cut_short_is_given_out_again() {
    // Move 1 fits rows 0..6, move 2 rows 4..10; only rows 0..4 and 8..10
    // are telling. The choice gives rows 6..10 to move 2, which leaves it two
    // rows of evidence where it needs four: those rows go unexplained.
    let mut explained = [2, 2, 2, 2, 6, 6, 4, 4, 4, 4];
    let telling = [
      true, true, true, true, false, false, false, false, true, true,
    ];
    let evidence = Evidence::new(&telling);
    let assigned = solve(&mut explained, &evidence, |_| false, 3, 4);
    assert_eq!(assigned, [1, 1, 1, 1, 1, 1, 3, 3, 3, 3]);
    assert!(explained[6..].iter().all(|&bits| bits & 4 == 0));
  }
}
