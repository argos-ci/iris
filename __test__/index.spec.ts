import { createHash } from "node:crypto";
import { mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fingerprintDiff as legacyFingerprintDiff } from "@argos-ci/mask-fingerprint";
import sharp from "sharp";
import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { diff, fingerprintDiff } from "../index.js";
import { legacyDiff } from "./legacy.js";

const fixtures = join(import.meta.dirname, "../fixtures");
const pair = (name: string) => ({
  base: join(fixtures, "pairs", name, "base.png"),
  head: join(fixtures, "pairs", name, "compare.png"),
});

let scratch: string;
beforeAll(async () => {
  scratch = await mkdtemp(join(tmpdir(), "iris-"));
});
afterAll(async () => {
  await rm(scratch, { recursive: true, force: true });
});

/** Raw RGBA pixels as a PNG buffer. */
function png(
  width: number,
  height: number,
  pixel: (x: number, y: number) => number[],
) {
  const data = Buffer.alloc(width * height * 4);
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      data.set(pixel(x, y), (y * width + x) * 4);
    }
  }
  return sharp(data, { raw: { width, height, channels: 4 } })
    .png()
    .toBuffer();
}

const WHITE = [255, 255, 255, 255];
const BLACK = [0, 0, 0, 255];

describe("diff", () => {
  test("reports nothing for identical images", async () => {
    const { base } = pair("simple");
    const result = await diff(base, base);
    expect(result).toMatchObject({ score: 0, diffPixels: 0 });
    expect(result.mask).toBeUndefined();
    expect(result.fingerprint).toBeUndefined();
    expect(result.shift).toBeUndefined();
  });

  test("returns the mask as a buffer with its content address", async () => {
    const { base, head } = pair("simple");
    const result = await diff(base, head);
    expect(result.score).toBeGreaterThan(0);
    expect(result.fingerprint).toMatch(
      /^v1:g16:d1:t0\.002,0\.02,0\.08:[0-9a-f]{16}$/,
    );

    const mask = result.mask!;
    expect(mask.path).toBeUndefined();
    expect(mask.data!.length).toBe(mask.byteLength);
    expect(mask.sha256).toBe(
      createHash("sha256").update(mask.data!).digest("hex"),
    );

    const metadata = await sharp(mask.data!).metadata();
    expect(metadata).toMatchObject({
      width: result.width,
      height: result.height,
      format: "png",
    });
  });

  test("writes the mask where asked", async () => {
    const { base, head } = pair("simple");
    const output = join(scratch, "mask.png");
    const result = await diff(base, head, { output });
    const mask = result.mask!;
    expect(mask.path).toBe(output);
    expect(mask.data).toBeUndefined();

    const written = await readFile(output);
    expect(written.length).toBe(mask.byteLength);
    expect(createHash("sha256").update(written).digest("hex")).toBe(
      mask.sha256,
    );
    // The fingerprint of the file is the one returned with it.
    expect(await fingerprintDiff(output)).toBe(result.fingerprint);
  });

  test("draws differences in opaque red on a transparent background", async () => {
    const base = await png(8, 4, () => WHITE);
    const head = await png(8, 4, (x, y) =>
      x === 5 && y === 2 ? BLACK : WHITE,
    );
    const result = await diff(base, head, {
      tiers: [{ threshold: 0.1 }],
      exactScore: true,
    });
    expect(result.diffPixels).toBe(1);
    expect(result.mask!.bounds).toEqual({ x: 5, y: 2, width: 1, height: 1 });

    const { data } = await sharp(result.mask!.data!)
      .ensureAlpha()
      .raw()
      .toBuffer({ resolveWithObject: true });
    for (let index = 0; index < 32; index++) {
      const expected = index === 2 * 8 + 5 ? [255, 0, 0, 255] : [0, 0, 0, 0];
      expect([...data.subarray(index * 4, index * 4 + 4)]).toEqual(expected);
    }
  });

  test("accepts buffers and paths alike", async () => {
    const { base, head } = pair("minorChange1");
    const [fromPaths, fromBuffers, mixed] = await Promise.all([
      diff(base, head),
      diff(await readFile(base), await readFile(head)),
      diff(base, await readFile(head)),
    ]);
    expect(fromBuffers).toEqual(fromPaths);
    expect(mixed).toEqual(fromPaths);
  });

  test("evaluates custom tiers in one call", async () => {
    const { base, head } = pair("tableAlpha");
    const result = await diff(base, head, {
      tiers: [{ threshold: 0.3 }, { threshold: 0.1 }, { threshold: 0.01 }],
      exactScore: true,
    });
    const counts = result.tiers.map((tier) => tier.diffPixels);
    // A stricter threshold can only see more.
    expect(counts[0]).toBeLessThanOrEqual(counts[1]!);
    expect(counts[1]).toBeLessThanOrEqual(counts[2]!);
    expect(result.tier).toBe(2);
    expect(result.diffPixels).toBe(counts[2]);
  });

  test("exactScore reports what the default rounding hides", async () => {
    const { base, head } = pair("minorChange2");
    const rounded = await diff(base, head);
    const exact = await diff(base, head, { exactScore: true });
    expect(rounded.score).toBe(0);
    expect(rounded.tiers[0]!.diffPixels).toBe(42);
    expect(exact.score).toBe(42 / (1920 * 1080));
    expect(exact.mask).toBeDefined();
  });

  test("offers a stricter and more perceptive anti-aliasing detector", async () => {
    const tiers = [{ threshold: 0.0225 }];
    const count = async (
      name: string,
      antialiasing: boolean | "classic" | "subpixel",
    ) => {
      const { base, head } = pair(name);
      const result = await diff(base, head, {
        tiers,
        antialiasing,
        exactScore: true,
        shifts: false,
      });
      return result.tiers[0]!.diffPixels;
    };

    // Compression noise: nothing changed, and far less is left to report.
    const noise = "imageCompression7";
    expect(await count(noise, true)).toBe(await count(noise, "classic"));
    expect(await count(noise, "classic")).toBeGreaterThan(40_000);
    expect(await count(noise, "subpixel")).toBeLessThan(6_000);
    expect(await count(noise, false)).toBeGreaterThan(
      await count(noise, "classic"),
    );

    // A real change: nearly all of it is kept, more than the classic one keeps.
    const change = "simple";
    expect(await count(change, "subpixel")).toBeGreaterThan(
      await count(change, "classic"),
    );
    expect(await count(change, "subpixel")).toBeGreaterThan(
      (await count(change, false)) * 0.99,
    );

    const { base, head } = pair(noise);
    // @ts-expect-error not a detector
    await expect(diff(base, head, { antialiasing: "fuzzy" })).rejects.toThrow(
      /unknown antialiasing "fuzzy"/,
    );
  });

  test("returns the same result on any number of threads", async () => {
    const { base, head } = pair("tableAlpha");
    const [one, four, auto] = await Promise.all([
      diff(base, head, { threads: 1 }),
      diff(base, head, { threads: 4 }),
      diff(base, head),
    ]);
    expect(four).toEqual(one);
    expect(auto).toEqual(one);
  });

  test("detects content that moved", async () => {
    const line = (seed: number) => (x: number, y: number) =>
      y < 12 &&
      (Math.imul(seed + 1, 2654435761) ^
        Math.imul((x / 3) | 0, 40503) ^
        Math.imul(y + 7, 9973)) %
        8 <
        3
        ? BLACK
        : WHITE;
    const page = (insertAt: number | null) =>
      png(160, 240, (x, y) => {
        if (insertAt !== null && y >= insertAt && y < insertAt + 24) {
          return [200, 40, 40, 255];
        }
        const source = insertAt !== null && y >= insertAt + 24 ? y - 24 : y;
        return line((source / 18) | 0)(x, source % 18);
      });

    const result = await diff(await page(null), await page(54));
    expect(result.shift).toBeDefined();
    expect(result.shift!.segments).toEqual([
      { x: 0, width: 160, baseY: 54, headY: 78, height: 162, dy: 24 },
    ]);
    expect(result.shift!.inserted).toEqual([
      { x: 0, y: 54, width: 160, height: 24 },
    ]);
    // The screenshot kept its height, so the last 24 rows fell off the bottom.
    expect(result.shift!.removed).toEqual([
      { x: 0, y: 216, width: 160, height: 24 },
    ]);
    expect(result.shift!.shiftedPixels).toBeGreaterThan(
      result.diffPixels * 0.6,
    );
    expect(result.shift!.shiftedPixels).toBeLessThan(
      result.diffPixels - 24 * 160,
    );

    const without = await diff(await page(null), await page(54), {
      shifts: false,
    });
    expect(without.shift).toBeUndefined();
    expect(without.fingerprint).toBe(result.fingerprint);
  });

  test("compares images of different sizes on a common canvas", async () => {
    const { base, head } = pair("big-images");
    const result = await diff(base, head);
    expect(result).toMatchObject({ width: 1000, height: 4469 });
    expect(result.score).toBeGreaterThan(0);
  });

  test("decodes JPEG", async () => {
    const pixels = await png(64, 64, (x, y) => [x * 4, y * 4, 128, 255]);
    const jpeg = await sharp(pixels).jpeg({ quality: 90 }).toBuffer();
    const same = await diff(jpeg, jpeg);
    expect(same).toMatchObject({ score: 0, width: 64, height: 64 });
  });

  test("rejects what it cannot compare", async () => {
    const { base } = pair("simple");
    await expect(diff(join(fixtures, "missing.png"), base)).rejects.toThrow(
      /missing\.png/,
    );
    await expect(diff(Buffer.from("not an image"), base)).rejects.toThrow(
      /unsupported image format/,
    );
    await expect(diff(base, base, { maxPixels: 1000 })).rejects.toThrow(
      /exceeds the limit/,
    );
    await expect(diff(base, base, { tiers: [] })).rejects.toThrow(
      /invalid options/,
    );
    await expect(
      diff(base, base, { tiers: [{ threshold: 2 }] }),
    ).rejects.toThrow(/invalid options/);
  });
});

describe("fingerprintDiff", () => {
  test("returns what @argos-ci/mask-fingerprint returns", async () => {
    const directory = join(fixtures, "masks");
    const names = (await readdir(directory)).filter((name) =>
      name.endsWith(".png"),
    );
    expect(names.length).toBeGreaterThanOrEqual(9);
    for (const name of names) {
      const path = join(directory, name);
      const expected = await legacyFingerprintDiff(path);
      expect(await fingerprintDiff(path), name).toBe(expected);
      expect(await fingerprintDiff(await readFile(path)), name).toBe(expected);
    }
  });
});

/**
 * The verdicts the Argos backend expects, from its own test suite.
 * [fixture, sensitivity, images differ]
 */
const CASES: [string, number, boolean][] = [
  ["alphaBackground", 0.5, false],
  ["alphaSmallGraph", 0.2, true],
  ["big-images", 0.5, true],
  ["border", 0.5, false],
  ["boxShadow", 0.5, false],
  ["fontAliasing", 0.5, false],
  ["imageCompression1", 0.5, false],
  ["imageCompression2", 0.5, false],
  ["imageCompression3", 0.5, false],
  ["imageCompression4", 0.5, false],
  ["imageCompression5", 0.5, false],
  ["imageCompression6", 0.5, false],
  ["imageCompression7", 0.5, false],
  ["simple", 0.5, true],
  ["tableAlpha", 0.5, true],
  ["minorChange1", 0.5, true],
  ["minorChange2", 0.5, false],
  ["minorChange3", 0, true],
];

describe("drop-in parity with the odiff + mask-fingerprint pipeline", () => {
  test.each(CASES)("%s at sensitivity %s", async (name, threshold, differs) => {
    const { base, head } = pair(name);
    const [legacy, result] = await Promise.all([
      legacyDiff(
        base,
        head,
        threshold,
        await mkdtemp(join(scratch, "legacy-")),
      ),
      diff(base, head, { threshold }),
    ]);

    expect(legacy.score > 0).toBe(differs);
    expect(result.score).toBe(legacy.score);
    expect(result.fingerprint ?? null).toBe(legacy.fingerprint);
    expect({ width: result.width, height: result.height }).toEqual({
      width: legacy.width,
      height: legacy.height,
    });
    if (legacy.maskPath) {
      // Same pixels, read back through the old fingerprinting package.
      expect(await legacyFingerprintDiff(result.mask!.data!)).toBe(
        legacy.fingerprint,
      );
    }
  });

  // Every fixture again, at the settings a project can pick.
  test.each(
    CASES.flatMap(([name]) =>
      [0, 0.25, 1].map((threshold) => [name, threshold] as const),
    ),
  )("%s at sensitivity %s", async (name, threshold) => {
    const { base, head } = pair(name);
    const [legacy, result] = await Promise.all([
      legacyDiff(
        base,
        head,
        threshold,
        await mkdtemp(join(scratch, "legacy-")),
      ),
      diff(base, head, { threshold }),
    ]);
    expect(result.score).toBe(legacy.score);
    expect(result.fingerprint ?? null).toBe(legacy.fingerprint);
  });

  test("compares an oversized pair on its full canvas", async () => {
    // 532x39062 against 2695x4986 is over the old 80 megapixel limit, where
    // the old pipeline capped the width at 2048, padded only the image that
    // fit, and let odiff count whatever stuck out: a mask narrower than one
    // of the two images. Here both are compared on the canvas that holds them.
    const { base, head } = pair("different-dimensions");
    const legacy = await legacyDiff(
      base,
      head,
      0,
      await mkdtemp(join(scratch, "legacy-")),
    );
    expect(legacy).toMatchObject({ width: 2048, height: 39062 });

    const result = await diff(base, head, { threshold: 0 });
    expect(result).toMatchObject({ width: 2695, height: 39062 });
    expect(result.score).toBeGreaterThan(0);
    expect(result.score).toBeLessThan(1);
    // The mask reaches past the narrower image: both were looked at in full.
    expect(result.mask!.bounds.width).toBeGreaterThan(532);
    expect(result.mask!.bounds.height).toBeGreaterThan(4986);
    expect(result.fingerprint).toBeDefined();

    await expect(diff(base, head, { maxPixels: 80_000_000 })).rejects.toThrow(
      /2695x39062 exceeds the limit/,
    );
  });

  test("does not rescale an image that is smaller both ways", async () => {
    // The old pipeline padded with sharp's `fit: "contain"`, which first
    // scales an image up when it is smaller in both dimensions: the same
    // content, shown at twice the size, read as a large difference.
    const content = (x: number, y: number) =>
      x >= 20 && x < 30 && y >= 20 && y < 30 ? BLACK : WHITE;
    const small = join(scratch, "small.png");
    const large = join(scratch, "large.png");
    await sharp(await png(50, 50, content)).toFile(small);
    await sharp(await png(100, 100, content)).toFile(large);

    const legacy = await legacyDiff(
      small,
      large,
      0.5,
      await mkdtemp(join(scratch, "legacy-")),
    );
    expect(legacy.score).toBeGreaterThan(0.03);

    // Nothing moved and nothing was added but white space.
    const result = await diff(small, large);
    expect(result).toMatchObject({ width: 100, height: 100, score: 0 });
  });
});
