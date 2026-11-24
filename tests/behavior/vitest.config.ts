import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["tests/behavior/deterministic/**/*.test.ts"],
    environment: "node",
    pool: "threads",
    fileParallelism: false,
    testTimeout: 120_000,
    hookTimeout: 60_000,
    teardownTimeout: 30_000,
  },
});
