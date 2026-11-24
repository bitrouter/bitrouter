import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["tests/behavior/live/**/*.test.ts"],
    environment: "node",
    pool: "threads",
    fileParallelism: false,
    maxWorkers: 1,
    testTimeout: 300_000,
    hookTimeout: 120_000,
    teardownTimeout: 60_000,
  },
});
