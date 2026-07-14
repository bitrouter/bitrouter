# Per-session ACP substrate

How BitRouter's per-session substrate works — one process, one session, one agent. For CLI flags see `references/cli.md` §ACP sessions; for config fields see `references/providers.md` §ACP agents.

## Substrate vs manager framing

**Substrate = mechanism (one session).** `bitrouter acp serve|prompt` runs one stateful ACP session against one agent: spawn the upstream, drive turns (serialized), stream updates, broker permissions, optionally own a git worktree. One substrate process = one session = one agent.

**Manager = orchestration (N sessions).** The GUI, an AI manager-agent, or any other orchestrator coordinates multiple substrate processes. Each manager spawns one `bitrouter acp serve --agent <id>` process per session; the substrate never knows about other sessions.

## Two CLI modes

```bash
# Expose one session as a vanilla ACP Agent over stdio (manager-driven)
bitrouter acp serve --agent <id> [--worktree <name>] [--rm-worktree] [--no-transcript] \
  [--turn-timeout SECS] [--warm] [--idle-timeout SECS] [--config PATH]

# One-shot headless: launch, send one prompt, stream NDJSON output, exit
bitrouter acp prompt --agent <id> [--worktree <name>] [--rm-worktree] [--no-transcript] \
  [--turn-timeout SECS] [--no-wait] [--config PATH] <text>

# List this repo's session records (.bitrouter/sessions/), newest first
bitrouter acp sessions

# Reattach a terminal to a warm session (stdio ↔ unix socket bridge)
bitrouter acp attach <record-id-or-prefix>
```

**Routing (default on).** `bitrouter spawn <agent> -p|--serve` is the newer umbrella over `acp prompt|serve` (same code path; `acp` remains a stable alias). Both **route the sub-agent's LLM traffic through the daemon by default**, using per-harness knowledge from the shared catalog (so `bitrouter launch claude` and `bitrouter spawn claude-acp` inject identical gateway env/args). Opt out with `--direct`; pin the model with `--model`; override the gateway with `--base-url`; skip daemon auto-start with `--no-start`. If the daemon is unreachable (after auto-start) or `skip_auth: false` and no `BITROUTER_API_KEY` is set, the launch **fails fast before any session side effect** — a single NDJSON `{"type":"error","code":"daemon_unreachable"|"auth_required",…}` line (`-p`) or a stderr error (`--serve`). The `-p` stream's first line is a `session` correlation line carrying `record_id` + `via` (the daemon base URL, or `null` when direct). Catalog harnesses whose routing is config-synthesis only (`opencode`, `pi-acp` — routed in the `bitrouter tui` orchestrator facet) and non-catalog agents warn and run direct. See `references/cli.md` → "Harness launch & spawn".

- **`serve`**: runs until the manager disconnects (stdin EOF) — or, with `--warm`, until the idle timeout. Stdout carries ACP JSON-RPC; logs go to stderr. The manager drives standard ACP: `initialize` → `session/new` (cwd + `mcpServers` relayed upstream) → `session/prompt` / `session/cancel` / `session/load`.
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

## Session records + transcript

Every launch writes `.bitrouter/sessions/<record_id>.json` — three-tier identity, worktree path, **branch + `base_ref`** (the base-repo `HEAD` a newly created worktree branch was cut from — the durable diff/merge base; absent when an existing branch/worktree was attached), pid, start/end timestamps, status — and shutdown settles it to `exited`. Records are written **atomically** (temp + rename). `bitrouter acp sessions` lists them; a `running` record whose pid is gone displays as `dead` (the substrate was killed without shutdown). The `.bitrouter/` state dir is created **self-ignoring** (a `.gitignore` containing `*`, cargo-style), so records and transcripts never land in version control by accident.

**Fleet memory.** `bitrouter tui` additionally maintains `.bitrouter/fleet-state.json` — the manager layer's durable memory across stops and crashes: the orchestrator sessions (`sessions: [{binary, model}]`, spawn order), and per agent (keyed by `record_id`) the autonomy tier, ready-to-review stat, allocated `PORT`, any pending-permission title (a stop denies it), and unsent composer drafts. Written atomically at most once a second while state changes and once at teardown with `clean_shutdown: true` — a `false` in a file whose writer is gone means a crash. This is memory, **not** auto-resume: nothing relaunches from it; the TUI surfaces a one-line notice about the previous fleet at startup, and any reader can join `record_id` back to the session records and transcripts.

Alongside it, a **durable transcript** (`<record_id>.transcript.ndjson`, disable with `--no-transcript`) records the whole conversation non-lossily: `prompt` (verbatim content blocks), `update` (every raw ACP `session/update`), `result` / `error` per turn. Each line is stamped `{seq, ts}` with a writer-minted monotonic `seq` — the cursor shape ACP v2's `session/resume { replayFrom }` replays from. Records + transcript together are the persistence substrate for v2 warm sessions.

## One agent per session (D8)

Agent identity is fixed at launch via `--agent <id>`. There is no mid-session agent switch. The invariant: one substrate process ↔ one upstream agent process ↔ one ACP session. This is the inverse of BitRouter's LLM router: ACP session state is agent-private — switching would cause silent amnesia.

## FIFO turn queue + session-scoped cancel (D9)

Turns are serialized by a single-writer FIFO queue. A second prompt submitted while a turn is in flight queues (bounded; rejected past the cap). Cancellation is **session-scoped**, matching ACP `session/cancel`: the queued backlog is flushed (each queued prompt resolves `stop_reason: "cancelled"` without running) and the active turn is cancelled cooperatively at the upstream. An optional per-turn deadline (`--turn-timeout SECS`) triggers the same cooperative cancel on elapse, with a 3s grace before the turn errors.

## Three-tier identity (D10)

Each session carries three identity fields:

| Field | Source | Purpose |
|---|---|---|
| `record_id` | Locally generated (UUID) | Stable local handle, survives wire/provider changes |
| `acp_session_id` | Returned by upstream `session/new` | ACP protocol session identity |
| `agent_session_id` | Optional — from `_meta.agentSessionId`, never synthesized | Agent's own session handle; hook for v2 resume |

## Vanilla ACP, no extensions (D11)

The substrate speaks standard ACP on the wire — `initialize`, `session/new`, `session/load`, `session/prompt`, `session/cancel`, `session/update`, `session/request_permission`. There are no `_conductor/*` extensions. Agent and worktree are launch-time arguments, not wire methods; the manager chooses the agent by spawning the right command.

Fidelity guarantees on that wire:

- **Capabilities relay**: the manager-facing `initialize` reflects the upstream agent's real capabilities (and `agentInfo`); `loadSession` is the substrate's own (advertised exactly when a transcript exists — replay is ours, not the upstream's) and auth methods are withheld.
- **Prompts forward verbatim**: `session/prompt` content blocks (text, images, resources, resource links) reach the upstream unmodified.
- **Exact permission outcomes**: the manager's chosen `optionId` passes through to the upstream verbatim (validated against the offered set); two same-kind options stay distinguishable. Dropping/failing to answer defaults to the reject option.

## Warm sessions (`serve --warm`)

By default a session lives for the process's lifetime and dies with its manager. With `--warm`, the session survives manager disconnects: reattach connections are accepted on a per-session unix socket (advertised in the session record; bound under `$BITROUTER_HOME/sock`) until `--idle-timeout` elapses with no manager. A reattaching manager (`bitrouter acp attach <record>` or any ACP client on the socket) runs `initialize` → `session/load` — which replays the durable transcript — and continues live. The socket speaks the exact stdio NDJSON JSON-RPC framing (no bespoke protocol); ACP's standardized remote transport (streamable HTTP / WebSocket RFD) replaces the socket when it ships. *Recovery* (respawning a dead agent via `agent_session_id`) remains future work.

## v1 limitations

| Risk | Detail |
|---|---|
| `fs/*` / `terminal/*` | Answered `method-not-found`, **by design**: ACP v2 removes this client surface (low adoption). The blessed channel is client-side MCP servers, which the substrate relays — the manager's `session/new` `mcpServers` reach the upstream agent verbatim. |
| Telemetry granularity | Per-turn records carry `{agent, stop_reason, latency_ms, context used/size}` (to stderr). Per-turn input/output token *deltas* are not in ACP's stable surface (only the `unstable_end_turn_token_usage` feature), so cost attribution finer than the streamed cumulative `cost` is deferred. |
| One-shot `acp prompt` | `acp prompt` is a single-turn command in v1. Multi-turn reuse over the CLI needs warm owners (v2). For a long conversation use `acp serve` and drive it with a manager. |
