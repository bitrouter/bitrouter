# Per-session ACP substrate

How BitRouter's per-session substrate works — one process, one session, one agent. For CLI flags see `references/cli.md` §ACP sessions; for config fields see `references/providers.md` §ACP agents.

## Substrate vs manager framing

**Substrate = mechanism (one session).** `bitrouter acp serve|prompt` runs one stateful ACP session against one agent: spawn the upstream, drive turns (serialized), stream updates, broker permissions, optionally own a git worktree. One substrate process = one session = one agent.

**Manager = orchestration (N sessions).** The GUI, an AI manager-agent, or any other orchestrator coordinates multiple substrate processes. Each manager spawns one `bitrouter acp serve --agent <id>` process per session; the substrate never knows about other sessions.

## Two CLI modes

```bash
# Expose one session as a vanilla ACP Agent over stdio (manager-driven)
bitrouter acp serve --agent <id> [--worktree <name>] [--rm-worktree] [--config PATH]

# One-shot headless: launch, send one prompt, stream NDJSON output, exit
bitrouter acp prompt --agent <id> [--worktree <name>] [--rm-worktree] [--no-wait] [--config PATH] <text>

# List this repo's session records (.bitrouter/sessions/), newest first
bitrouter acp sessions
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
| `usage` | Context-window occupancy from the upstream's `UsageUpdate` — `used`/`size` tokens, optional cumulative `cost` |
| `result` | Terminal line — carries `stop_reason` (ACP wire spelling, e.g. `"end_turn"`) |
| `submitted` | Only with `--no-wait` — emitted after enqueue, then the process exits |

## Worktrees: retained by default

`--worktree <name>` provisions `.bitrouter/worktrees/<name>` — created with a same-named branch, or **reused** when the worktree already exists (attaching to an existing branch instead of failing). At shutdown the worktree is **retained** (it holds the agent's work; the path is logged to stderr). `--rm-worktree` opts in to removal, which destroys uncommitted work; a pre-existing (reused) worktree is never removed. `serve` and `prompt` share these semantics.

## Session records

Every launch writes `.bitrouter/sessions/<record_id>.json` — three-tier identity, worktree path, pid, start/end timestamps, status — and shutdown settles it to `exited`. `bitrouter acp sessions` lists them; a `running` record whose pid is gone displays as `dead` (the substrate was killed without shutdown). Records are the persistence hook for v2 `session/load`.

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

Fidelity guarantees on that wire:

- **Capabilities relay**: the manager-facing `initialize` reflects the upstream agent's real capabilities (and `agentInfo`), with `loadSession` masked to `false` (no `session/load` in v1) and auth methods withheld.
- **Prompts forward verbatim**: `session/prompt` content blocks (text, images, resources, resource links) reach the upstream unmodified.
- **Exact permission outcomes**: the manager's chosen `optionId` passes through to the upstream verbatim (validated against the offered set); two same-kind options stay distinguishable. Dropping/failing to answer defaults to the reject option.

## Ephemeral v1 vs warm-owner v2

**v1** sessions are ephemeral: the session lives for the process's lifetime. When the process exits (manager disconnect or `acp prompt` completion), the session is gone.

**v2** (deferred): per-session warm owners — a lease file + IPC socket + heartbeat so a session can outlive the initial client. Recovery would use the three-tier identity (D10): respawn → ACP `session/resume(agent_session_id)` → `session/load` → `session/new`. The substrate design already reserves these hooks; v2 adds the machinery without a rewrite.

## v1 limitations

| Risk | Detail |
|---|---|
| `fs/*` / `terminal/*` (R1) | These ACP methods respond with `method-not-found` in v1. The planned fix is pass-through (relay the manager's `ClientCapabilities` upstream and proxy agent→client `fs`/`terminal` requests down to the manager), which requires deferring the upstream handshake until the manager's `initialize` — a follow-up. |
| Telemetry granularity | Per-turn records carry `{agent, stop_reason, latency_ms, context used/size}` (to stderr). Per-turn input/output token *deltas* are not in ACP's stable surface (only the `unstable_end_turn_token_usage` feature), so cost attribution finer than the streamed cumulative `cost` is deferred. |
| One-shot `acp prompt` | `acp prompt` is a single-turn command in v1. Multi-turn reuse over the CLI needs warm owners (v2). For a long conversation use `acp serve` and drive it with a manager. |
