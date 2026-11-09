// The pipeline this library replaces, as the Argos backend ran it: pad both
// images to a common size with sharp, run odiff twice at two thresholds, pick
// one of the two masks, then fingerprint it with `@argos-ci/mask-fingerprint`.
// Kept here, unchanged in behavior, as the reference the new implementation
// is held to.
import { stat } from "node:fs/promises";
import { join } from "node:path";
import { fingerprintDiff } from "@argos-ci/mask-fingerprint";
import { compare } from "odiff-bin";
import sharp from "sharp";

const BASE_THRESHOLD = 0.15;
const BASE_MAX_SCORE = 0.0002;
const MAXIMUM_PIXELS_TO_IGNORE = 20;
const COLOR_SENSIBLE_THRESHOLD = 0.0225;
const COLOR_SENSIBLE_MAX_SCORE = 0.03;
const MAX_PIXELS = 80_000_000;
const DEFAULT_MAX_WIDTH = 2048;

type Dimensions = { width: number; height: number };

function fitIntoMaxPixels(size: Dimensions): Dimensions {
  const nbPixels = size.width * size.height;
  if (nbPixels > MAX_PIXELS) {
    const scaleFactor = Math.sqrt(MAX_PIXELS / nbPixels);
    return {
      width: Math.floor(size.width * scaleFactor),
      height: Math.floor(size.height * scaleFactor),
    };
  }
  return size;
}

async function getDimensions(path: string): Promise<Dimensions> {
  const { width, height } = await sharp(path).metadata();
  return { width, height };
}

function getMaxDimensions(...sizes: Dimensions[]): Dimensions {
  const width = Math.max(...sizes.map((size) => size.width));
  const height = Math.max(...sizes.map((size) => size.height));
  if (width * height > MAX_PIXELS && width < height) {
    return fitIntoMaxPixels({
      width: DEFAULT_MAX_WIDTH,
      height: Math.floor(MAX_PIXELS / DEFAULT_MAX_WIDTH),
    });
  }
  return fitIntoMaxPixels({ width, height });
}

async function enlarge(
  path: string,
  size: Dimensions,
  target: Dimensions,
  output: string,
): Promise<string> {
  if (
    (size.height === target.height && size.width === target.width) ||
    size.width > target.width ||
    size.height > target.height
  ) {
    return path;
  }
  await sharp(path)
    .resize({
      width: target.width,
      height: target.height,
      fit: "contain",
      position: "left top",
      background: { r: 0, g: 0, b: 0, alpha: 0 },
    })
    .toFile(output);
  return output;
}

async function computeDiff(args: {
  basePath: string;
  headPath: string;
  diffPath: string;
  threshold: number;
}): Promise<number> {
  const result = await compare(args.basePath, args.headPath, args.diffPath, {
    outputDiffMask: true,
    threshold: args.threshold,
    antialiasing: true,
  });
  if (result.match) {
    return 0;
  }
  switch (result.reason) {
    case "layout-diff":
      return 1;
    case "pixel-diff":
      return result.diffPercentage / 100;
    default:
      throw new Error(`Unexpected odiff result: ${result.reason}`);
  }
}

export type LegacyResult = {
  score: number;
  /** Null when there is no difference, or no mask to fingerprint. */
  fingerprint: string | null;
  maskPath: string | null;
  width: number;
  height: number;
};

export async function legacyDiff(
  base: string,
  head: string,
  threshold: number,
  scratch: string,
): Promise<LegacyResult> {
  const [baseSize, headSize] = await Promise.all([
    getDimensions(base),
    getDimensions(head),
  ]);
  const max = getMaxDimensions(baseSize, headSize);
  const basePath = await enlarge(
    base,
    baseSize,
    max,
    join(scratch, "base.png"),
  );
  const headPath = await enlarge(
    head,
    headSize,
    max,
    join(scratch, "head.png"),
  );

  const relative = threshold * 2;
  const baseDiffPath = join(scratch, "diff-base.png");
  const colorDiffPath = join(scratch, "diff-color.png");
  const [baseScore, colorScore] = await Promise.all([
    computeDiff({
      basePath,
      headPath,
      diffPath: baseDiffPath,
      threshold: BASE_THRESHOLD * relative,
    }),
    computeDiff({
      basePath,
      headPath,
      diffPath: colorDiffPath,
      threshold: COLOR_SENSIBLE_THRESHOLD * relative,
    }),
  ]);

  const maxBaseScore = Math.min(
    BASE_MAX_SCORE * relative,
    (MAXIMUM_PIXELS_TO_IGNORE * relative) / (max.width * max.height),
  );
  const adjustedBase = baseScore < maxBaseScore ? 0 : baseScore;
  const adjustedColor =
    colorScore < COLOR_SENSIBLE_MAX_SCORE * relative ? 0 : colorScore;

  const selected =
    adjustedBase > 0 && adjustedBase >= adjustedColor
      ? { score: adjustedBase, path: baseDiffPath }
      : adjustedColor > 0 && adjustedColor > adjustedBase
        ? { score: adjustedColor, path: colorDiffPath }
        : null;

  if (!selected) {
    return { score: 0, fingerprint: null, maskPath: null, ...max };
  }
  // odiff writes no mask when it gives up on images of different sizes.
  const written = await stat(selected.path).then(
    () => true,
    () => false,
  );
  return {
    score: selected.score,
    fingerprint: written ? await fingerprintDiff(selected.path) : null,
    maskPath: written ? selected.path : null,
    ...max,
  };
}
