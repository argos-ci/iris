//! How much of each diff layout-shift detection explains.
//!
//!   cargo run --release -p iris --example shifts -- DIR [-v]
//!
//! `DIR` holds pairs named `NAME.base.png` and `NAME.head.png`. For each pair
//! it prints the differing pixels, the share of them explained by movement,
//! and the blocks found; `-v` lists every block.
use std::path::Path;
use std::time::Instant;

use iris::{compare_files, Options};

fn main() {
  let args: Vec<String> = std::env::args().collect();
  let directory = Path::new(&args[1]);
  let verbose = args.iter().any(|arg| arg == "-v");

  let mut names: Vec<String> = std::fs::read_dir(directory)
    .unwrap()
    .filter_map(|entry| {
      let name = entry.unwrap().file_name().into_string().unwrap();
      name.strip_suffix(".base.png").map(str::to_owned)
    })
    .collect();
  names.sort();

  println!(
    "{:<28} {:>11} {:>9} {:>7} {:>5} {:>4} {:>4} {:>7}",
    "pair", "size", "differs", "moved", "segs", "ins", "rem", "ms"
  );
  for name in names {
    let base = directory.join(format!("{name}.base.png"));
    let head = directory.join(format!("{name}.head.png"));
    let start = Instant::now();
    let result = compare_files(&base, &head, &Options::default()).unwrap();
    let elapsed = start.elapsed().as_secs_f64() * 1e3;
    let (moved, segments, inserted, removed) = match &result.shift {
      Some(shift) => (
        shift.shifted_pixels,
        shift.segments.len(),
        shift.inserted.len(),
        shift.removed.len(),
      ),
      None => (0, 0, 0, 0),
    };
    let share = if result.diff_pixels == 0 {
      0.0
    } else {
      moved as f64 / result.diff_pixels as f64
    };
    println!(
      "{:<28} {:>11} {:>9} {:>6.1}% {:>5} {:>4} {:>4} {:>7.1}",
      name,
      format!("{}x{}", result.width, result.height),
      result.diff_pixels,
      share * 100.0,
      segments,
      inserted,
      removed,
      elapsed
    );
    if verbose {
      if let Some(shift) = &result.shift {
        for segment in &shift.segments {
          println!(
            "    x {:>4}+{:<4} base {:>5} head {:>5} height {:>5} dy {:>+6} dx {:>+5}",
            segment.x,
            segment.width,
            segment.base_y,
            segment.head_y,
            segment.height,
            segment.dy(),
            segment.dx
          );
        }
        for rect in &shift.inserted {
          println!(
            "    inserted x {:>4}+{:<4} y {:>5} height {:>5}",
            rect.x, rect.width, rect.y, rect.height
          );
        }
        for rect in &shift.removed {
          println!(
            "    removed  x {:>4}+{:<4} y {:>5} height {:>5}",
            rect.x, rect.width, rect.y, rect.height
          );
        }
      }
    }
  }
}
