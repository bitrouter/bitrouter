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
  expect(await verify(join(import.meta.dir, "../../../artifacts/superpowers-policy-demo/latest"))).toMatchObject({ ok: true, traceCount: 12 });
});

test("smoke and full execute the local runner and publish drill-down evidence", async () => {
  for (const mode of ["smoke", "full"]) {
    const proc = Bun.spawn(["bun", "src/cli.ts", mode], { cwd: import.meta.dir + "/..", stdout: "pipe", stderr: "pipe" });
    await proc.exited;
    expect(proc.exitCode).toBe(0);
    const root = join(import.meta.dir, "../../../artifacts/superpowers-policy-demo");
    expect(JSON.parse(await readFile(join(root, "manifest.json"), "utf8"))).toMatchObject({ executionMode: mode, realExecution: true });
    const dashboard = await readFile(join(root, "dashboard.html"), "utf8");
    expect(dashboard).toContain("totalNormalizedShowbackUsd");
    expect(dashboard).toContain("Trace drill-down");
    expect((await verify(join(root, "latest"))).ok).toBe(true);
  }
});

describe("safe policy", () => { test("documented commands are present", async () => { const p = await readFile(join(import.meta.dir, "../src/cli.ts"), "utf8"); expect(p).toContain("contextual-safe-key-v1"); expect(p).toContain("unsafePhases"); }); });
