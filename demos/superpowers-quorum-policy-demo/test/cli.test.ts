import { describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { verify } from "../src/cli.ts";

test("fixture bundle has isolated concurrent arms and verifies independently", async () => {
  const proc = Bun.spawn(["bun", "src/cli.ts", "fixture"], { cwd: import.meta.dir + "/..", stdout: "pipe", stderr: "pipe" });
  await proc.exited;
  expect(proc.exitCode).toBe(0);
  const result = await verify(join(import.meta.dir, "../../../artifacts/superpowers-policy-demo"));
  expect(result.ok).toBe(true); expect(result.traceCount).toBe(12);
  expect((await readFile(join(join(import.meta.dir, "../../../artifacts/superpowers-policy-demo"), "manifest.json"), "utf8"))).toContain('"simulated": true');
});

describe("safe policy", () => { test("documented commands are present", async () => { const p = await readFile(join(import.meta.dir, "../src/cli.ts"), "utf8"); expect(p).toContain("contextual-safe-key-v1"); expect(p).toContain("unsafePhases"); }); });
