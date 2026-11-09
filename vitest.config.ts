import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["__test__/**/*.spec.ts"],
    // The legacy pipeline runs odiff several times per fixture.
    testTimeout: 180_000,
  },
});
