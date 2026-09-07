import assert from "node:assert/strict";
import { test } from "node:test";
import { prompt, observe, initialize, metadataKey } from "./runtime.mjs";
import { transform } from "./transform.mjs";

const adapter = { package: "fixture", version: "1", moduleDigest: "fixture" };
const request = (id, original = { sessionId: "same-session", prompt: [{ type: "text", text: "same text" }] }) => ({
  ...original,
  _meta: { ...(original._meta ?? {}), [metadataKey]: {
    origin: { operationId: id },
    metaState: Object.hasOwn(original, "_meta") ? original._meta === null ? "null" : "object" : "absent",
  } },
});

test("initialization preserves original capabilities, metadata and errors", async () => {
  const original = { protocolVersion: 1, agentCapabilities: { loadSession: true }, _meta: { userOption: "kept" } };
  const result = await initialize(async () => original, adapter);
  assert.deepEqual(result, { ...original, _meta: { ...original._meta, [metadataKey]: { schema: 1, adapter } } });
  assert.deepEqual(original._meta, { userOption: "kept" });
  const failure = new Error("initialize failed");
  await assert.rejects(initialize(async () => { throw failure; }, adapter), (error) => error === failure);
});

test("concurrent identical prompts preserve their own producer closure and user options", async () => {
  const records = [];
  let releaseFirst;
  const held = new Promise((resolve) => { releaseFirst = resolve; });
  const original = { sessionId: "same-session", prompt: [{ type: "text", text: "same text" }], _meta: { userOption: "retained" } };
  const first = prompt(request("first", original), async (clean) => {
    assert.deepEqual(clean, original);
    await held;
    observe(clean, { kind: "codex_accepted", threadId: "native-a", turnId: "turn-a", role: "prompt" });
    return "a";
  }, (record) => records.push(record), adapter);
  const second = prompt(request("second", original), async (clean) => {
    assert.deepEqual(clean, original);
    observe(clean, { kind: "codex_accepted", threadId: "native-b", turnId: "turn-b", role: "prompt" });
    return "b";
  }, (record) => records.push(record), adapter);
  assert.equal(await second, "b");
  releaseFirst();
  assert.equal(await first, "a");
  for (const [id, turn] of [["first", "turn-a"], ["second", "turn-b"]]) {
    const own = records.filter((record) => record.origin.operationId === id);
    assert.deepEqual(own.map((record) => record.sequence), [0, 1, 2]);
    assert.equal(own[1].event.turnId, turn);
    assert.equal(own[2].event.outcome, "returned");
    assert.ok(own.every((record) => !Object.hasOwn(record, "prompt")));
  }
});

test("acceptance after cancellation still names the original operation", async () => {
  const records = [];
  let late;
  const cancelled = { stopReason: "cancelled" };
  assert.equal(await prompt(request("cancelled"), async (clean) => {
    late = () => observe(clean, { kind: "codex_accepted", threadId: "native", turnId: "late", role: "prompt" });
    return cancelled;
  }, (record) => records.push(record), adapter), cancelled);
  await prompt(request("replacement"), async (clean) => {
    late();
    observe(clean, { kind: "codex_accepted", threadId: "native", turnId: "new", role: "prompt" });
  }, (record) => records.push(record), adapter);
  const acceptance = records.find((record) => record.event.turnId === "late");
  assert.equal(acceptance.origin.operationId, "cancelled");
  assert.equal(acceptance.sequence, 2);
  assert.equal(records.find((record) => record.event.turnId === "new").origin.operationId, "replacement");
});

test("absent/null metadata and original errors survive transport failure", async () => {
  for (const original of [{ sessionId: "s", prompt: [] }, { sessionId: "s", prompt: [], _meta: null }]) {
    const failure = new Error("original invocation failure");
    const records = [];
    await assert.rejects(prompt(request("op", original), async (clean) => {
      assert.deepEqual(clean, original);
      throw failure;
    }, (record) => {
      if (record.sequence === 0) throw new Error("notification write failure");
      records.push(record);
    }, adapter), (error) => error === failure);
    assert.equal(records[0].event.outcome, "threw");
    assert.equal(records[0].event.notificationFailures, 1);
  }
});

test("an unresponsive evidence writer cannot retain the prompt indefinitely", async () => {
  let count = 0;
  const started = Date.now();
  const result = await prompt(request("bounded"), async (clean) => {
    for (let i = 0; i < 200; i++) observe(clean, { kind: "claude_enqueued", commandId: `c-${i}` });
    return "done";
  }, () => { count++; return new Promise(() => {}); }, adapter);
  assert.equal(result, "done");
  assert.equal(count, 128);
  assert.ok(Date.now() - started < 5000);
});

test("unrecognized producer code cannot be transformed by matching snippets", () => {
  const snippet = Buffer.from("class Fixture { async prompt(params) {} }");
  for (const harness of ["codex", "claude", "unknown"]) {
    assert.equal(transform(harness, snippet, new URL("./runtime.mjs", import.meta.url).href), null);
  }
});
