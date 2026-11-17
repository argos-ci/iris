//! Layout shifts on synthetic pages: rows of "text" that can be inserted,
//! removed and moved, so each scenario knows exactly what moved where.

mod support;

use iris::{compare, Antialiasing, Comparison, Image, Options, Rect, ShiftSegment};

const WHITE: [u8; 4] = [255, 255, 255, 255];
const INK: [u8; 4] = [20, 24, 40, 255];

type Row = Vec<[u8; 4]>;

/// One line of fake text: twelve rows of glyph-like marks, six of leading.
/// Lines differ by `seed`, so no two look alike.
fn text_line(width: usize, seed: u64) -> Vec<Row> {
  (0..18u64)
    .map(|y| {
      (0..width as u64)
        .map(|x| {
          let mark = (seed.wrapping_mul(0x9e3779b97f4a7c15)
            ^ (x / 3).wrapping_mul(0xc2b2ae3d27d4eb4f)
            ^ y.wrapping_mul(0x165667b19e3779f9))
          .wrapping_mul(0xff51afd7ed558ccd)
            >> 61;
          if y < 12 && mark < 3 {
            INK
          } else {
            WHITE
          }
        })
        .collect()
    })
    .collect()
}

fn text(width: usize, lines: std::ops::Range<u64>) -> Vec<Row> {
  lines.flat_map(|seed| text_line(width, seed)).collect()
}

fn block(width: usize, height: usize, color: [u8; 4]) -> Vec<Row> {
  vec![vec![color; width]; height]
}

fn page(rows: &[Row]) -> Image {
  let flat: Vec<u8> = rows.iter().flatten().flatten().copied().collect();
  Image::from_rgba8(rows[0].len() as u32, rows.len() as u32, &flat).unwrap()
}

/// Put `left` and `right` side by side, `gutter` white columns apart.
fn beside(left: &[Row], gutter: usize, right: &[Row]) -> Vec<Row> {
  left
    .iter()
    .zip(right)
    .map(|(left, right)| {
      let mut row = left.clone();
      row.extend(std::iter::repeat_n(WHITE, gutter));
      row.extend_from_slice(right);
      row
    })
    .collect()
}

fn diff(base: &[Row], head: &[Row]) -> Comparison {
  compare(&page(base), &page(head), &Options::default()).unwrap()
}

fn segment(x: u32, width: u32, base_y: u32, head_y: u32, height: u32) -> ShiftSegment {
  ShiftSegment {
    x,
    width,
    base_y,
    head_y,
    height,
  }
}

fn band(x: u32, y: u32, width: u32, height: u32) -> Rect {
  Rect {
    x,
    y,
    width,
    height,
  }
}

#[test]
fn an_inserted_block_pushes_the_page_down() {
  let width = 200;
  let base = text(width, 0..20);
  let mut head = base.clone();
  // A 30 px banner after the fifth line.
  head.splice(90..90, block(width, 30, [200, 30, 30, 255]));

  let result = diff(&base, &head);
  let shift = result.shift.expect("a shift");

  // Fifteen lines moved down by thirty rows; the leading above the banner did
  // not change, so the difference starts at the banner.
  assert_eq!(shift.segments, [segment(0, 200, 90, 120, 270)]);
  assert_eq!(shift.segments[0].dy(), 30);
  assert_eq!(shift.inserted, [band(0, 90, 200, 30)]);
  assert_eq!(shift.removed, []);

  // Almost all of the diff is movement: what is left is the banner.
  let banner = 30 * 200;
  assert_eq!(result.diff_pixels - shift.shifted_pixels, banner);
  assert!(
    shift.shifted_pixels > 3 * banner,
    "{}",
    shift.shifted_pixels
  );
}

#[test]
fn a_removed_block_pulls_the_page_up() {
  let width = 160;
  let base = text(width, 0..16);
  let mut head = base.clone();
  // The third and fourth lines go.
  head.drain(36..72);

  let result = diff(&base, &head);
  let shift = result.shift.expect("a shift");

  assert_eq!((result.width, result.height), (160, 288));
  assert_eq!(shift.segments, [segment(0, 160, 72, 36, 216)]);
  assert_eq!(shift.segments[0].dy(), -36);
  assert_eq!(shift.inserted, []);
  // What was removed is reported where it used to be.
  assert_eq!(shift.removed, [band(0, 36, 160, 36)]);
  assert!(shift.shifted_pixels < result.diff_pixels);
  assert!(shift.shifted_pixels * 10 > result.diff_pixels * 8);
}

#[test]
fn a_sidebar_that_stays_put_does_not_hide_the_shift() {
  // 60 px of navigation that does not move, a 40 px gutter, then content
  // that does.
  let sidebar = text(60, 100..112);
  let content = text(200, 0..12);
  let mut moved = content.clone();
  moved.splice(54..54, block(200, 24, [30, 90, 200, 255]));
  moved.truncate(content.len());

  let base = beside(&sidebar, 40, &content);
  let head = beside(&sidebar, 40, &moved);
  let shift = diff(&base, &head).shift.expect("a shift");

  // The strip is the content column only.
  assert_eq!(shift.segments, [segment(100, 200, 54, 78, 138)]);
  assert_eq!(shift.inserted, [band(100, 54, 200, 24)]);
}

#[test]
fn a_change_inside_moved_content_stands_out() {
  let width = 180;
  let base = text(width, 0..14);
  let mut head = base.clone();
  // The page moves down twelve rows, and the ninth line is reworded.
  head.splice(144..162, text_line(width, 999));
  head.splice(36..36, block(width, 12, [240, 200, 40, 255]));

  let shift = diff(&base, &head).shift.expect("a shift");
  assert_eq!(
    shift.segments,
    [
      segment(0, 180, 36, 48, 108),
      // The reworded line splits the block; its leading still lines up.
      segment(0, 180, 156, 168, 96),
    ]
  );
  assert!(shift.segments.iter().all(|segment| segment.dy() == 12));
  // The banner, then the twelve glyph rows of the new line.
  assert_eq!(
    shift.inserted,
    [band(0, 36, 180, 12), band(0, 156, 180, 12)]
  );
  assert_eq!(shift.removed, [band(0, 144, 180, 12)]);
}

#[test]
fn a_change_in_place_is_not_a_shift() {
  let width = 150;
  let base = text(width, 0..10);
  let mut head = base.clone();
  // Three lines reworded where they stand.
  head.splice(54..108, text(width, 500..503));

  let result = diff(&base, &head);
  assert!(result.score > 0.0);
  assert_eq!(result.shift, None);
}

#[test]
fn a_recolour_is_not_a_shift() {
  let base = [
    block(120, 40, WHITE),
    block(120, 60, [30, 90, 200, 255]),
    block(120, 40, WHITE),
  ]
  .concat();
  let head = [
    block(120, 40, WHITE),
    block(120, 60, [200, 60, 30, 255]),
    block(120, 40, WHITE),
  ]
  .concat();
  let result = diff(&base, &head);
  assert!(result.score > 0.0);
  assert_eq!(result.shift, None);
}

#[test]
fn flat_areas_alone_cannot_anchor_a_shift() {
  // A solid block moves down. Every row of it is one colour: there is
  // nothing to tell "moved" from "resized", so nothing is claimed.
  let base = [
    block(100, 20, WHITE),
    block(100, 30, INK),
    block(100, 70, WHITE),
  ]
  .concat();
  let head = [
    block(100, 40, WHITE),
    block(100, 30, INK),
    block(100, 50, WHITE),
  ]
  .concat();
  let result = diff(&base, &head);
  assert!(result.score > 0.0);
  assert_eq!(result.shift, None);
}

#[test]
fn a_couple_of_matching_rows_are_a_coincidence() {
  let width = 140;
  let base = text(width, 0..8);
  let mut head = base.clone();
  // Three rows of the second line reappear lower down, in a reworded line.
  let borrowed: Vec<Row> = base[20..23].to_vec();
  head.splice(90..108, text_line(width, 777));
  head.splice(94..97, borrowed);

  let result = diff(&base, &head);
  assert!(result.score > 0.0);
  assert_eq!(result.shift, None);
}

#[test]
fn shift_detection_can_be_turned_off() {
  let base = text(120, 0..10);
  let mut head = base.clone();
  head.splice(36..36, block(120, 20, INK));
  let options = Options {
    shifts: false,
    ..Options::default()
  };
  let result = compare(&page(&base), &page(&head), &options).unwrap();
  assert!(result.score > 0.0);
  assert_eq!(result.shift, None);
}

#[test]
fn the_minimum_shift_height_is_configurable() {
  let base = text(120, 0..6);
  let mut head = base.clone();
  head.splice(36..36, block(120, 20, INK));
  let options = Options {
    // Taller than anything on this page.
    min_shift_rows: 500,
    ..Options::default()
  };
  let result = compare(&page(&base), &page(&head), &options).unwrap();
  assert_eq!(result.shift, None);
}

const BANNER: [u8; 4] = [200, 30, 30, 255];

#[test]
fn content_moved_by_a_fraction_of_a_row_is_one_shift() {
  // A banner 23.5 rows tall: the browser snaps the lines below it 23 or 24
  // rows down, each on its own. The first seven land 23 rows down, the rest
  // 24, with a row of leading more between the two.
  let width = 200;
  let base = text(width, 0..20);
  let head = [
    &base[..90],
    &block(width, 23, BANNER),
    &base[90..216],
    &block(width, 1, WHITE),
    &base[216..],
  ]
  .concat();

  // Whichever detector leaves noise out of the mask.
  for antialiasing in [
    Antialiasing::Off,
    Antialiasing::Classic,
    Antialiasing::Subpixel,
  ] {
    let options = Options {
      antialiasing,
      ..Options::default()
    };
    let result = compare(&page(&base), &page(&head), &options).unwrap();
    let shift = result.shift.expect("a shift");

    // One block, which moved by 23 rows and a half: no seam where the two
    // halves meet.
    assert_eq!(shift.segments.len(), 1, "{antialiasing:?}: {shift:?}");
    let segment = shift.segments[0];
    assert!([23, 24].contains(&segment.dy()), "{segment:?}");
    assert_eq!(
      (segment.head_y, segment.head_y + segment.height),
      (113, 384)
    );
    assert_eq!(shift.inserted, [band(0, 90, 200, 23)]);
    assert_eq!(shift.removed, []);
    assert_eq!(result.diff_pixels - shift.shifted_pixels, 23 * 200);
  }
}

#[test]
fn an_edge_rendered_anew_where_it_moved_to_is_noise() {
  // A rule down the page, between two columns of text, that starts at a
  // fraction of a row: where the page moved to, its first pixel is a blend
  // rather than ink. That pixel matches nothing it could have come from,
  // but it is anti-aliasing, as the mask would have it.
  let width = 160;
  let mut base = text(width, 0..12);
  for row in &mut base[100..160] {
    row[80] = INK;
  }
  let mut head = base.clone();
  head.splice(36..36, block(width, 12, BANNER));
  head[112][80] = [128, 130, 138, 255];

  let result = diff(&base, &head);
  let shift = result.shift.expect("a shift");
  assert_eq!(shift.segments, [segment(0, 160, 36, 48, 180)]);
  assert_eq!(shift.inserted, [band(0, 36, 160, 12)]);
}

#[test]
fn a_copy_of_content_still_in_place_is_inserted() {
  // The fourth line is reworded where it stands, and further down a copy
  // of the sixth line goes in, pushing the rest down. The sixth line has not
  // moved: the copy is new, not moved from it.
  let width = 160;
  let base = text(width, 0..16);
  let mut head = base.clone();
  head.splice(54..72, text_line(width, 900));
  head.splice(162..162, base[90..108].to_vec());

  let shift = diff(&base, &head).shift.expect("a shift");
  // The lines below the copy moved down, with the copy's blank leading,
  // which fits there as well as anywhere.
  assert_eq!(shift.segments, [segment(0, 160, 156, 174, 132)]);
  // The reworded line's glyphs and the copy's.
  assert_eq!(
    shift.inserted,
    [band(0, 54, 160, 12), band(0, 162, 160, 12)]
  );
  assert_eq!(shift.removed, [band(0, 54, 160, 12)]);
}

/// Rows of text over a light grey that alternates by one level from row to
/// row, the way a browser dithers a gradient: the pattern belongs to the
/// screen, so it does not move with the page. `first_row` is where the rows
/// sit on the screen.
fn dithered(rows: &[Row], first_row: usize) -> Vec<Row> {
  rows
    .iter()
    .enumerate()
    .map(|(y, row)| {
      let level = 240 + ((first_row + y) % 2) as u8;
      row
        .iter()
        .map(|&pixel| {
          if pixel == WHITE {
            [level, level, level, 255]
          } else {
            pixel
          }
        })
        .collect()
    })
    .collect()
}

#[test]
fn content_rendered_anew_where_it_moved_to_is_found() {
  // A section of text on white, then one on a dithered grey. A banner with
  // an odd number of rows pushes both down: the white section lands as it
  // was, but no row of the grey one does — its background is dithered the
  // other way round. Moving explains it all the same.
  let width = 180;
  let plain = text(width, 0..6);
  let shaded = text(width, 6..16);
  let base = [plain.clone(), dithered(&shaded, 108)].concat();
  let head = [
    &plain[..36],
    &block(width, 13, BANNER),
    &plain[36..],
    &dithered(&shaded, 121),
  ]
  .concat();

  let result = diff(&base, &head);
  let shift = result.shift.expect("a shift");
  assert_eq!(shift.segments, [segment(0, 180, 36, 49, 252)]);
  assert_eq!(shift.inserted, [band(0, 36, 180, 13)]);
  assert_eq!(shift.removed, []);
  assert_eq!(result.diff_pixels - shift.shifted_pixels, 13 * 180);
}

#[test]
fn new_content_that_looks_like_old_is_inserted() {
  // A copy of the twelfth line goes in after the third. The twelfth line
  // itself is still there, eighteen rows lower: the copy is new, however
  // alike the two are.
  let width = 160;
  let base = text(width, 0..16);
  let mut head = base.clone();
  head.splice(54..54, base[198..216].to_vec());

  let shift = diff(&base, &head).shift.expect("a shift");
  assert_eq!(shift.segments, [segment(0, 160, 54, 72, 234)]);
  // The copy's twelve rows of glyphs. Its leading is blank, as was what
  // stood there before: that stayed in place.
  assert_eq!(shift.inserted, [band(0, 54, 160, 12)]);
  assert_eq!(shift.removed, []);
}

/// Rows framed by a border down each side, as in a card.
fn framed(rows: &[Row]) -> Vec<Row> {
  rows
    .iter()
    .map(|row| {
      let mut row = row.clone();
      let last = row.len() - 1;
      (row[0], row[last]) = (INK, INK);
      row
    })
    .collect()
}

#[test]
fn rows_with_nothing_to_tell_do_not_make_a_shift() {
  // A card of three lines of text over forty blank rows. The text is
  // reworded into two lines, so the blank rows start higher. Each of them
  // equals every other one, border for border: they fit anywhere, and say
  // nothing about where they came from.
  let width = 150;
  let base = framed(&[text(width, 0..3), block(width, 40, WHITE)].concat());
  let head = framed(&[text(width, 100..102), block(width, 58, WHITE)].concat());

  let result = diff(&base, &head);
  assert!(result.score > 0.0);
  assert_eq!(result.shift, None);
}
