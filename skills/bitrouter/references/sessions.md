# Per-session ACP substrate

How BitRouter's per-session substrate works — one process, one session, one agent. For CLI flags see `references/cli.md` §ACP sessions; for config fields see `references/providers.md` §ACP agents.

## Substrate vs manager framing

**Substrate = mechanism (one session).** `bitrouter acp serve|prompt` runs one stateful ACP session against one agent: spawn the upstream, drive turns (serialized), stream updates, broker permissions, optionally own a git worktree. One substrate process = one session = one agent.

**Manager = orchestration (N sessions).** The GUI, an AI manager-agent, or any other orchestrator coordinates multiple substrate processes. Each manager spawns one `bitrouter acp serve --agent <id>` process per session; the substrate never knows about other sessions.

## Two CLI modes

```bash
# Expose one session as a vanilla ACP Agent over stdio (manager-driven)
bitrouter acp serve --agent <id> [--worktree <name>] [--config PATH]

# One-shot headless: launch, send one prompt, stream NDJSON output, exit
bitrouter acp prompt --agent <id> [--worktree <name>] [--no-wait] [--config PATH] <text>
```

- **`serve`**: runs until the manager disconnects (stdin EOF). Stdout carries ACP JSON-RPC; logs go to stderr. The manager drives standard ACP: `initialize` → `session/new` → `session/prompt` / `session/cancel`.
- **`prompt`**: runs the same substrate engine in-process, sends one prompt, streams NDJSON to stdout, exits. Logs go to stderr.

### NDJSON format

Each line is a self-describing JSON object with a `type` field (snake_case):

| `type` | Meaning |
|---|---|
| `message_chunk` | Streaming text output |
| `thought_chunk` | Streaming thought/reasoning |
| `tool_call` | Agent initiated a tool call |
| `tool_call_update` | Update on an in-flight tool call |
| `result` | Terminal line — carries `stop_reason` (ACP wire spelling, e.g. `"end_turn"`) |
| `submitted` | Only with `--no-wait` — emitted after enqueue, then the process exits |

## One agent per session (D8)

Agent identity is fixed at launch via `--agent <id>`. There is no mid-session agent switch. The invariant: one substrate process ↔ one upstream agent process ↔ one ACP session. This is the inverse of BitRouter's LLM router: ACP session state is agent-private — switching would cause silent amnesia.

## FIFO turn queue + upstream cancel (D9)

Turns are serialized by a single-writer FIFO queue. A second prompt submitted while a turn is in flight queues (bounded; rejected past the cap). Turn cancellation is **upstream-level**: the engine calls ACP `session/cancel` on the upstream connection, which completes the in-flight turn cooperatively (`stop_reason: "cancelled"`). Cancel affects only the active turn; queued turns proceed normally.

## Three-tier identity (D10)

Each session carries three identity fields:

| Field | Source | Purpose |
|---|---|---|
| `record_id` | Locally generated (UUID) | Stable local handle, survives wire/provider changes |
| `acp_session_id` | Returned by upstream `session/new` | ACP protocol session identity |
| `agent_session_id` | Optional — from `_meta.agentSessionId`, never synthesized | Agent's own session handle; hook for v2 resume |

## Vanilla ACP, no extensions (D11)

The substrate speaks standard ACP on the wire — `initialize`, `session/new`, `session/prompt`, `session/cancel`, `session/update`, `session/request_permission`. There are no `_conductor/*` extensions. Agent and worktree are launch-time arguments, not wire methods; the manager chooses the agent by spawning the right command.

## Ephemeral v1 vs warm-owner v2

**v1** sessions are ephemeral: the session lives for the process's lifetime. When the process exits (manager disconnect or `acp prompt` completion), the session is gone.

**v2** (deferred): per-session warm owners — a lease file + IPC socket + heartbeat so a session can outlive the initial client. Recovery would use the three-tier identity (D10): respawn → ACP `session/resume(agent_session_id)` → `session/load` → `session/new`. The substrate design already reserves these hooks; v2 adds the machinery without a rewrite.

## v1 limitations

| Risk | Detail |
|---|---|
| `fs/*` / `terminal/*` (R1) | These ACP methods respond with `method-not-found` in v1. If an agent in the bundled catalog actually calls them, minimal handlers sandboxed to the worktree are added first. |
| Coarse permission outcomes (R5) | Permission answers flow through a 3-value `PermissionOutcome` (`AllowOnce` / `AllowAlways` / `Deny`). The manager's exact `optionId` is not preserved verbatim to the upstream — the down-handler maps to an outcome and the up-handler re-derives an option by kind. |
| Thin telemetry | v1 telemetry is `{agent, stop_reason}` only. Token counts and latency enrichment (via `UsageUpdate` and a `PreRequest` timestamp hook) are v2 follow-ups. |
| One-shot `acp prompt` | `acp prompt` is a single-turn command in v1. Multi-turn reuse over the CLI needs warm owners (v2). For a long conversation use `acp serve` and drive it with a manager. |
