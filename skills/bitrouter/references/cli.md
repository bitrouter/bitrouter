# CLI reference

Every subcommand the v1 binary actually exposes. Anything not listed here doesn't exist — don't suggest `bitrouter doctor`, `bitrouter providers add`, `bitrouter cloud connect`, or the old auth subcommand tree (cloud identity is `bitrouter cloud whoami`, see below).

Bare `bitrouter` opens first-run onboarding when no default ACP harness is saved. Credentials alone do not complete setup. The wizard saves `chat.agent` and optional `chat.model`, then either opens BitRouter's ACP TUI, starts the daemon, or exits. Subsequent bare invocations immediately open the saved TUI.  Configuration resolves from `./bitrouter.yaml`, then `$BITROUTER_HOME/bitrouter.yaml`, then `~/.bitrouter/bitrouter.yaml`. With no existing file, onboarding writes to the BitRouter home. `init -c PATH` selects an explicit destination. Existing configuration values are preserved while updating chat defaults; `--force` replaces them with the starter configuration. Writes are atomic. First-run defaults bind `127.0.0.1:4356` with `skip_auth: true`.  `init --yes` saves configuration without interactive credential prompts and exits by default. The default harness is `codex-acp`; `--harness claude` selects `claude-acp`. Repeated `--harness` flags use the first as the default. An explicit `--after launch` opens the ACP TUI even when setup itself was headless. Without a terminal, bare unconfigured invocation prints setup instructions and an inert onboarding envelope; it does not silently complete the wizard.

Interactive onboarding offers every active public registry provider in one
alphabetical, searchable list; BitRouter Cloud (`bitrouter`) is a normal row.
The binary includes a registry snapshot for uncached/offline setup. Custom or
disabled registries remain authoritative. Configured providers are marked; add
providers, then select **Continue to harness setup**. All setup and login choices
use Up/Down + Enter, with eight visible rows, typing to search, Backspace/Ctrl-U
to edit/clear, and Home/End or Page Up/Down to scroll. Digits filter rather than
select. Esc/Ctrl-C cancels without completing setup. The harness list comes from
the ACP registry; the headless `--harness` aliases remain `codex` and `claude`.



Global `--context NAME` selects a named remote-control target for the read-only
`status` (including `--requests`), `models`, and `route` commands; `local`
forces normal local behavior.
Manage targets with `bitrouter context add NAME --endpoint URL --token-env
ENV`, `context list`, `context show NAME`, and `context remove NAME`. The store
contains the token environment-variable name, never its value. HTTPS is
required except for loopback/SSH-forwarded HTTP. The client handshakes first,
follows no redirects, never falls back to local state on a remote error, and
rejects every lifecycle/ACP/agent command under a remote context.

## Daemon lifecycle

| Command | Effect |
|---|---|
| `bitrouter serve [--config PATH]` | Run the inference HTTP server + local control socket **in the foreground**. Optional `control.enabled: true` also starts the authenticated HTTP control listener at `control.listen` (default `127.0.0.1:4358`), including Streamable HTTP MCP at `/mcp-control`. It requires a dedicated `BITROUTER_CONTROL_TOKEN` of at least 32 bytes, validates browser origins, stays loopback-only for a private tunnel/TLS reverse proxy, and ignores inference `server.skip_auth`. It does not expose ACP sessions. |
| `bitrouter start [--config PATH] [--log PATH]` | Spawn `serve` as a detached background process. Stdout/stderr go to `~/.bitrouter/bitrouter.log` unless `--log` overrides. Refuses to start over a live daemon. |
| `bitrouter stop [--config PATH] [--socket PATH]` | Graceful shutdown via the control socket. |
| `bitrouter restart [--config PATH] [--log PATH] [--socket PATH]` | Stop, wait up to 30s for in-flight requests to drain, then start. Escalates to SIGKILL on timeout. |
| `bitrouter reload [--config PATH] [--socket PATH]` | Hot-reload the running daemon's config + routing table. **Also re-pushes provider env vars** from the current shell into the daemon, so `export OPENAI_API_KEY=new...; bitrouter reload` rotates the key without a restart. SIGHUP reloads daemon-side config but cannot forward newly exported shell variables. |
| `bitrouter status [--config PATH] [--socket PATH]` | `systemctl status`-style block: pid / listen / model count / the distinct providers behind them / socket, plus `spend`. Reports `stopped` (exit 0) when no daemon is reachable — and still reports spend, which is on disk and outlives the daemon. Same report type as the origin MCP server's `status` tool, so `--json` here and that tool's structured content are the same bytes. **`spend` is two independent facts**: `spend.spent` (money already gone — `estimated_micro_usd` over `window`, `requests`, `unpriced`) comes from the local metering database on *any* deployment; `spend.limit` (money left — `balance_micro_usd`, `pending_micro_usd`, `remaining_micro_usd`) appears only where a cap exists, today a metered cloud account's prepaid credit. `spent` is an **estimate and a floor**: requests with no charge evidence are excluded rather than summed as zero, and `unpriced` counts them — non-zero means the figure understates by an unknown amount, so never read it as a total. A `limit` is the opposite, an authoritative ledger. No `spend` key at all means no metering database was readable, which is *not* the same as `estimated_micro_usd: 0`. Machine-wide, not per-caller. |
| `bitrouter requests [--limit N]` | Newest-first settled requests (time, model, provider actually used, tokens, cost, latency, status) + the window's spend and trailing-minute rate. **JSON by default**; `--human` renders the table. Reads the metering store directly and works with no daemon (`mode: history_only`). Portable to named remote contexts. `status --requests` is a hidden compatibility spelling. |

## Inspection

| Command | Effect |
|---|---|
| `bitrouter route <model> [--prompt TEXT] [--config PATH]` | Resolve a model name through the routing table. Tries the running daemon first (live table; its `route` verb resolves the model as given — the daemon's policy table runs on real requests, not on this preview), falls back to standalone config resolution — **policy table included**, so the answer there is what would actually run. `--prompt` supplies the request text the policy table keys on (it routes by agent-loop step, so the selected model can differ with the prompt); config path only. JSON keys: `requested_model` (what you asked for) and `effective_model` (what would run — **read this one**; equals `requested_model` on `live`), `effective_effort`, `resolved_via` (`live` \| `config` \| `zero_config` — the same words `bitrouter models` uses), `policy_decision` (absent on `live`: the daemon's `route` verb does not replay policy), `provider_chain[]` of `{provider, service_id, api_protocol}`, and `estimated_cost` (the first hop's per-token rates plus any steeper long-context brackets — rates, not a total). Same report type as the MCP `route_preview` tool. Read-only: nothing is sent upstream, and no credential is surfaced. |
| `bitrouter models [--config PATH] [--provider ID]` | List every routable model selector, each with **all** the providers that can serve it (the fallback chain, in order). Subscription providers are explicit-route-only and therefore appear as pinned `provider:canonical-model` selectors; every displayed selector can be passed unchanged to `bitrouter route`. Tries the running daemon first, falls back to a standalone config parse — same order as `bitrouter route`. The parse is resolved the way the daemon resolves its own config at start-up (built-in defaults, then subscription providers such as `claude-code` / `google-ai` re-activated from the OAuth credential store), so a subscription-backed provider is listed with no daemon running; the live table additionally reflects `reload`s and whatever the daemon resolved at start-up. `--json` reports `resolved_via: "live" \| "config"`. Filter with `--provider`. Same report type as the MCP `list_models` tool. |
| `bitrouter providers list [--config PATH]` | Tab-aligned: `ID  MODELS  ACTIVE  API_BASE`. |
| `bitrouter mcp serve` | Run BitRouter's local origin MCP server over protocol-pure stdio for a host or plugin. Network-capable hosts connect directly to the daemon's authenticated `/mcp-control` Streamable HTTP endpoint. The old standalone HTTP/cloud flags are hidden compatibility inputs and no longer start a second listener. |
| `bitrouter mcp check [server] [--config PATH]` | Connect to one configured upstream MCP server, or all of them, and report transport, reachability, latency, negotiated tools capability, and advertised tool names. |
| `bitrouter agents list [--remote] [--config PATH]` | Show bundled ACP catalog + which are configured. `--remote` also fetches the official ACP agent registry (cdn.agentclientprotocol.com) and lists its agents with version + install support (`npx`/`uvx` stub-able; `manual` for binary-only). |
| `bitrouter agents inspect <agent> [--config PATH]` | Open a fresh ACP session, wait briefly for advertised slash commands, and report which source answers each command. |
| `bitrouter agents check [agent] [--config PATH]` | Preflight one friendly/catalog agent or spawn each configured ACP agent and verify `initialize`. |
| `bitrouter agents conformance <id>` | Run the `acp_compat_1` ACP-compatibility suite against a catalog agent and print the `conformance:` block to record in `registry/runtimes/<runtime>.yaml`. `<id>` is `<runtime>/<harness>` or a bare harness id (`local/` is the default runtime and may be elided). Two tiers: **handshake** (the agent answers `initialize` and settles on the ACP version its registry entry declares) and **routability** (the agent's LLM traffic reaches BitRouter when its routing block is applied). Needs **no provider credentials** — the agent is launched with its own routing pointed at an ephemeral loopback gateway that records what arrived; it does spawn the agent, so the package or binary must be installed. Exits non-zero when a tier fails, and prints no record in that case. |
| `bitrouter agents scaffold <id>` | Print a paste-ready YAML stub for `<id>` — resolved from the bundled catalog first, then the ACP registry (`npx`/`uvx` distributions, version-pinned, `env` included). `agents install` is a hidden compatibility spelling. |
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

Two ACP execution modes share one controller and differ only in who drives it. `acp serve` exposes the connection-level controller over **stdio** for an ACP client. `run` drives one prompt **in-process** and presents it as NDJSON/text/quiet output. Session ownership is harness-native in both. Hidden `spawn` and `acp prompt` spellings remain for migration. Both **attempt to route the harness's model traffic through the daemon when the headless adapter supports redirection** — add `--direct` / `--base-url` / `--model` / `--no-start`.

| Command | Effect |
|---|---|
| `bitrouter run <agent> [prompt\|-] [--prompt-file PATH] [--load ID\|--resume ID] [--cwd PATH] [--turn-timeout SECS] [--approve-all\|--approve-reads\|--deny-all] [--permission-policy JSON\|@PATH] [--format ndjson\|text\|quiet] [routing flags]` | Canonical always-headless entry. Reads a positional prompt, `-`/implicit piped stdin, or a UTF-8 prompt file. Creates a new harness-native session unless `--load` (history replay) or `--resume` (no replay) is capability-advertised and selected. Streams versioned NDJSON by default. |
| `bitrouter acp serve <agent> [--turn-timeout SECS] [routing flags] [--config PATH]` | Expose an ACP-compatible adapter over protocol-pure **stdio** until this ACP client disconnects. The client initializes first, may open multiple harness-native sessions, and owns prompt deadlines. Session IDs, history, and storage remain harness-owned. |

**Controller lifecycle**: ACP client `initialize` capabilities and `_meta` reach the harness exactly. Each `session/new` is forwarded and returns that harness response's opaque `sessionId`; repeated calls may create different sessions. Advertised `session/list|load|resume|fork|close|delete`, prompts, cancellations, callbacks, updates, errors, `_meta`, and extension payloads pass through. BitRouter neither mints a client-facing session alias nor keeps a session catalog.

**Endpoint setup**: Claude uses pinned `@agentclientprotocol/claude-agent-acp@0.75.1`; Codex uses pinned `@agentclientprotocol/codex-acp@1.10.0`. Controller-to-harness `providers/*` configures the model endpoint and is removed from client-facing capabilities. Claude's fallback is `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / newline-separated `ANTHROPIC_CUSTOM_HEADERS`; Codex's is `CODEX_CONFIG` plus `MODEL_PROVIDER`, with no ACP-mode `-c` arguments.

**Session route control**: a locally bound controller advertises stable-v1
`_bitrouter/route/list|set|reset` methods under initialize response
`_meta["bitrouter.dev/controller"].routeControl`. They operate on opaque native
`sessionId` values and create daemon-confirmed ephemeral leases; client-side
`providers/*` remains unavailable. `--direct` and explicit remote `--base-url`
connections do not advertise route control because hosted HTTP route control
is not implemented yet. `route/list.available` is a logical-model suggestion list;
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

**Observability and turns**: `acp serve` forwards the harness's session/cancel and session/update wire unchanged, except that a locally bound controller decorates the harness's own `usage_update` with session-attributed `cost` (see **Session cost**); it never synthesizes per-session usage or timeout behavior. Authenticated routed model calls normalize BitRouter's static controller/harness headers and the harness's native Claude/Codex identity into controlled capture/replay, request spans, route decisions, and nullable metering correlation. The normal API/virtual key is the only authentication boundary; authorization, cookies, and credentials remain excluded. `run` drives the same controller in-process, so it gets identical forwarding; its OTel turn spans are re-derived from the prompt round-trip and correlate on the **harness-native** session id. `--turn-timeout` and cooperative cancellation are the client's there, and in `code <agent>`: every command now drives the same controller through the same client, and no local `record_id` or controller-owned FIFO queue exists. Code may keep an explicit process-local next-turn queue.

**NDJSON format**: every event carries `"version":1`, a monotonically increasing `"seq"`, and a `"type"`. The first success event is `session`; streamed events include `message_chunk`, `thought_chunk`, `tool_call`, `tool_call_update`, `usage`, and `permission`; exactly one terminal `result` or `error` follows. The old `--format json` value remains an alias for `ndjson` during migration.

**Headless permissions** (`run`, plus the hidden compatibility forms): nobody is at the terminal, so the caller states the rule. `--deny-all` (the default) answers every `session/request_permission` with the agent's reject option; `--approve-reads` approves calls the harness labels `read` or `search` (the ACP tool `kind`) and denies the rest, unlabelled calls included; `--approve-all` approves everything. `--permission-policy '{"autoApprove":["read","Grep"],"autoDeny":["execute"],"defaultAction":"deny"}'` (or `@path`) overrides per tool: entries match the tool kind, the tool-call title, or the title's first word, case-insensitively; `autoDeny` beats `autoApprove`, and an unmatched request falls to `defaultAction`, else the mode flag. An approval against a request that offered no allow option still resolves to the reject option and counts as a denial. Each answer is one NDJSON line, `{"type":"permission","decision":"approved"|"denied","title":"…","kind":"edit"|null}`. **Exit status 5** when at least one request was denied and none approved; 0 otherwise (a turn or launch failure is still 1). The retained piped `chat` compatibility path uses this same `Policy` with deny-all. Canonical `code` requires interactive stdin and stdout; use `run` for headless work.

**Result contract** (`run --result-schema '<JSON Schema>'`, or `@path` to read it from a file; conflicts with `--no-wait`): the schema rides the subagent's prompt as an instruction to end the reply with a ```json fenced block. The reply's **last** ```json block (or a bare-JSON reply) is extracted and validated; on a missing/invalid result the subagent gets **one** repair re-prompt. The terminal line then carries the machine-consumable outcome — success: `{"type":"result","stop_reason":…,"result":{…},"schema_ok":true}`; failure after repair: `…,"result":null,"schema_ok":false,"raw":"<last reply text>"` (the orchestrator is never blocked). Bare `run` output is unchanged (no `result`/`schema_ok`/`raw` keys). A malformed schema fails fast before any session side effect.

See `references/sessions.md` for the controller/native-session boundary and what `run` adds on top of it.

## Interactive interface (`bitrouter code`)

Bare local `bitrouter code` opens an empty conversation and **Choose agent**.
`bitrouter code <agent>` connects directly through the shared ACP session host.
There are no permanent tabs: Ctrl-P opens searchable commands and temporary
pickers/inspectors. Closing them preserves the draft and reading position.
Agent, confirmed session route, activity, and attributed session cost are the
persistent status fields. Missing cost remains unreported, never zero.

| Key | Effect |
| --- | --- |
| `Enter` | Send at idle; preserve draft and explain queueing during work |
| `Shift-Enter` / `Ctrl-J` | Newline |
| `Tab` | Complete the open popup, otherwise queue next during work |
| `Ctrl-P` / leading `/` | Command palette / slash completion, labelled by owner |
| Arrows, Home/End, Up/Down at draft boundaries | Cursor editing and process-local history |
| `Ctrl-G` | External editor at idle without pending permissions |
| `PageUp` / `PageDown` | Read history without following new output |
| `F2` | Focus the oldest pending permission; choose a row, then Enter confirms |
| `Esc` / `Ctrl-C` during work | Request cancellation and wait for settlement |
| `Ctrl-C` at idle | Clear draft; exit when empty |
| `Ctrl-D` at idle | Exit only with an empty draft |
| `Ctrl-L` | Redraw |

Paste retains exact line breaks and does not submit. Follow-up queueing is an
explicit client feature, not native steering; abnormal stops pause the queue.
Agent settings are ACP-reported and separate from `/route` session overrides.
Load replays native history; resume does not. No durable BitRouter session store
is created. Pending permissions use exact offered IDs, have no default approval,
and resolve as cancelled when the turn is cancelled.

`--context NAME code` and explicit `code --socket PATH` open read-only
operations inspectors, with no ACP execution or local fallback for remote
errors. Host requests remain clearly host-scoped. Hidden interactive `tui` and
`chat` aliases share the Code loop; piped compatibility output stays plain.

## Setup helpers

| Command | Effect |
|---|---|
| `bitrouter init [--yes] [--force] [--reset] [-c PATH] [credential flags] [--harness claude\|codex] [--after launch\|serve\|exit] [--model ID]` | Save the default ACP harness and model in the resolved configuration, or BitRouter home when absent. Credential flags: `--cloud-login`, `--api-key`, `--provider`, `--provider-api-key`, `--use-detected`. Headless setup reports-and-skips interactive logins. `--after launch` opens BitRouter ACP TUI; `--force` resets existing configuration. |
| `bitrouter config validate [--config PATH]` | Validate a config file by running the real parse path: structure (deserialization), `derives` resolution, the upstream-URL (SSRF) gate, and any referenced `policy-lock.yaml`. Exits non-zero on an invalid config — **CI-safe**. Does *not* load the JSON Schema (that artifact, at `dist/schema/bitrouter.config.schema.json` / regenerated with `cargo run -p dist-helper -- generate-schema`, is for IDE autocomplete + the drift check). Unset `${VAR}` references are substituted with a `.invalid` placeholder and reported as warnings, so secrets need not be present; a value that embeds one mid-string is not authoritatively checked. Also reports `ignored_config` — `plugins.<id>` blocks the binary does not read and therefore ignores, which is otherwise silent (`bitrouter-guardrails`, `bitrouter-policy` and `bitrouter-telemetry` are the ids it reads). That does **not** fail validation: it is a misconfiguration, not a malformed config. The daemon, `bitrouter acp serve`, `bitrouter run`, and `bitrouter code <agent>` log the same set on every start. |
| `bitrouter skills list [--global] [--json\|--human]` | List skills. Reads the project root by default; `--global` reads `~/.claude/`. Covers all three conventional layouts of that root (`<root>/SKILL.md`, `<root>/skills/<name>/`, `<root>/.claude/skills/<name>/`) — it used to read only the last. Each row carries `name`, `description`, `dir`, `skill_md`, `valid`, and a `problem` when `valid` is false (bad frontmatter, a directory name that does not match `frontmatter.name`, an out-of-bounds name/description). Invalid skills are listed *marked* here and in `skills_search`, and omitted from SEP-2640 `skills/list`, which requires a verifiable entry — so this is where you learn why a skill on disk will not load. Same report type as the `skills_search` tool. |
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
| `bitrouter workflow-state classifier-bakeoff --fixtures DIR [--submission FILE] --output FILE` | Build a deterministic, read-only Route Context V3 research report (artifact/report schema v2: corrected ECE, null unobserved selective risk, and `classification_surrogate_loss`). Omit `--submission` for the compiled scorecard baseline. Candidate files are strict full-coverage shadow evidence and never alter live routing. |
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

## Native launch and headless ACP

The maintained ACP adapters are pinned to
`@agentclientprotocol/codex-acp@1.10.0` and
`@agentclientprotocol/claude-agent-acp@0.75.1`. Node.js 22+ and `npx` are
required. At ACP session startup, BitRouter probes for compatible installed
Codex and Claude CLIs behind the adapters; explicit env and transport
overrides win, and custom adapter versions are not modified.

The native shortcuts and `launch` run a harness's **interactive native TUI**;
`run` drives an **ACP-compatible adapter as a headless sub-agent**. Both route the
harness's LLM traffic through the daemon, drawing per-harness routing knowledge
from one shared catalog, so native `launch claude` and ACP `run claude` resolve
the appropriate facets of the same harness entry.

| Command | Effect |
|---|---|
| `bitrouter launch <agent> [options] -- <agent args...>` | Launch a coding-agent CLI's native interface through BitRouter without editing its user config. `bitrouter claude`, `bitrouter claude-code`, and `bitrouter codex` are first-class shortcuts. The hidden `launch --agent` spelling remains compatible. Gateway MCP injection continues to use protocol-pure `mcp serve` subprocesses where the harness supports them. |
The former `spawn` command is hidden for compatibility: `spawn -p` maps to `run`, `spawn --serve` to `acp serve`, and `spawn --check` to `agents check`.

**Routing (attempted by default)** for `run` and `acp serve`:
- `--direct` — do **not** route through the daemon; the harness uses its own provider auth.
- `--model <id>` — pin the harness's model (its model env var, or `-c model=` for codex).
- `--base-url <URL>` — override the gateway URL (else derived from `server.listen`).
- `--no-start` — never auto-start a local daemon; fail fast if it's down.
- Session flags (`--turn-timeout`) match `acp`.
- Auth: routed sub-agents authenticate with `BITROUTER_API_KEY` when set, else a local placeholder (fine under `skip_auth: true`); under `skip_auth: false` a key is required or `run` fails fast with `auth_required`.
- Fail-fast: if the daemon is unreachable (after auto-start) or auth is required and absent, `run` emits a single structured error before any session side effect and exits non-zero; `acp serve` reports the diagnostic on stderr. Catalog adapters whose routing is config-synthesis only and non-catalog agents warn and run direct.
- `bitrouter spawn --agent <claude\|codex> …` is a **deprecated alias** for `bitrouter launch` (prints a migration note).

**`run` first line** is a versioned `session` correlation line carrying the native session id, resolved agent, route endpoint, and launch id; `launch_id` joins it to daemon cost/metering. Then the normal sequenced NDJSON update stream follows.


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
