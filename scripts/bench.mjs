// Time the pipeline this library replaces against the library, on the
// screenshot fixtures, the way the Argos backend calls each: one comparison
// at a time, from two files on disk to a mask on disk, its content hash and
// its fingerprint.
//
//   pnpm build && pnpm bench [filter] [--runs=9]
import { createHash } from "node:crypto";
import { mkdtemp, readFile, readdir, rm, stat } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { performance } from "node:perf_hooks";

import { legacyDiff } from "../__test__/legacy.ts";

const require = createRequire(import.meta.url);
const { diff } = require("../index.js");
const sharp = require("sharp");

const args = process.argv.slice(2);
const runs = Number(
  args.find((arg) => arg.startsWith("--runs="))?.slice(7) ?? 9,
);
const filter = args.find((arg) => !arg.startsWith("--"));

const pairs = join(import.meta.dirname, "../fixtures/pairs");
const scratch = await mkdtemp(join(tmpdir(), "iris-bench-"));

async function legacy(base, head) {
  const result = await legacyDiff(base, head, 0.5, scratch);
  if (result.maskPath) {
    // The backend hashes the mask to address it in storage.
    createHash("sha256")
      .update(await readFile(result.maskPath))
      .digest("hex");
  }
  return result;
}

const candidates = {
  "odiff x2 + fingerprint": legacy,
  iris: (base, head) => diff(base, head, { output: join(scratch, "mask.png") }),
  "iris, subpixel": (base, head) =>
    diff(base, head, {
      output: join(scratch, "mask.png"),
      antialiasing: "subpixel",
    }),
};

async function median(run) {
  await run();
  const times = [];
  for (let index = 0; index < runs; index++) {
    const start = performance.now();
    await run();
    times.push(performance.now() - start);
  }
  times.sort((a, b) => a - b);
  return times[times.length >> 1];
}

const rows = [];
for (const name of (await readdir(pairs)).sort()) {
  if (filter && !name.includes(filter)) {
    continue;
  }
  const base = join(pairs, name, "base.png");
  const head = join(pairs, name, "compare.png");
  const [a, b] = await Promise.all([
    sharp(base).metadata(),
    sharp(head).metadata(),
  ]);
  const megapixels =
    (Math.max(a.width, b.width) * Math.max(a.height, b.height)) / 1e6;
  const { score, mask } = await diff(base, head);
  const before = await legacyDiff(base, head, 0.5, scratch);
  const maskBytes = before.maskPath ? (await stat(before.maskPath)).size : 0;

  const row = {
    fixture: name,
    MP: megapixels.toFixed(1),
    "diff %": (score * 100).toFixed(2),
    "mask before (KB)": (maskBytes / 1024).toFixed(1),
    "mask after (KB)": ((mask?.byteLength ?? 0) / 1024).toFixed(1),
  };
  const times = {};
  for (const [label, run] of Object.entries(candidates)) {
    times[label] = await median(() => run(base, head));
    row[`${label} (ms)`] = times[label].toFixed(1);
  }
  row.speedup = `${(times["odiff x2 + fingerprint"] / times.iris).toFixed(1)}x`;
  rows.push({ row, times });
  console.error(`${name}: ${row.speedup}`);
}

console.table(rows.map(({ row }) => row));

const total = (label) => rows.reduce((sum, { times }) => sum + times[label], 0);
const [before, after] = [total("odiff x2 + fingerprint"), total("iris")];
console.log(
  `total: ${before.toFixed(0)} ms -> ${after.toFixed(0)} ms (${(before / after).toFixed(1)}x), median of ${runs} runs each`,
);

await rm(scratch, { recursive: true, force: true });
