//! The fingerprint algorithm of `@argos-ci/mask-fingerprint` 0.1.3, kept as
//! the oracle the in-memory implementation is tested against. This is that
//! crate's `src/lib.rs` with the N-API glue removed and nothing else changed:
//! it decodes the PNG twice, row by row, exactly as the published package
//! does. Do not "fix" it — its quirks are the specification.

#![allow(dead_code, clippy::all)]

use std::io::{BufRead, BufReader, Cursor, Seek};

type Error = String;
type Result<T> = std::result::Result<T, Error>;

/// Fingerprint of a PNG mask, with the package's default options.
pub fn fingerprint(png: &[u8]) -> Result<String> {
  fingerprint_two_pass_from_buffer(png, &EqualityFingerprintOptions::default())
}

#[derive(Clone, Copy, Debug)]
struct RedThreshold {
  r_min: u8,
  g_max: u8,
  b_max: u8,
  a_min: u8,
}

#[derive(Clone, Debug)]
struct EqualityFingerprintOptions {
  red_threshold: RedThreshold,
  dilate_radius: u8, // 0 or 1
  grid_size: usize,  // 8, 16, 32
  density_thresholds: Vec<f32>,
  pad_to_square: bool,
  max_bytes: usize,
}

impl Default for EqualityFingerprintOptions {
  fn default() -> Self {
    Self {
      red_threshold: RedThreshold {
        r_min: 200,
        g_max: 90,
        b_max: 90,
        a_min: 16,
      },
      dilate_radius: 1,
      grid_size: 16,
      density_thresholds: vec![0.002, 0.02, 0.08],
      pad_to_square: true,
      max_bytes: 200 * 1024 * 1024,
    }
  }
}

fn ensure_within_budget(width: usize, height: usize, max_bytes: usize) -> Result<()> {
  // Streaming path allocates O(width) plus tiny grids
  // Still protect against pathological widths and overflows

  let row_rgba = width
    .checked_mul(4)
    .ok_or_else(|| Error::from("Row length overflow"))?;

  let row_masks = width
    .checked_mul(3)
    .ok_or_else(|| Error::from("Row length overflow"))?;

  let scratch = row_rgba
    .checked_add(row_masks)
    .ok_or_else(|| Error::from("Size overflow"))?;

  // Add a small fixed overhead
  let estimated = scratch.saturating_add(64 * 1024);

  // Also reject obviously absurd pixel counts to avoid long runtimes
  let _pixels = width.saturating_mul(height);

  if estimated > max_bytes {
    return Err(Error::from(format!(
      "Budget too small for image width: width {width}, estimated {estimated} bytes, max {max_bytes} bytes"
    )));
  }

  Ok(())
}

fn open_png_reader_from_buffer(buf: &[u8]) -> Result<png::Reader<BufReader<Cursor<&[u8]>>>> {
  let cursor = Cursor::new(buf);
  let reader = BufReader::new(cursor);

  let mut decoder = png::Decoder::new(reader);
  decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);

  decoder
    .read_info()
    .map_err(|e| Error::from(format!("PNG decode error: {e}")))
}

fn read_next_rgba_row<R: BufRead + Seek>(
  png: &mut png::Reader<R>,
  row_out: &mut [u8],
  width: usize,
) -> Result<bool> {
  let row = match png.next_row() {
    Ok(Some(row)) => row,
    Ok(None) => return Ok(false),
    Err(err) => {
      if is_unexpected_eof_decode_error(&err) {
        return Ok(false);
      }
      return Err(Error::from(format!("PNG decode error: {err}")));
    }
  };

  let data = row.data();

  let expected = width
    .checked_mul(4)
    .ok_or_else(|| Error::from("Row length overflow"))?;

  if data.len() != expected || row_out.len() != expected {
    return Err(Error::from(format!(
      "Unexpected row size: got {}, expected {}",
      data.len(),
      expected
    )));
  }

  row_out.copy_from_slice(data);
  Ok(true)
}

fn is_unexpected_eof_decode_error(err: &png::DecodingError) -> bool {
  match err {
    png::DecodingError::IoError(io) => io.kind() == std::io::ErrorKind::UnexpectedEof,
    _ => err
      .to_string()
      .to_lowercase()
      .contains("unexpected end of file"),
  }
}

fn fill_mask_row(rgba_row: &[u8], mask_out: &mut [u8], t: RedThreshold) {
  for (dst, px) in mask_out.iter_mut().zip(rgba_row.chunks_exact(4)) {
    let r = px[0];
    let g = px[1];
    let b = px[2];
    let a = px[3];

    *dst = if r >= t.r_min && g <= t.g_max && b <= t.b_max && a >= t.a_min {
      1
    } else {
      0
    };
  }
}

#[derive(Clone, Copy, Debug)]
struct Bbox {
  x: usize,
  y: usize,
  width: usize,
  height: usize,
}

#[derive(Clone, Copy, Debug)]
struct BboxState {
  min_x: usize,
  min_y: usize,
  max_x: isize,
  max_y: isize,
}

impl BboxState {
  fn new(width: usize, height: usize) -> Self {
    Self {
      min_x: width,
      min_y: height,
      max_x: -1,
      max_y: -1,
    }
  }

  fn update(&mut self, x: usize, y: usize) {
    if x < self.min_x {
      self.min_x = x;
    }
    if y < self.min_y {
      self.min_y = y;
    }
    if (x as isize) > self.max_x {
      self.max_x = x as isize;
    }
    if (y as isize) > self.max_y {
      self.max_y = y as isize;
    }
  }

  fn into_bbox(self) -> Option<Bbox> {
    if self.max_x < self.min_x as isize || self.max_y < self.min_y as isize {
      return None;
    }

    Some(Bbox {
      x: self.min_x,
      y: self.min_y,
      width: (self.max_x as usize) - self.min_x + 1,
      height: (self.max_y as usize) - self.min_y + 1,
    })
  }
}

#[derive(Clone, Copy)]
struct BboxUpdateCtx {
  y: usize,
  width: usize,
  dilate_radius: u8,
}

#[derive(Clone, Copy)]
struct AccumulateCtx<'a> {
  bbox: &'a Bbox,
  off_x: usize,
  off_y: usize,
  norm_w: usize,
  norm_h: usize,
  grid_w: usize,
  grid_h: usize,
  dilate_radius: u8,
}

fn fingerprint_two_pass_from_buffer(
  buf: &[u8],
  opts: &EqualityFingerprintOptions,
) -> Result<String> {
  let bbox = compute_bbox_streaming_from_buffer(buf, opts)?;

  let Some(bbox) = bbox else {
    return Ok("empty".to_string());
  };

  let hash = accumulate_grid_streaming_from_buffer(buf, opts, &bbox)?;
  Ok(format_fingerprint(opts, hash))
}

fn format_fingerprint(opts: &EqualityFingerprintOptions, h: u64) -> String {
  let t_joined = opts
    .density_thresholds
    .iter()
    .map(|v| format!("{v}"))
    .collect::<Vec<_>>()
    .join(",");

  format!(
    "v1:g{}:d{}:t{}:{:016x}",
    opts.grid_size, opts.dilate_radius, t_joined, h
  )
}

/* ------------------------- pass 1: bbox streaming ------------------------- */

fn compute_bbox_streaming_from_buffer(
  buf: &[u8],
  opts: &EqualityFingerprintOptions,
) -> Result<Option<Bbox>> {
  let mut png = open_png_reader_from_buffer(buf)?;
  compute_bbox_streaming(&mut png, opts)
}

fn compute_bbox_streaming<R: BufRead + Seek>(
  png: &mut png::Reader<R>,
  opts: &EqualityFingerprintOptions,
) -> Result<Option<Bbox>> {
  let width = png.info().width as usize;
  let height = png.info().height as usize;

  ensure_within_budget(width, height, opts.max_bytes)?;

  let row_len = width
    .checked_mul(4)
    .ok_or_else(|| Error::from("Row length overflow"))?;

  let mut rgba_row = vec![0u8; row_len];

  let mut prev = vec![0u8; width];
  let mut curr = vec![0u8; width];
  let mut next = vec![0u8; width];

  let mut have_prev = false;
  let mut have_curr = false;

  let mut bbox_state = BboxState::new(width, height);

  let mut y = 0usize;

  while y < height {
    if !read_next_rgba_row(png, &mut rgba_row, width)? {
      break;
    }
    fill_mask_row(&rgba_row, &mut next, opts.red_threshold);

    if !have_curr {
      curr.copy_from_slice(&next);
      have_curr = true;
      y += 1;
      continue;
    }

    if !have_prev {
      prev.copy_from_slice(&curr);
      curr.copy_from_slice(&next);
      have_prev = true;
      y += 1;
      continue;
    }

    let ctx = BboxUpdateCtx {
      y: y - 1,
      width,
      dilate_radius: opts.dilate_radius,
    };

    update_bbox_from_dilated_row(&prev, &curr, &next, ctx, &mut bbox_state);

    prev.copy_from_slice(&curr);
    curr.copy_from_slice(&next);
    y += 1;
  }

  if have_curr {
    if have_prev {
      if y >= 2 {
        let ctx = BboxUpdateCtx {
          y: y - 2,
          width,
          dilate_radius: opts.dilate_radius,
        };
        update_bbox_from_dilated_row(&prev, &curr, &next, ctx, &mut bbox_state);
      }

      if y >= 1 {
        let ctx = BboxUpdateCtx {
          y: y - 1,
          width,
          dilate_radius: opts.dilate_radius,
        };
        update_bbox_from_dilated_row(&curr, &next, &next, ctx, &mut bbox_state);
      }
    } else {
      let ctx = BboxUpdateCtx {
        y: 0,
        width,
        dilate_radius: opts.dilate_radius,
      };
      update_bbox_from_dilated_row(&curr, &curr, &curr, ctx, &mut bbox_state);
    }
  }

  Ok(bbox_state.into_bbox())
}

fn update_bbox_from_dilated_row(
  prev: &[u8],
  curr: &[u8],
  next: &[u8],
  ctx: BboxUpdateCtx,
  bbox: &mut BboxState,
) {
  if ctx.dilate_radius == 0 {
    for (x, &v) in curr.iter().enumerate() {
      if v == 1 {
        bbox.update(x, ctx.y);
      }
    }
    return;
  }

  for (x, _) in curr.iter().enumerate() {
    let x0 = x.saturating_sub(1);
    let x1 = (x + 1).min(ctx.width - 1);

    let mut on = false;
    for xx in x0..=x1 {
      if prev[xx] == 1 || curr[xx] == 1 || next[xx] == 1 {
        on = true;
        break;
      }
    }

    if on {
      bbox.update(x, ctx.y);
    }
  }
}

/* --------------------- pass 2: accumulate grid streaming --------------------- */

fn accumulate_grid_streaming_from_buffer(
  buf: &[u8],
  opts: &EqualityFingerprintOptions,
  bbox: &Bbox,
) -> Result<u64> {
  let mut png = open_png_reader_from_buffer(buf)?;
  accumulate_grid_streaming(&mut png, opts, bbox)
}

fn accumulate_grid_streaming<R: BufRead + Seek>(
  png: &mut png::Reader<R>,
  opts: &EqualityFingerprintOptions,
  bbox: &Bbox,
) -> Result<u64> {
  let width = png.info().width as usize;
  let height = png.info().height as usize;

  let row_len = width
    .checked_mul(4)
    .ok_or_else(|| Error::from("Row length overflow"))?;

  let mut rgba_row = vec![0u8; row_len];

  let mut prev = vec![0u8; width];
  let mut curr = vec![0u8; width];
  let mut next = vec![0u8; width];

  let mut have_prev = false;
  let mut have_curr = false;

  let side = if opts.pad_to_square {
    bbox.width.max(bbox.height)
  } else {
    // For compatibility, treat the cropped rectangle as the working surface
    // norm_w and norm_h are still used to compute cell areas
    bbox.width.max(1).max(bbox.height.max(1))
  };

  let (off_x, off_y, norm_w, norm_h) = if opts.pad_to_square {
    (
      (side - bbox.width) / 2,
      (side - bbox.height) / 2,
      side,
      side,
    )
  } else {
    (0, 0, bbox.width.max(1), bbox.height.max(1))
  };

  let grid_w = opts.grid_size;
  let grid_h = opts.grid_size;

  let mut counts = vec![0u32; grid_w * grid_h];

  let ctx = AccumulateCtx {
    bbox,
    off_x,
    off_y,
    norm_w,
    norm_h,
    grid_w,
    grid_h,
    dilate_radius: opts.dilate_radius,
  };

  let mut y = 0usize;

  while y < height {
    if !read_next_rgba_row(png, &mut rgba_row, width)? {
      break;
    }
    fill_mask_row(&rgba_row, &mut next, opts.red_threshold);

    if !have_curr {
      curr.copy_from_slice(&next);
      have_curr = true;
      y += 1;
      continue;
    }

    if !have_prev {
      prev.copy_from_slice(&curr);
      curr.copy_from_slice(&next);
      have_prev = true;
      y += 1;
      continue;
    }

    accumulate_from_dilated_row(&prev, &curr, &next, y - 1, ctx, &mut counts);

    prev.copy_from_slice(&curr);
    curr.copy_from_slice(&next);
    y += 1;
  }

  if y >= 2 {
    accumulate_from_dilated_row(&prev, &curr, &next, y - 2, ctx, &mut counts);
  }

  if y >= 1 {
    accumulate_from_dilated_row(&curr, &next, &next, y - 1, ctx, &mut counts);
  }

  Ok(hash_counts_density(
    &counts,
    norm_w,
    norm_h,
    grid_w,
    grid_h,
    &opts.density_thresholds,
  ))
}

fn accumulate_from_dilated_row(
  prev: &[u8],
  curr: &[u8],
  next: &[u8],
  y: usize,
  ctx: AccumulateCtx<'_>,
  counts: &mut [u32],
) {
  let bbox = ctx.bbox;

  if y < bbox.y || y >= bbox.y + bbox.height {
    return;
  }

  let x_start = bbox.x;
  let x_end = bbox.x + bbox.width;

  if ctx.dilate_radius == 0 {
    for (x, &v) in curr.iter().enumerate().take(x_end).skip(x_start) {
      if v == 1 {
        accumulate_pixel(x, y, ctx, counts);
      }
    }
    return;
  }

  let width = curr.len();

  for (x, _) in curr.iter().enumerate().take(x_end).skip(x_start) {
    let x0 = x.saturating_sub(1);
    let x1 = (x + 1).min(width - 1);

    let mut on = false;
    for xx in x0..=x1 {
      if prev[xx] == 1 || curr[xx] == 1 || next[xx] == 1 {
        on = true;
        break;
      }
    }

    if on {
      accumulate_pixel(x, y, ctx, counts);
    }
  }
}

fn accumulate_pixel(x: usize, y: usize, ctx: AccumulateCtx<'_>, counts: &mut [u32]) {
  let bbox = ctx.bbox;

  let nx = (x - bbox.x) + ctx.off_x;
  let ny = (y - bbox.y) + ctx.off_y;

  let gx = (nx * ctx.grid_w) / ctx.norm_w.max(1);
  let gy = (ny * ctx.grid_h) / ctx.norm_h.max(1);

  let gx = gx.min(ctx.grid_w - 1);
  let gy = gy.min(ctx.grid_h - 1);

  counts[gy * ctx.grid_w + gx] += 1;
}

/* -------------------------------- hashing ---------------------------------- */

fn hash_counts_density(
  counts: &[u32],
  src_w: usize,
  src_h: usize,
  grid_w: usize,
  grid_h: usize,
  thresholds: &[f32],
) -> u64 {
  let mut hash: u64 = 0xcbf29ce484222325;
  let prime: u64 = 0x00000100000001b3;

  let mut packed_byte: u8 = 0;
  let mut byte_shift: u8 = 0;

  for gy in 0..grid_h {
    let y0 = (gy * src_h) / grid_h;
    let y1 = ((gy + 1) * src_h) / grid_h;
    let cell_h = (y1.saturating_sub(y0)).max(1);

    for gx in 0..grid_w {
      let x0 = (gx * src_w) / grid_w;
      let x1 = ((gx + 1) * src_w) / grid_w;
      let cell_w = (x1.saturating_sub(x0)).max(1);

      let area = (cell_w * cell_h) as f32;
      let sum = counts[gy * grid_w + gx] as f32;
      let density = sum / area;

      let v = quantize_density(density, thresholds) & 3;
      packed_byte |= v << byte_shift;

      if byte_shift == 6 {
        hash ^= packed_byte as u64;
        hash = hash.wrapping_mul(prime);
        packed_byte = 0;
        byte_shift = 0;
      } else {
        byte_shift += 2;
      }
    }
  }

  if byte_shift != 0 {
    hash ^= packed_byte as u64;
    hash = hash.wrapping_mul(prime);
  }

  hash
}

fn quantize_density(density: f32, thresholds: &[f32]) -> u8 {
  for (i, t) in thresholds.iter().enumerate() {
    if density < *t {
      return i as u8;
    }
  }
  thresholds.len() as u8
}
