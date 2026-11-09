//! Where the time goes for one pair of images.
//!
//!   cargo run --release -p adiff --example profile -- base.png head.png [runs]
use std::time::Instant;

use adiff::{compare, Antialiasing, Image, Options};

fn median(runs: usize, mut work: impl FnMut()) -> f64 {
  let mut times: Vec<f64> = (0..runs)
    .map(|_| {
      let start = Instant::now();
      work();
      start.elapsed().as_secs_f64() * 1e3
    })
    .collect();
  times.sort_by(f64::total_cmp);
  times[times.len() / 2]
}

fn main() {
  let args: Vec<String> = std::env::args().collect();
  let (base_path, head_path) = (&args[1], &args[2]);
  let runs = args.get(3).and_then(|runs| runs.parse().ok()).unwrap_or(7);

  let base_bytes = std::fs::read(base_path).unwrap();
  let head_bytes = std::fs::read(head_path).unwrap();
  let base = Image::decode(&base_bytes).unwrap();
  let head = Image::decode(&head_bytes).unwrap();
  println!(
    "{}x{} vs {}x{}",
    base.width(),
    base.height(),
    head.width(),
    head.height()
  );

  let with = |change: fn(&mut Options)| {
    let mut options = Options::default();
    change(&mut options);
    options
  };
  let bare = with(|options| {
    options.antialiasing = Antialiasing::Off;
    options.fingerprint = false;
    options.shifts = false;
  });
  let antialiasing = with(|options| {
    options.fingerprint = false;
    options.shifts = false;
  });
  let fingerprint = with(|options| options.shifts = false);
  let full = Options::default();

  let result = compare(&base, &head, &full).unwrap();
  println!(
    "score {:.4}, {} px, {} antialiased, tiers {:?}",
    result.score,
    result.diff_pixels,
    result.antialiased_pixels,
    result
      .tiers
      .iter()
      .map(|tier| tier.diff_pixels)
      .collect::<Vec<_>>()
  );

  let decode_base = median(runs, || drop(Image::decode(&base_bytes).unwrap()));
  let decode_head = median(runs, || drop(Image::decode(&head_bytes).unwrap()));
  let scan = median(runs, || drop(compare(&base, &head, &bare).unwrap()));
  let scan_aa = median(runs, || drop(compare(&base, &head, &antialiasing).unwrap()));
  let scan_fp = median(runs, || drop(compare(&base, &head, &fingerprint).unwrap()));
  let scan_full = median(runs, || drop(compare(&base, &head, &full).unwrap()));
  let encode = result.mask.as_ref().map(|mask| {
    (
      median(runs, || drop(mask.to_png().unwrap())),
      mask.to_png().unwrap().len(),
    )
  });

  println!("decode base        {decode_base:8.2} ms");
  println!("decode head        {decode_head:8.2} ms");
  println!("scan               {scan:8.2} ms");
  println!("  + anti-aliasing  {:8.2} ms", scan_aa - scan);
  println!("  + fingerprint    {:8.2} ms", scan_fp - scan_aa);
  println!("  + shifts         {:8.2} ms", scan_full - scan_fp);
  if let Some((time, size)) = encode {
    println!("encode mask        {time:8.2} ms ({size} bytes)");
  }
}
