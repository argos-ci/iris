# @argos-ci/iris

Screenshot comparison in a single pass, written in Rust, for Node.js.

Two images go in. Out come a score, the mask of the pixels that differ, a
fingerprint that is the same for diffs that look alike, and a report of the
content that moved rather than changed.

It replaces a pipeline of three tools — `sharp` to pad the images to a common
size, `odiff` run twice at two thresholds, and `@argos-ci/mask-fingerprint` on
the mask one of the two runs wrote — that decoded each screenshot at least
twice and the mask twice more. Here each image is decoded once, on its own thread; every
threshold is evaluated in the same scan; anti-aliasing is decided once per
pixel; and the fingerprint is computed from the mask in memory.

```ts
import { diff } from "@argos-ci/iris";

const result = await diff("base.png", "head.png", { output: "mask.png" });

result.score; // 0.0421 — share of the image that differs, 0 when nothing does
result.fingerprint; // "v1:g16:d1:t0.002,0.02,0.08:ccb73aa8a5d0317b"
result.mask; // { path, sha256, byteLength, bounds: { x, y, width, height } }
result.shift; // { segments: [{ baseY: 90, headY: 120, height: 270, dy: 30, … }], … }
```

## What it does

### One scan, every threshold

A comparison is made of _tiers_: a colour threshold, and how much of the image
has to be over it before the tier reports anything. The two built-in tiers are
the ones Argos uses:

| tier      | colour threshold | reports when                             | catches                        |
| --------- | ---------------- | ---------------------------------------- | ------------------------------ |
| tolerant  | 0.15             | 20 pixels differ (0.02% on small images) | a changed icon, a moved border |
| sensitive | 0.0225           | 3% of the image differs                  | a tint, a shadow, a faded text |

`threshold` (0 to 1, default 0.5) scales both; `tiers` replaces them. The
pixels over a high threshold are a subset of those over a low one, so one
counter per pixel describes every tier and the image is read once whatever
their number. The result carries what each tier saw, and the mask of the one
that reports the most.

### Anti-aliasing

Two detectors, chosen with `antialiasing`:

- **`"classic"`** (default) is the detector of pixelmatch and odiff: a pixel is
  anti-aliasing when it is brighter than one neighbour and darker than another,
  and one of the two sits in a flat area. Its masks are identical to odiff
  4.5.0's, pixel for pixel, which keeps existing fingerprints valid.
- **`"subpixel"`** asks a different question: _could both images be the same
  shape, rendered less than a pixel apart?_ Along each axis through the pixel
  it looks for two pixels that did not change, then requires what lies between
  them to be either one edge whose position moved by at most 0.6 px, or one
  thin stroke that kept its ink and its centre.

What that changes, each line being a test in
[`tests/antialiasing.rs`](crates/iris/tests/antialiasing.rs):

|                                                    | classic      | subpixel |
| -------------------------------------------------- | ------------ | -------- |
| an edge blended differently                        | noise        | noise    |
| an edge over a gradient, a photo, a dithered fill  | **reported** | noise    |
| a hairline snapped to another pixel (font hinting) | **reported** | noise    |
| a hairline a little lighter                        | **reported** | noise    |
| a recoloured shape: its blended edge pixels        | **ignored**  | reported |
| an anti-aliased edge moved by a whole pixel        | **ignored**  | reported |
| a red pixel on a black and white edge              | **ignored**  | reported |
| a hard edge or a hairline moved by a pixel         | reported     | reported |
| a new hairline, a bolder stroke                    | reported     | reported |

On the screenshot fixtures, differing pixels left at the sensitive tier
(`cargo run --release -p iris --example antialiasing`):

| nothing changed but the rendering | none   | classic | subpixel |
| --------------------------------- | ------ | ------- | -------- |
| `fontAliasing`                    | 1,803  | 575     | 432      |
| `imageCompression4`               | 1,416  | 1,275   | 228      |
| `imageCompression5`               | 21,295 | 21,203  | 3,345    |
| `imageCompression7`               | 42,365 | 42,306  | 5,314    |

| something changed | none      | classic   | subpixel  |
| ----------------- | --------- | --------- | --------- |
| `simple`          | 499,332   | 489,927   | 497,624   |
| `big-images`      | 3,809,477 | 3,618,183 | 3,809,398 |

Switching detector changes masks, and with them the fingerprints of diffs that
involve anti-aliasing: ignore rules recorded against the old fingerprints stop
matching those diffs.

### Fingerprint

The mask is dilated by a pixel, cropped to its bounding box, centred in a
square, reduced to a 16×16 grid of fill levels, and hashed. Position and scale
drop out. The format is `v1` of `@argos-ci/mask-fingerprint` and the values
are identical bit for bit — including the three places where the original,
which streams a PNG row by row, departs from a textbook dilation. They are
written down in [`fingerprint.rs`](crates/iris/src/fingerprint.rs) and
checked against a copy of the original on every mask up to 4×4 and thousands
of random ones.

`fingerprintDiff(png)` fingerprints a stored mask, as the old package does,
truncated files included.

### Layout shifts

When an element grows by twenty pixels, everything below it moves and the diff
lights up the whole page. `result.shift` says how much of the diff is that:

```ts
{
  // Blocks of rows found at another height.
  segments: [{ x: 0, width: 200, baseY: 90, headY: 120, height: 270, dy: 30 }],
  // What is new, and what is gone, once movement is set aside.
  inserted: [{ x: 0, y: 90, width: 200, height: 30 }],
  removed: [],
  // Pixels of the mask that differ only because content moved.
  shiftedPixels: 20537,
}
```

Rows are matched by hash within the columns that differ, so a sidebar that
stayed put does not stop the page next to it from matching. Matching is exact:
a shift is never invented, and content that moved _and_ was re-rendered is not
found. Vertical movement only.

## API

```ts
diff(base, head, options?): Promise<DiffResult>
fingerprintDiff(png): Promise<string>
```

`base`, `head` and `png` are file paths or buffers. PNG and JPEG are decoded.

| option         | default     |                                                                   |
| -------------- | ----------- | ----------------------------------------------------------------- |
| `threshold`    | `0.5`       | sensitivity, 0 (strict) to 1; scales the built-in tiers           |
| `tiers`        |             | `{ threshold, minScore?, maxIgnoredPixels? }[]`, up to 8          |
| `antialiasing` | `"classic"` | `"classic"`, `"subpixel"`, or `false`                             |
| `exactScore`   | `false`     | see below                                                         |
| `fingerprint`  | `true`      |                                                                   |
| `shifts`       | `true`      |                                                                   |
| `minShiftRows` | `4`         | fewest rows that must move together                               |
| `maxPixels`    |             | reject canvases over this size, before decoding                   |
| `threads`      | up to 4     | for the scan, above a megapixel; the result does not depend on it |
| `output`       |             | write the mask there instead of returning it in `mask.data`       |

The mask is a 1-bit PNG, opaque red on transparent, the size of the larger
width and the larger height. `mask.sha256` is the hash of the file.

### `exactScore`

Argos has always read its scores from the `odiff` command line, which prints a
percentage with two decimals. A change under 0.005% of the image therefore
scored zero and was never reported: on a 1920×1080 screenshot that is 103
pixels, where the tolerant tier is meant to stop at 20. The `minorChange2`
fixture — 42 differing pixels, expected verdict "no change" — only passes
because of it.

By default the library rounds the same way, so that swapping it in changes no
verdict. `exactScore: true` turns the rounding off: the tiers then mean what
they say, and changes that were silently dropped are reported.

## Compared to the pipeline it replaces

Same verdict, same score and same fingerprint on every fixture at four
sensitivities, checked against the real `odiff-bin`, `sharp` and
`@argos-ci/mask-fingerprint` in [`__test__`](__test__/index.spec.ts). Three
things differ on purpose:

- **An image smaller both ways is not rescaled.** `sharp`'s `fit: "contain"`
  scaled such an image up before padding it, so the same content at twice the
  size read as a large difference. Images are now padded, never scaled.
- **Oversized pairs are compared in full.** Over 80 megapixels the old code
  capped the width, padded one image and not the other, and let odiff count
  what stuck out. Set `maxPixels` to refuse them instead.
- **The mask is a 1-bit PNG**, several times smaller than odiff's RGBA one.
  Its content hash differs from that of the same mask written by odiff.

Median of 9 runs, Apple M-series, from two files on disk to a mask on disk
with its hash and fingerprint (`pnpm bench`):

| fixture                | megapixels | differs | before (ms) | after (ms) |       |
| ---------------------- | ---------- | ------- | ----------- | ---------- | ----- |
| `boxShadow`            | 0.1        | —       | 5.6         | 0.3        | 17.1× |
| `alphaSmallGraph`      | 1.4        | —       | 21.5        | 1.6        | 13.4× |
| `minorChange1`         | 2.7        | 0.01%   | 58.5        | 2.6        | 22.4× |
| `minorChange2`         | 2.1        | —       | 29.4        | 3.3        | 9.0×  |
| `imageCompression5`    | 3.0        | —       | 49.4        | 11.5       | 4.3×  |
| `imageCompression7`    | 2.7        | —       | 97.4        | 26.5       | 3.7×  |
| `tableAlpha`           | 2.1        | 18%     | 53.8        | 6.3        | 8.6×  |
| `simple`               | 1.6        | 30%     | 55.9        | 8.3        | 6.7×  |
| `big-images`           | 4.5        | 81%     | 349.5       | 41.0       | 8.5×  |
| `different-dimensions` | 105.3      | 9%      | 3043.7      | 518.8      | 5.9×  |

All 19 fixtures: 3.9 s before, 0.65 s after. Where little differs the time
left is PNG decoding; `imageCompression7` is two noisy 2.7 megapixel PNGs that
take 16 ms each to inflate.

### Memory

A comparison holds the two decoded images, 4 bytes a pixel each, and a
quarter of a byte per pixel of the canvas: one bit per tier, the reported bit
plane becoming the mask. Nothing else grows with the image:

- images of different sizes are read in place, never copied onto a padded
  canvas;
- PNG files are decoded as they are read, greyscale ones widened in place, and
  JPEG decoded straight into the pixel buffer;
- with `maxPixels`, an oversized pair — or a decompression bomb — is refused
  from the headers, before a pixel buffer exists.

[`tests/memory.rs`](crates/iris/tests/memory.rs) counts every allocation and
fails when one of these grows past its budget, and a Node.js test checks that
repeated calls give their memory back.

The old pipeline ran two odiff processes at once, each holding both images
and an RGBA diff. Peak resident memory, measured with `/usr/bin/time -l`:

| fixture             | megapixels | odiff ×2 | iris   |
| ------------------- | ---------- | -------- | ------ |
| `simple`            | 1.6        | 44 MiB   | 14 MiB |
| `tableAlpha`        | 2.1        | 54 MiB   | 17 MiB |
| `imageCompression7` | 2.7        | 68 MiB   | 22 MiB |
| `big-images`        | 4.5        | 108 MiB  | 34 MiB |

iris is measured above a Node.js process with the addon loaded; the odiff
figures leave out `sharp` and the fingerprinting process.

## Rust

The comparison is the `iris` crate, in
[`crates/iris`](crates/iris); the Node.js package is a thin N-API
binding over it.

```rust
use iris::{compare_files, Antialiasing, Options};

let options = Options {
  antialiasing: Antialiasing::Subpixel,
  ..Options::sensitivity(0.5)
};
let result = compare_files("base.png", "head.png", &options)?;
if let Some(mask) = &result.mask {
  std::fs::write("mask.png", mask.to_png()?)?;
}
```

## Development

```bash
pnpm install
pnpm build      # the native addon, release profile
pnpm test       # Rust tests, then the Node.js tests
pnpm bench
```

`pnpm test:rust` points `ODIFF_BIN` at the binary `odiff-bin` ships and runs
it on every fixture and on generated images, at six thresholds, with
anti-aliasing on and off, comparing masks bit by bit. Parity is with odiff
**4.5.0**, whose fixed-point coefficients are truncated; later versions round
them and count a few more pixels near the threshold.

## License

MIT
