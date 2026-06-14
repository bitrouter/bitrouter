#!/usr/bin/env node
// Minimal fake ACP agent for unit tests. Reads newline-delimited JSON-RPC on
// stdin, writes responses + notifications on stdout.
let buf = "";
function send(o) { process.stdout.write(JSON.stringify(o) + "\n"); }
function note(sessionId, update) {
  send({ jsonrpc: "2.0", method: "session/update", params: { sessionId, update } });
}
process.stdin.on("data", (d) => {
  buf += d.toString();
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    const msg = JSON.parse(line);
    if (msg.method === "initialize") {
      send({ jsonrpc: "2.0", id: msg.id, result: { protocolVersion: 1, agentCapabilities: {} } });
    } else if (msg.method === "session/new") {
      send({ jsonrpc: "2.0", id: msg.id, result: { sessionId: "ses_fake" } });
    } else if (msg.method === "session/prompt") {
      note("ses_fake", { sessionUpdate: "tool_call", toolCallId: "t1", title: "write /tmp/out.txt", status: "pending" });
      note("ses_fake", { sessionUpdate: "tool_call_update", toolCallId: "t1", status: "completed" });
      note("ses_fake", { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "done: wrote the file" } });
      send({ jsonrpc: "2.0", id: msg.id, result: { stopReason: "end_turn" } });
    } else if (msg.id !== undefined) {
      send({ jsonrpc: "2.0", id: msg.id, result: {} });
    }
  }
});
