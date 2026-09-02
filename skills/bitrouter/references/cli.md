# CLI reference

Every subcommand the v1 binary actually exposes. Anything not listed here doesn't exist — don't suggest `bitrouter doctor`, `bitrouter providers add`, `bitrouter cloud connect`, or the old auth subcommand tree (cloud identity is `bitrouter cloud whoami`, see below).

Bare `bitrouter` (no subcommand) is the onboarding front door: it runs the network-free credential probe and either launches the setup wizard (unconfigured) or prints a one-line status + a `bitrouter launch` hint (configured), exit 0 either way. See `bitrouter init` under *Setup helpers*.

## Daemon lifecycle

| Command | Effect |
|---|---|
| `bitrouter serve [--config PATH]` | Run the HTTP server + control socket **in the foreground**. Ctrl-C to stop. |
| `bitrouter start [--config PATH] [--log PATH]` | Spawn `serve` as a detached background process. Stdout/stderr go to `~/.bitrouter/bitrouter.log` unless `--log` overrides. Refuses to start over a live daemon. |
| `bitrouter stop [--config PATH] [--socket PATH]` | Graceful shutdown via the control socket. |
| `bitrouter restart [--config PATH] [--log PATH] [--socket PATH]` | Stop, wait up to 30s for in-flight requests to drain, then start. Escalates to SIGKILL on timeout. |
| `bitrouter reload [--config PATH] [--socket PATH]` | Hot-reload the running daemon's config + routing table. **Also re-pushes provider env vars** from the current shell into the daemon, so `export OPENAI_API_KEY=new...; bitrouter reload` rotates the key without a restart. SIGHUP reloads daemon-side config but cannot forward newly exported shell variables. |
| `bitrouter status [--config PATH] [--socket PATH]` | `systemctl status`-style block: pid / listen / model count / socket. Reports `stopped` (exit 0) when no daemon is reachable. |
| `bitrouter status --requests` (`-r`) | Newest-first settled requests (time, model, provider actually used, tokens, cost, latency, status) + the window's spend and trailing-minute rate. **JSON by default** like every other command; `--human` renders the table. Reads the metering store directly, so it works with no daemon (`mode: history_only`). The spend rollup states its `scope` (`all callers`, not one session) and reports `null` / `unreported` when no request has charge evidence rather than `$0.00`. Rows carry `charge_status` and `episode_id` (the handle for `bitrouter trajectory inspect`, `null` when capture is off). Prints once and exits. Portable. Replaces the removed `--watch` live view; use `watch -n1 … --human` to repeat it. |

## Inspection

| Command | Effect |
|---|---|
| `bitrouter route <model> [--config PATH]` | Resolve a model name through the routing table. Tries the running daemon first (live table), falls back to standalone config resolution. Prints the provider/service chain. |
| `bitrouter models [--config PATH] [--provider ID]` | List every routable model the config exposes. Filter by provider. |
| `bitrouter providers list [--config PATH]` | Tab-aligned: `ID  MODELS  ACTIVE  API_BASE`. |
| `bitrouter tools list [--config PATH]` | Enumerate tools advertised by every configured MCP server (one `tools/list` round-trip per server). |
| `bitrouter tools status [--config PATH]` | Health-check each MCP server. Latency or error per row. |
| `bitrouter tools discover <server> [--config PATH]` | Print a YAML stub for the discovered server, paste into `mcp_servers:`. |
| `bitrouter mcp search <query> [--limit N]` | Search official MCP registry names (registry.modelcontextprotocol.io) server-side. Rows: name / version / install support (`remote` zero-install, `npx`/`uvx` version-pinned stdio stub, `manual` other/incomplete package type) / description. Only `active` + latest-version entries. |
| `bitrouter mcp list [--limit N]` | List the official MCP registry with the same install-support column. Responses cache for 24h under `$XDG_CACHE_HOME/bitrouter/mcp-registry/` (stale fallback when the registry is unreachable). |
| `bitrouter mcp add <name>` | Print a paste-ready YAML stub for `mcp_servers:` — prefers a zero-install `streamable-http` remote when published (header `{var}` templates become `${VAR}` env refs), else stubs explicit, version-pinned npm/PyPI stdio packages with required env vars as `""` placeholders. Other package types and incomplete/unpinned entries are refused with a manual-install pointer. |
| `bitrouter agents list [--remote] [--config PATH]` | Show bundled ACP catalog + which are configured. `--remote` also fetches the official ACP agent registry (cdn.agentclientprotocol.com) and lists its agents with version + install support (`npx`/`uvx` stub-able; `manual` for binary-only). |
| `bitrouter agents check [--config PATH]` | Spawn each configured ACP agent and verify `initialize` round-trip. |
| `bitrouter agents install <id>` | Print a paste-ready YAML stub for `<id>` — resolved from the bundled catalog first, then the ACP registry (`npx`/`uvx` distributions, version-pinned, `env` included). Binary-only registry entries are refused with a manual-install pointer (the registry has no checksums). |
| `bitrouter observe status [--json] [--config PATH] [--socket PATH]` | OTel exporter snapshot: wired / endpoint / sampler / cardinality usage / in-flight spans. JSON output for tooling. |

## Durable trajectory operations

Trajectory capture is a local, explicit opt-in and is generic across tasks and
protocols. It never requires task datasets or private routing headers.

```yaml
trajectory:
  enabled: true
  retention_days: 30       # positive; default 30
  outbox_batch_size: 100   # 1..=1000; default 100
```

BitRouter fails closed if a signed lock contains any `progress_guard` while
`trajectory.enabled` is false. Every trajectory setting is restart-only:
reload rejects changes to `enabled`, `retention_days`, or `outbox_batch_size`
and preserves the live last-known-good state. Restart the daemon to apply any
of them. When enabled, capture uses the existing local correlation-key
lifecycle and starts durable outbox publication and startup retention. When
disabled, it creates no correlation key and writes no trajectory ledger rows.

Provider-native Responses continuation remains always active even when
trajectory is disabled. Its independent lifecycle settings are:

```yaml
continuation:
  retention_days: 30       # positive; default 30
  prune_batch_size: 1000   # 1..=10000; default 1000
```

These settings are restart-only. The registry encrypts provider response IDs;
clients see only canonical `brc_` continuation IDs.

| Command | Effect |
|---|---|
| `bitrouter trajectory [--config <PATH>] inspect <EPISODE_ID>` | Resolve the globally unique episode, then report owner-scoped correlation/completeness, structural health, active hold, typed route clauses, and event digests. |
| `bitrouter trajectory [--config <PATH>] replay <EPISODE_ID>` | Audit a stable snapshot and compare the newest persisted route checkpoint digest with replay. Corruption output uses a stable reason code and the first intrinsically invalid event id/sequence only. |
| `bitrouter trajectory [--config <PATH>] prune --before <RFC3339> --dry-run` | Return exact global eligible counts without mutation. |
| `bitrouter trajectory [--config <PATH>] prune --before <RFC3339>` | Transactionally prune delivered old outbox rows and eligible terminal episode history in configured batches. |

`--config <PATH>` is optional and may appear before or after the trajectory leaf
command. Without it, all commands use the standard config resolution chain.
They open the selected source's database: a relative SQLite URL is anchored to
the config file's directory, or to the implicit BitRouter home (normally
`~/.bitrouter`) for zero-config. Resolution neither depends on nor changes the
caller's working directory; absolute/memory SQLite and server URLs are left
unchanged. The global `--json` / `--human` flags work before or after the
subcommand, and no operation takes an owner argument. Inspect/replay resolve the
owner once and keep all subsequent reads owner-filtered. Audit retries a bounded
head→events→head stable read, so a concurrent append yields a valid before/after
snapshot or a contention error, never false corruption. Its prefix reducer
distinguishes a valid route awaiting guard activation from intrinsic route or
guard corruption, so the reported first bad event is exact.

Retention uses an exclusive cutoff. An episode is eligible only when its last
capture is older, every request is settled or failed, and no request points at a
pending outbox row. Explicitly closed episodes must also have an old close time.
Deletion is owner/identity guarded and transactional; no synthetic close event
is written. Startup applies the same rules from `retention_days`. A late native
continuation after pruning becomes a new `unresolved` / `incomplete` episode.

Privacy is write-time, not display-time redaction: durable event JSON, outbox
payloads, Eval evidence, logs, and both report formats contain structural facts,
fixed categories/reason codes, counters, and digests only. They exclude
API/Bearer secrets, prompts, system instructions, tool arguments, file bodies,
and private provider metadata. Trajectory request IDs and Responses native-parent
IDs use the same installation-keyed, owner-bound opaque identity domain. This
does not change external request IDs carried on the wire, sent upstream, or used
for metering joins.

Built-in operational Eval results are always `inconclusive`: metrics describe
counts, streaks, elapsed time, and optional authoritative token/cost facts—not
task quality. Missing metering remains absent instead of becoming zero.
`trajectory.history_complete` is true only for a proven complete prefix;
incomplete/unknown history is not treated as complete and follows the guard's
configured incomplete-history behavior.

Recovery sequence: back up the configured DB → run prune `--dry-run` → inspect
valuable episodes → replay them → run real prune. Retry ordinary audit
contention after traffic quiets. Investigate stable corruption codes or restore
from backup.

## Generic eval exchange

These commands operate on the local append-only evidence ledger. They never
edit or publish `policy-lock.yaml`.

| Command | Effect |
|---|---|
| `bitrouter eval subject seal <DRAFT> --output <SEALED>` | Calculate the canonical digest of redacted evidence, validate the completed subject, and write deterministic JSON. This local file operation never opens the evidence ledger. |
| `bitrouter eval subject put <FILE> [--config PATH]` | Insert an immutable JSON/YAML request, episode, or task subject. |
| `bitrouter eval subject get <EVAL_ID> [--config PATH]` | Read one subject. |
| `bitrouter eval subject list [--config PATH]` | List subjects, including automatically observed routed requests. |
| `bitrouter eval result submit <FILE> [--config PATH]` | Submit a JSON/YAML evaluator result as the local operator; runs the same admission logic as REST. |
| `bitrouter eval snapshot freeze [--at RFC3339] [--config PATH]` | Freeze currently admitted results into a content-addressed manifest. |
| `bitrouter eval snapshot get <SHA256> [--config PATH]` | Read an immutable snapshot manifest. |
| `bitrouter eval status [--config PATH]` | Count subjects and latest admission states. |

Authenticated daemon endpoints mirror the exchange at
`/v1/evals/subjects`, `/v1/evals/results`, `/v1/evals/snapshots`, and
`/v1/evals/status`. External evaluators submit scores; BitRouter owns schema,
identity/metric authority, conflict/holdout admission, and snapshots.
The CLI uses the local ownership scope; authenticated REST is isolated by the
virtual key's owning user. Snapshot roots commit both subject and result content.

## ACP sessions

Two ACP execution modes share launch routing but not session ownership. `acp serve` is a connection-level controller: one process owns one harness connection carrying multiple harness-native sessions. `acp prompt` is the local one-shot session engine. `bitrouter spawn <agent> --serve|-p` is the umbrella over these; `acp serve|prompt` remain stable aliases. Both **attempt to route the harness's model traffic through the daemon when the headless adapter supports redirection** — add `--direct` / `--base-url` / `--model` / `--no-start` (see "Harness launch & spawn").

| Command | Effect |
|---|---|
| `bitrouter acp serve --agent <id> [--turn-timeout SECS] [--direct] [--base-url URL] [--model ID] [--no-start] [--config PATH]` | Run one ACP controller over **stdio** until the manager disconnects. The manager initializes first, then may open multiple native sessions and use every lifecycle method the harness advertises. Session IDs, history, and storage remain harness-owned. Logs go to stderr; stdout carries ACP JSON-RPC. `--turn-timeout` is a local-engine option and does not rewrite transparent controller turns. |
| `bitrouter acp prompt --agent <id> [--turn-timeout SECS] [--no-wait] [--direct] [--base-url URL] [--model ID] [--no-start] [--config PATH] <text>` | Launch a session, send one prompt, stream session updates to **stdout as NDJSON** (one JSON object per line), then exit. First line is the `session` correlation line (see below). Logs go to stderr. `--no-wait` submits and returns `{"type":"submitted"}` without streaming. Attempts to route through the daemon when the headless adapter supports redirection (`--direct` opts out). |

**Controller lifecycle**: manager `initialize` capabilities and `_meta` reach the harness exactly. Each `session/new` is forwarded and returns that harness response's opaque `sessionId`; repeated calls may create different sessions. Advertised `session/list|load|resume|fork|close|delete`, prompts, cancellations, callbacks, updates, errors, `_meta`, and extension payloads pass through. BitRouter neither mints a manager-facing session alias nor keeps a session catalog.

**Endpoint setup**: Claude uses pinned `@agentclientprotocol/claude-agent-acp@0.70.0`; Codex uses pinned `@agentclientprotocol/codex-acp@1.7.0`. Controller-to-harness `providers/*` configures the model endpoint and is removed from manager-facing capabilities. Claude's fallback is `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / newline-separated `ANTHROPIC_CUSTOM_HEADERS`; Codex's is `CODEX_CONFIG` plus `MODEL_PROVIDER`, with no ACP-mode `-c` arguments.

**Session route control**: a locally bound controller advertises stable-v1
`_bitrouter/route/list|set|reset` methods under initialize response
`_meta["bitrouter.dev/controller"].routeControl`. They operate on opaque native
`sessionId` values and create daemon-confirmed ephemeral leases; manager-side
`providers/*` remains unavailable. `--direct` and explicit remote `--base-url`
connections do not advertise route control because they have no trusted local
daemon binding. `route/list.available` is a logical-model suggestion list;
presets and permitted explicit routes remain validated free-form `set` inputs.

**Session cost**: the same binding advertises
`_meta["bitrouter.dev/controller"].usage` (`version: "1"`, `scope: "session"`,
`fields: ["cost"]`, `provenance: "bitrouter.dev/cost"`). The controller then
decorates the harness's own `usage_update` notifications: `used` and `size`
pass through untouched and `cost` becomes the spend BitRouter metered for that
native session and its child agents, marked
`update._meta["bitrouter.dev/cost"] = "router"`. No usage update is ever
synthesized, and unmetered traffic (`--direct`, explicit `--base-url`, own-auth
harnesses, sessions with no priced requests) is forwarded exactly as sent —
never `$0.00`, never a daemon-wide figure.

**Observability and turns**: `acp serve` forwards the harness's session/cancel and session/update wire unchanged, except that a locally bound controller decorates the harness's own `usage_update` with session-attributed `cost` (see **Session cost**); it never synthesizes per-session usage or timeout behavior. Authenticated routed model calls normalize BitRouter's static controller/harness headers and the harness's native Claude/Codex identity into controlled capture/replay, request spans, route decisions, and nullable metering correlation. Authorization, cookies, and controller credentials remain excluded. `acp prompt` and `chat` retain the local `record_id`, OTel spans, FIFO queue, cooperative cancellation, and `--turn-timeout` behavior.

**NDJSON format** (for `acp prompt` / `spawn -p`): the **first** line is a `session` correlation line — `{"type":"session","record_id":"…","agent":"…","via":"http://127.0.0.1:4356"}` (`via` is `null` when `--direct`) — for joining the session to daemon cost/metering. Each update line is then a self-describing JSON object with a `type` field (snake_case): `message_chunk`, `thought_chunk`, `tool_call`, `tool_call_update`, `usage` (context-window occupancy: `used`, `size`, optional `cost`). The terminal line is `{"type":"result","stop_reason":"end_turn"}` (ACP wire spelling). In `--no-wait` mode only `{"type":"submitted"}` follows the session line. A fail-fast routing failure emits a single `{"type":"error","code":"daemon_unreachable"|"auth_required","via":…,"hint":…}` line instead, before any session is created.

**Result contract** (`spawn -p --result-schema '<JSON Schema>'`, or `@path` to read it from a file; conflicts with `--no-wait`): the schema rides the subagent's prompt as an instruction to end the reply with a ```json fenced block. The reply's **last** ```json block (or a bare-JSON reply) is extracted and validated; on a missing/invalid result the subagent gets **one** repair re-prompt. The terminal line then carries the machine-consumable outcome — success: `{"type":"result","stop_reason":…,"result":{…},"schema_ok":true}`; failure after repair: `…,"result":null,"schema_ok":false,"raw":"<last reply text>"` (the orchestrator is never blocked). Bare `spawn -p` output is unchanged (no `result`/`schema_ok`/`raw` keys). A malformed schema fails fast before any session side effect.

See `references/sessions.md` for the controller/native-session boundary and the separate one-shot engine model.

## Interactive chat (`bitrouter chat`)

The interactive counterpart to `acp serve`: same launch, same routing flags, same per-session log, but the session is rendered for a person instead of exposed to a manager over stdio.

| Command | What it does |
|---|---|
| `bitrouter chat <agent> [--turn-timeout SECS] [--direct] [--base-url URL] [--model ID] [--no-start] [--config PATH]` | Render one ACP agent session in the terminal: streamed messages, reasoning, tool calls with diffs, permission prompts, and how full the context window is (`ctx 12.4k/200k`, highlighted at 80%). The cost line reads `cost unreported`: `chat` still runs the local engine rather than connecting to the controller, so nothing attributes spend to it — see **Session cost** for the `acp serve` path, which does. Draws inline — output lands in real scrollback, so terminal search/selection keep working and exiting leaves the transcript. |

**Raw mode**: `chat` holds the terminal in raw mode for the whole session, so enter, `Ctrl-C` and `Ctrl-D` are its own line editor's rather than the shell's. Typing, backspace, word-delete and bracketed paste; no history, no multi-line. A redirected stdout takes no raw mode and prints plain text with no escape sequences.

**Keys**

| Key | Effect |
|---|---|
| `Enter` | Send the line |
| `Esc` | Deny the open permission prompt, or close the picker; with neither open, cancel the running turn |
| `Ctrl-C` | Cancel the running turn; end the session when idle |
| `Ctrl-D` | End the session (idle only) |
| `Ctrl-L` | Repaint the screen |
| `Ctrl-W` | Delete the previous word |

Cancelling a turn with a permission outstanding **denies it** — a cancel is never read as consent.

**In-session commands**: `/route` opens the provider picker; `/commands` lists the slash commands the agent itself advertises.

**A scrolled-off row can be stale**: rows are repainted only while they are on screen, so a tool call that scrolls away mid-run keeps the status it had when it left. The renderer will not clear your scrollback to fix that. `Ctrl-L` repaints what is on screen.

**Cost honesty**: the cost line always carries its scope. `USD 0.4200` is this session's spend; `all callers USD 1.3200` means the traffic could not be attributed (you supplied your own credential, which BitRouter never rewrites to tag) so the figure is the daemon's window total; `cost unreported` means the agent sent no cost — it is never rendered as `$0.00`.

**`/route`**: lists routable providers and switches the live session's route. Offered **only** when the session can honour it — that needs a daemon to install the override in and an attributable launch id to scope it by, so a `--direct` session or one using your own credential is told plainly that it cannot be rerouted. After a switch, `chat` re-reads `providers/list` and reports what the daemon is actually serving; a refused change reports the old route and why.

**On failure**: a failed turn prints the tail of `~/.bitrouter/logs/session-<stamp>-<pid>.log` after the session ends and names the path. There is no permanent log pane. `chat` logs **only** to that file — it owns the terminal, so a stray log line would scroll the screen out from under the renderer.

## Setup helpers

| Command | Effect |
|---|---|
| `bitrouter init [flags]` | Guided onboarding wizard: **credentials** → **harness** → **finish** (launch / serve+snippet / exit). Interactive by default; `--yes` runs it headlessly — process the flags below, never block on a human, emit the JSON result envelope (`action: onboarding`, `providers_configured`, `providers_skipped_interactive`, `harnesses_installed`, `after`, `snippet`), and scaffold the starter `bitrouter.yaml` (`skip_auth: true`, `listen: 127.0.0.1:4356`, common providers stubbed `{}`). The scaffold refuses to overwrite unless `--force`; `--reset` clears stored credentials first (cloud session always, provider creds after a confirm / unconditionally under `--yes`). Flags mirror every prompt: `--cloud-login`, `--api-key <brk_…>` (cloud), `--provider <id>` + `--provider-api-key <k>` (repeatable), `--use-detected`, `--harness claude\|codex` (repeatable), `--no-install`, `--after launch\|serve\|exit`, `--model <id>`, `--write-config`, `-c/--config PATH`. Under `--yes`, anything needing interactive OAuth (bare `--cloud-login`, a `--provider` with no key) is reported in `providers_skipped_interactive`, not attempted. |
| `bitrouter config validate [--config PATH]` | Validate a config file by running the real parse path: structure (deserialization), `derives` resolution, the upstream-URL (SSRF) gate, and any referenced `policy-lock.yaml`. Exits non-zero on an invalid config — **CI-safe**. Does *not* load the JSON Schema (that artifact, at `dist/schema/bitrouter.config.schema.json` / regenerated with `cargo run -p dist-helper -- generate-schema`, is for IDE autocomplete + the drift check). Unset `${VAR}` references are substituted with a `.invalid` placeholder and reported as warnings, so secrets need not be present; a value that embeds one mid-string is not authoritatively checked. |
| `bitrouter skills list [--global] [--json\|--human]` | List installed skills. Reads the project skills root by default; `--global` reads `~/.claude/skills/`. |
| `bitrouter skills init <NAME> [--output PATH] [--json\|--human]` | Scaffold a spec-valid skill directory — writes `<NAME>/SKILL.md` unless `--output` names a path. `<NAME>` is written into the generated frontmatter. |
| `bitrouter policy create <id> [--dir DIR]` | Write a starter access-control policy file under `--dir` (default `./policies`). Bind to a key with `bitrouter key sign --user <id> --policy <id>`. |
| `bitrouter policy init <name> --preset <preset> --economy <model> [--economy-effort <level>] [--strong <model>] [--strong-effort <level>] [--config PATH]` | Create or extend deterministic `policy-lock.yaml`, bind the named policy to a preset, and set the process configuration to `policy.mode: adaptive`. Model-only targets retain scalar compatibility; an explicit supported effort makes `(model, effort)` the target identity, so the same model can occupy strong and economy at different levels. The strong model is inferred from an existing preset when omitted; `--strong-effort` therefore requires explicit `--strong`. Presets use `@preset[:variant]`; `templates/auto-router` binds the `auto` preset, published as `bitrouter/auto` and `bitrouter/auto:cost`. |
| `bitrouter policy check|status [--config PATH]` | Cross-validate the main config and lock, or report the resolved path, semantic digest, runtime mode, policies, and preset bindings. |
| `bitrouter policy show <name> [--config PATH]` | Print one validated effective policy. |
| `bitrouter policy compile --output FILE [--eval-snapshot SHA256] [--snapshot-time UNIX_MS] [--config PATH]` | Compile legacy migration evidence and an optional frozen generic-eval snapshot into a deterministic v3 candidate. Never changes the active lock. |
| `bitrouter policy diff <ACTIVE> <CANDIDATE>` | Compare explicit route selections. |
| `bitrouter policy publish <CANDIDATE> [--config PATH] [--socket PATH]` | Publish that exact compiled v3 candidate under adaptive mode using its parent digest as a compare-and-swap token. |
| `bitrouter policy verify --evidence [--config PATH]` | Reconstruct the active compiled lock's evidence root from the local ledger/snapshot. |
| `bitrouter policy evolve [--apply \| --output FILE] [--config PATH]` | Compatibility compile/publish command. `--apply` requires `policy.mode: adaptive`; request-time routing remains lock-only. |
| `bitrouter policy reload [--config PATH] [--socket PATH]` | Hot-reload main config and policy lock through the existing daemon control socket. Invalid locks preserve the last-known-good runtime snapshot. |
| `bitrouter policy rollback <DIGEST> [--config PATH] [--socket PATH]` | Restore exact lock bytes from local promotion history, then reload or restore on rejection. |
| `bitrouter optimize run [--policy auto] [--candidate-tier TIER] [--exploration-ppm N] [--minimum-tasks N] [--maximum-tasks N] [--minimum-pass-rate-ppm N] [--evaluator-config-digest SHA256] [--config PATH] [--socket PATH]` | Perform and autonomously publish one deterministic history-driven controller step. Omit `--candidate-tier` to use the signed policy's `adequacy.explore_tier`; pass it only to override that tier. Champion-only history can start signed exploration; later steps promote, retreat, hold, or converge. The invocation itself grants publication authority; there is no separate review or approval command. |
| `bitrouter optimize status [--policy auto] [--config PATH]` | Observe the signed policy state without changing files or the database: `exploring` means an experiment is active and `idle` means none is active. Status does not infer convergence; repeat normal traced agent/Terminal Bench work, external Eval submission, and `run` until `optimize run` reports `converged`. Only complete task/episode cohorts gate quality and complete-unit cost; request subjects rank opportunities only. |
| `bitrouter key sign --user <id> [--db URL] [--policy ID]` | Mint a `brvk_…` virtual key in the auth DB. Plaintext is shown once; only its SHA-256 hash is stored. Default DB is `sqlite://./bitrouter.db`. |

Adaptive routing uses a source-independent predictor selected by
`key_strategy: agent_trace`. Its static policy keys are exclusively canonical
`agent_route/v1|<task-family>|<role>|<risk>` values. Native runtime adapters add
diagnostics only, not policy keys, and private BitRouter headers are not needed.
Observed `agent_trace/v2` values remain telemetry; retired route shapes and
`key_strategy: legacy_fingerprint` are rejected during configuration
validation. `adequacy.explore_opening: true` enables exploration for
source-neutral opening projections. The removed
`adequacy.max_downgraded_requests_per_session` setting is rejected because
session identity is diagnostic-only.

`policy.mode` belongs to the running process and defaults to `frozen`. Frozen
mode ignores evidence-ledger rows for live routing and forbids active lock
replacement while continuing to record evidence. `adaptive` permits validated
writeback but routes identically from the lock. Legacy
`writeback: locked|evolve` input parses as `frozen|adaptive`, but new config and
status output use only `mode`.

## Per-provider OAuth

| Command | Effect |
|---|---|
| `bitrouter providers login <provider>` | Per-provider OAuth. Supported providers include **`claude-code`**, **`github-copilot`**, and **`openai-codex`** — runs or adopts the provider's login flow and stores the refreshing token under `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. |
| `bitrouter providers login <provider> --api-key <KEY>` / `--key-stdin` | Seed a BYOK provider (any that accepts a pasted key — `openai`, `anthropic`, `google`, `openrouter`, `opencode-*`) non-interactively: skips the method menu and the stdin paste. `--key-stdin` reads one line from stdin instead. Both conflict with the OAuth-only `--import-existing` / `--no-browser`, and error if the provider has no API-key method. For `bitrouter`, the key seeds the cloud credential (same as `cloud login --api-key`). |
| `bitrouter providers logout <provider>` | Remove the stored OAuth token or credential for `<provider>`. |

## BitRouter Cloud sign-in (`bitrouter cloud …`)

OAuth 2.0 device-flow or non-interactive API-key sign-in against BitRouter Cloud. The persisted credential drives the raw API client, the `bitrouter` provider in the local daemon, telemetry attribution, and the management subcommands below.

| Command | Effect |
|---|---|
| `bitrouter cloud login [--oauth-as URL] [--client-id ID] [--scope SCOPE]` | RFC 8628 device-flow login. Prints an approval URL, polls the token endpoint, and persists access + refresh tokens to `$XDG_DATA_HOME/bitrouter/account-credentials.json` (mode 0600 on Unix). Auto-refreshes within 60 s of access-token expiry on every subsequent call. Defaults: AS `https://api.bitrouter.ai`, client id `bitrouter-cli`, scope set covering `inference:invoke usage:read keys:* billing:read policy:* byok:* namespace:read`. |
| `bitrouter cloud login --api-key <BRK_API_KEY> [--oauth-as URL]` | Non-interactive CI login. Validates `brk_<token_id>.<secret>` and stores it without a network request. Conflicts with OAuth-only `--client-id` and `--scope`; never prints the key. |
| `bitrouter cloud logout` | OAuth: best-effort RFC 7009 revoke, then delete the local file. API key: local deletion only. |
| `bitrouter cloud whoami` | Print auth type (`oauth` or `api_key`) and non-secret local metadata. Reads the on-disk file only — no network. |

## BitRouter Cloud raw API (`bitrouter cloud api`)

`bitrouter cloud api <relative-endpoint>` mirrors the core `gh api` workflow and reuses either stored credential. It accepts arbitrary relative paths but never follows redirects or sends credentials off the login origin.

| Flag | Effect |
|---|---|
| `-X, --method METHOD` | Explicit method; implicit `GET`, or `POST` when fields/input are supplied. |
| `-H, --header KEY:VALUE` | Repeatable request header. User `Authorization` overrides the stored bearer. |
| `-f, --raw-field KEY=VALUE` | String JSON/query field with nested `key[sub]` / `key[]` grammar; bare `key[]` creates an empty array. |
| `-F, --field KEY=VALUE` | Typed bool/null/integer field, or `@file` / `@-` string content. |
| `--input FILE|-` | Exact request body; fields move to the query string. |
| `-i, --include` | Status line + response headers before body. |
| `--silent` | Drain without printing the body. |
| `--verbose` | Redacted method/URL/header/status diagnostics on stderr. |

Non-TTY JSON, binary bodies, and SSE stream byte-for-byte to stdout. HTTP 4xx/5xx preserves the body on stdout, writes the error to stderr, and exits non-zero. Tested endpoints include models, providers, public usage stats, Chat Completions, Messages, Responses, `generateContent`, `streamGenerateContent`, settlement receipts, routing presets, OAuth clients, billing ledger/status routes, and namespace/account management routes.

Side effect: when the credentials file exists, the local daemon auto-adds the `bitrouter` provider to the zero-config providers map, so every model your account is entitled to is routable as `bitrouter:<model-id>` against `localhost:4356` without further configuration.

## BitRouter Cloud management (`bitrouter cloud …`)

Typed wrappers over the common Cloud management workflows. Requires either login form first. OAuth credentials use their baked namespace; API keys use `/v1/namespaces/me/*`. Every typed leaf accepts `--json` for raw response output; default is a `systemctl`-style key:value block (single resource) or a small table (lists). Use `bitrouter cloud api <relative-endpoint>` for Cloud APIs that do not yet have typed CLI wrappers. On a 403 with `missing required scope: <s>`, OAuth users receive a copy-pasteable `--scope` re-login hint; API-key users are directed to a key with that scope.

| Command | Effect |
|---|---|
| `bitrouter cloud whoami` | Cloud base URL + local subject/scope from the credentials file. Offline. |
| `bitrouter cloud namespace list/current` | Inspect available workspaces and the one this CLI session is baked to. Lifecycle operations require the Console or raw `cloud api` with the right control-plane scope. |
| `bitrouter cloud keys list / mint / revoke` | List `brk_…` API keys, mint a new one (plaintext shown once), revoke by id. Scopes: `keys:read` / `keys:write`. |
| `bitrouter cloud usage [--from RFC3339] [--to RFC3339]` | Aggregate spend (micro-USD) + token counts over a window (default last 30 days). Scope: `usage:read`. |
| `bitrouter cloud requests [--limit N] [--offset N]` | Paged request history. Scope: `usage:read`. |
| `bitrouter cloud billing balance` | Credit balance + pending debits + available (`max(balance - pending, 0)`). Scope: `billing:read`. |
| `bitrouter cloud billing checkout --amount-cents N` | Start a Stripe checkout session for a credit top-up. Returns a hosted URL. Scope: `billing:write` (opt-in via `--scope` at login). |
| `bitrouter cloud policy list/get/create/update/delete/bind/unbind/disable/enable/bindings/effective/for-principal` | Generic CRUD over policy registry. `create` and `update --spec` accept a JSON file path or `-` for stdin. `effective` and `for-principal` answer "what would happen for this principal" without making an actual inference call. Scope: `policy:read` / `policy:write`. |
| `bitrouter cloud budget list/get/create/update/delete` | Typed sugar over budget-kind policies. |
| `bitrouter cloud preset list/get/create/update/delete` | Typed sugar over preset-kind policies. |
| `bitrouter cloud byok list/set/delete` | BYOK provider keys. `set` takes already-sealed ciphertext (`--ciphertext-b64` + `--kek-id` matching the cloud's current X25519 public key). Scope: `byok:read` / `byok:write`. |

## Harness launch & spawn

Two verbs, split by role. `launch` runs a harness as an **interactive native
TUI** (the human is the orchestrator); `spawn` runs an **ACP-compatible harness
as a headless sub-agent** (a program is the orchestrator). Both route the
harness's LLM traffic through the daemon, drawing per-harness routing knowledge
from one shared catalog, so `launch -a claude` and `spawn claude-acp` inject
identical gateway env/args. `launch` additionally routes the harnesses env/args
*cannot* reach (opencode, pi) by synthesizing a throwaway config file —
headless `spawn` still runs those direct with a note.

| Command | Effect |
|---|---|
| `bitrouter launch --agent <id> [--model ID] [--config PATH] [--base-url URL] [--no-install] [--no-start] [--check] -- <agent args...>` | Launch a coding-agent CLI's native TUI through BitRouter without editing agent config files. `--agent` accepts `claude`, `codex`, `opencode`, and `pi` (catalog ids `claude-acp`, `codex-acp`, `pi-acp` also resolve); an unknown id fails up front with the available list. **`hermes`, `openclaw`, `grok`, and `agy` are no longer launch-supported** — run them directly or via `bitrouter spawn`; they remain usable as providers. Claude uses child env overrides (`ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`); Codex uses one-shot `-c` provider overrides with `wire_api="responses"`; **opencode and pi route via synthesized config** — an `OPENCODE_CONFIG` JSON / a `PI_CODING_AGENT_DIR` with `models.json` — written under the working tree's self-ignoring `.bitrouter/launch/`, model lists filled best-effort from the daemon's `/v1/models`. `--model ID` pins the model through whatever mechanism the harness has (model env var / `-c model=` / the synthesized config's default). **Gateway MCP injection**: the `bitrouter_tools` server (the daemon's aggregate `/mcp` route, omitted when `mcp.aggregate.enabled: false`) and the `bitrouter_skills` server (`mcp serve --backend skills`) are injected into the harnesses that have a mechanism for it — claude (`--mcp-config`), codex (`-c mcp_servers…`), and opencode (its synthesized config file); pi has no injectable MCP surface and launches without them. Only `claude` and `codex` have a bundled native installer; the others error with a pointer upstream. Prints a one-line session spend summary to stderr on exit. |
| `bitrouter spawn <agent> -p "<text>" [--no-wait] [--result-schema JSON\|@PATH] [session/routing flags]` | Spawn an ACP sub-agent, send one prompt, stream **NDJSON** to stdout, exit. `<agent>` is a catalog id (`claude-acp`, `codex-acp`, `gemini-cli`, `opencode`, `pi-acp`, `hermes-acp`, `openclaw`) or a configured `agents:` entry; a catalog id needs no config entry. `--result-schema` adds the machine-consumable result contract (see **Result contract** above). |
| `bitrouter spawn <agent> --serve [session/routing flags]` | Serve the sub-agent as a vanilla ACP Agent over stdio (for a GUI/manager). Same as `acp serve`, with routing attempted by default when the headless adapter supports it. |
| `bitrouter spawn <agent> --check [routing flags]` | Preflight harness resolution, the routing decision, and daemon reachability without launching anything. |

**Routing (attempted by default)** for `spawn` and the `acp serve\|prompt` aliases:
- `--direct` — do **not** route through the daemon; the harness uses its own provider auth.
- `--model <id>` — pin the harness's model (its model env var, or `-c model=` for codex).
- `--base-url <URL>` — override the gateway URL (else derived from `server.listen`).
- `--no-start` — never auto-start a local daemon; fail fast if it's down.
- Session flags (`--turn-timeout`) match `acp`.
- Auth: routed sub-agents authenticate with `BITROUTER_API_KEY` when set, else a local placeholder (fine under `skip_auth: true`); under `skip_auth: false` a key is required or `spawn` fails fast with `auth_required`.
- Fail-fast: if the daemon is unreachable (after auto-start) or auth is required and absent, `spawn` emits a single structured error **before** any session side effect — NDJSON `{"type":"error","code":"daemon_unreachable"|"auth_required",…}` in `-p` mode, stderr in `--serve` mode — and exits non-zero. Catalog harnesses whose routing is config-synthesis only (`opencode`, `pi-acp` — routed in the `bitrouter launch` interactive facet, not headless spawn yet; `hermes-acp` and `openclaw` are spawn-only now) and non-catalog agents warn and run direct; `hermes-acp` routes headless too if you export a synthesized `HERMES_HOME` + `CUSTOM_API_KEY`, and `openclaw` follows `OPENCLAW_STATE_DIR`/`OPENCLAW_CONFIG_PATH` to a profile whose gateway it auto-starts.
- `bitrouter spawn --agent <claude\|codex> …` is a **deprecated alias** for `bitrouter launch` (prints a migration note).

**`spawn -p` first line** is a `session` correlation line: `{"type":"session","record_id":"…","agent":"…","via":"http://127.0.0.1:4356"}` (`via` is `null` when direct), so an orchestrator can join the session to the daemon's cost/metering. Then the normal NDJSON update stream follows.


## Not in the proxy binary

- `bitrouter wallet` does **not** exist — it exits with `unrecognized subcommand`. OWS wallet integration lives in the separate `ows` workspace and is unlikely to land here.

## Config resolution

Every command that takes `--config` resolves the path in this order when the flag is omitted:

1. `./bitrouter.yaml` (current working directory)
2. `$BITROUTER_HOME/bitrouter.yaml`
3. `~/.bitrouter/bitrouter.yaml`
4. Zero-config in-memory defaults (no file)

The daemon `chdir`s to the directory holding the resolved config on startup, so every relative path inside the config (`database.url: sqlite://./bitrouter.db`, policy/agent file references) resolves against that directory, not the launcher's CWD.

## Signals

| Signal | Behavior |
|---|---|
| SIGHUP | Hot-reload daemon-side config + routing table. It does not forward provider keys from the invoking shell; use `bitrouter reload` for env-var rotation. |
| SIGINT / SIGTERM | Graceful shutdown: flush OTel exporter, remove pid file, exit 0. |
| SIGKILL | No cleanup — pid file will be stale and `bitrouter status` will report it. `bitrouter start` cleans up stale pid files automatically before launching. |
