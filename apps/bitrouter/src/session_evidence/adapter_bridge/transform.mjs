// Hashes are for the published npm modules, checked against registry tarballs.
// Reference sources at their published git revisions:
// https://github.com/agentclientprotocol/codex-acp/tree/061f9a4a2e463a220d7a3ab2ae5e9732837085ef
// https://github.com/agentclientprotocol/claude-agent-acp/tree/3e23c5b960b66a6d2c892e7524c952e731c076a7
import { createHash } from "node:crypto";
import pins from "./pins.json" with { type: "json" };
export const adapters = pins;

function replace(source, before, after, expected = 1) {
  const parts = source.split(before);
  if (parts.length !== expected + 1) throw new Error("native bridge anchor mismatch");
  return parts.join(after);
}

export function transform(harness, bytes, runtimeUrl) {
  const adapter = adapters[harness];
  const buffer = Buffer.from(bytes);
  if (!adapter || createHash("sha256").update(buffer).digest("hex") !== adapter.moduleDigest) {
    return null;
  }
  let source = buffer.toString("utf8");
  const identity = JSON.stringify({
    package: adapter.package, version: adapter.version, moduleDigest: adapter.moduleDigest,
  });
  if (harness === "claude") {
    source = replace(source, "    async initialize(request) {", `    async initialize(request) {\n        return __brEvidenceInitialize(() => this.__brEvidenceInitializeBody(request), ${identity});\n    }\n    async __brEvidenceInitializeBody(request) {`);
    const entry = "    async prompt(params) {";
    source = replace(source, entry, `${entry}\n        return __brEvidencePrompt(params, (clean) => this.__brEvidencePromptBody(clean), (payload) => this.client.extNotification(\"_bitrouter/nativeBinding\", payload), ${identity});\n    }\n    async __brEvidencePromptBody(params) {`);
    source = replace(source,
      "        session.turnQueue.push(turn);\n        session.input.push(userMessage);",
      "        session.turnQueue.push(turn);\n        session.input.push(userMessage);\n        __brEvidenceObserve(params, { kind: \"claude_enqueued\", commandId: promptUuid });");
  } else {
    source = replace(source, "  async initialize(_params) {", `  async initialize(_params) {\n    return __brEvidenceInitialize(() => this.__brEvidenceInitializeBody(_params), ${identity});\n  }\n  async __brEvidenceInitializeBody(_params) {`);
    const entry = "  async prompt(params, signal, onTurnStarted) {";
    source = replace(source, entry, `${entry}\n    return __brEvidencePrompt(params, (clean) => this.__brEvidencePromptBody(clean, signal, onTurnStarted), (payload) => this.connection.notify(\"_bitrouter/nativeBinding\", payload), ${identity});\n  }\n  async __brEvidencePromptBody(params, signal, onTurnStarted) {`);
    source = replace(source,
      "        onTurnStarted: (turnId, threadId) => {\n          const turn = { threadId, turnId };",
      "        onTurnStarted: (turnId, threadId) => {\n          __brEvidenceObserve(params, { kind: \"codex_command_observed\", threadId, turnId });\n          const turn = { threadId, turnId };");
    for (const [indent, role] of [["            ", "prompt"], ["                ", "implementation"]]) {
      const before = `\n${indent}const turn = { threadId: params.sessionId, turnId };`;
      source = replace(source, before, `\n${indent}__brEvidenceObserve(params, { kind: \"codex_accepted\", threadId: params.sessionId, turnId, role: ${JSON.stringify(role)} });${before}`);
    }
  }
  // Keep the shebang first and the original module URL for relative imports,
  // createRequire, executable resolution and source identity.
  const firstLine = source.startsWith("#!") ? source.indexOf("\n") + 1 : 0;
  return source.slice(0, firstLine)
    + `import { prompt as __brEvidencePrompt, observe as __brEvidenceObserve, initialize as __brEvidenceInitialize } from ${JSON.stringify(runtimeUrl)};\n`
    + source.slice(firstLine);
}
