# Per-session ACP substrate

How BitRouter's per-session substrate works — one process, one session, one agent. For CLI flags see `references/cli.md` §ACP sessions; for config fields see `references/providers.md` §ACP agents.

## Substrate vs manager framing

**Substrate = mechanism (one session).** `bitrouter acp serve|prompt` runs one stateful ACP session against one agent: spawn the upstream, drive turns (serialized), stream updates, broker permissions. One substrate process = one session = one agent.

**Manager = orchestration (N sessions).** The GUI, an AI manager-agent, or any other orchestrator coordinates multiple substrate processes. Each manager spawns one `bitrouter acp serve --agent <id>` process per session; the substrate never knows about other sessions.

## Two CLI modes

```bash
# Expose one session as a vanilla ACP Agent over stdio (manager-driven)
bitrouter acp serve --agent <id> [--turn-timeout SECS] [--config PATH]

# One-shot headless: launch, send one prompt, stream NDJSON output, exit
bitrouter acp prompt --agent <id> [--turn-timeout SECS] [--no-wait] \
  [--config PATH] <text>
```

**Routing (default on).** `bitrouter spawn <agent> -p|--serve` is the newer umbrella over `acp prompt|serve` (same code path; `acp` remains a stable alias). Both **route the sub-agent's LLM traffic through the daemon by default**, using per-harness knowledge from the shared catalog (so `bitrouter launch claude` and `bitrouter spawn claude-acp` inject identical gateway env/args). Opt out with `--direct`; pin the model with `--model`; override the gateway with `--base-url`; skip daemon auto-start with `--no-start`. If the daemon is unreachable (after auto-start) or `skip_auth: false` and no `BITROUTER_API_KEY` is set, the launch **fails fast before any session side effect** — a single NDJSON `{"type":"error","code":"daemon_unreachable"|"auth_required",…}` line (`-p`) or a stderr error (`--serve`). The `-p` stream's first line is a `session` correlation line carrying `record_id` + `via` (the daemon base URL, or `null` when direct). Catalog harnesses whose routing is config-synthesis only (`opencode`, `pi-acp`, `hermes-acp`, `openclaw` — routed in the `bitrouter launch` interactive facet; `hermes-acp` also routes headless when you export `HERMES_HOME` pointing at a dir whose `config.yaml` declares the loopback `custom` provider plus `CUSTOM_API_KEY`) and non-catalog agents warn and run direct. See `references/cli.md` → "Harness launch & spawn".

- **`serve`**: runs until the manager disconnects (stdin EOF). Stdout carries ACP JSON-RPC; logs go to stderr. The manager drives standard ACP: `initialize` → `session/new` (cwd + `mcpServers` relayed upstream) → `session/prompt` / `session/cancel`.
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

## Session records

Every launch writes `.bitrouter/sessions/<record_id>.json` — three-tier identity, pid, start/end timestamps, status — and shutdown settles it to `exited`. Records are written **atomically** (temp + rename). A `running` record whose pid is gone is stale — the substrate was killed without shutdown. The `.bitrouter/` state dir is created **self-ignoring** (a `.gitignore` containing `*`, cargo-style), so records never land in version control by accident.

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

The substrate speaks standard ACP on the wire — `initialize`, `session/new`, `session/prompt`, `session/cancel`, `session/update`, `session/request_permission`. There are no `_conductor/*` extensions. The agent is a launch-time argument, not a wire method; the manager chooses the agent by spawning the right command.

Fidelity guarantees on that wire:

- **Capabilities relay**: the manager-facing `initialize` reflects the upstream agent's real capabilities (and `agentInfo`); `loadSession` is masked to `false` (this endpoint does not honor `session/load`) and auth methods are withheld.
- **Prompts forward verbatim**: `session/prompt` content blocks (text, images, resources, resource links) reach the upstream unmodified.
- **Exact permission outcomes**: the manager's chosen `optionId` passes through to the upstream verbatim (validated against the offered set); two same-kind options stay distinguishable. Dropping/failing to answer defaults to the reject option.

## v1 limitations

| Risk | Detail |
|---|---|
| `fs/*` / `terminal/*` | Answered `method-not-found`, **by design**: ACP v2 removes this client surface (low adoption). The blessed channel is client-side MCP servers, which the substrate relays — the manager's `session/new` `mcpServers` reach the upstream agent verbatim. |
| Telemetry granularity | Per-turn records carry `{agent, stop_reason, latency_ms, context used/size}` (to stderr). Per-turn input/output token *deltas* are not in ACP's stable surface (only the `unstable_end_turn_token_usage` feature), so cost attribution finer than the streamed cumulative `cost` is deferred. |
| One-shot `acp prompt` | `acp prompt` is a single-turn command in v1. For a long conversation use `acp serve` and drive it with a manager. |
