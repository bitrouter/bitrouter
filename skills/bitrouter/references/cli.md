# CLI reference

Every subcommand the v1 binary actually exposes. Anything not listed here doesn't exist — don't suggest `bitrouter doctor`, `bitrouter providers add`, `bitrouter cloud connect`, or the old auth subcommand tree (cloud identity is `bitrouter cloud whoami`, see below).

## Daemon lifecycle

| Command | Effect |
|---|---|
| `bitrouter serve [--config PATH]` | Run the HTTP server + control socket **in the foreground**. Ctrl-C to stop. |
| `bitrouter start [--config PATH] [--log PATH]` | Spawn `serve` as a detached background process. Stdout/stderr go to `~/.bitrouter/bitrouter.log` unless `--log` overrides. Refuses to start over a live daemon. |
| `bitrouter stop [--config PATH] [--socket PATH]` | Graceful shutdown via the control socket. |
| `bitrouter restart [--config PATH] [--log PATH] [--socket PATH]` | Stop, wait up to 30s for in-flight requests to drain, then start. Escalates to SIGKILL on timeout. |
| `bitrouter reload [--config PATH] [--socket PATH]` | Hot-reload the running daemon's config + routing table. **Also re-pushes provider env vars** from the current shell into the daemon, so `export OPENAI_API_KEY=new...; bitrouter reload` rotates the key without a restart. SIGHUP reloads daemon-side config but cannot forward newly exported shell variables. |
| `bitrouter status [--config PATH] [--socket PATH]` | `systemctl status`-style block: pid / listen / model count / socket. Reports `stopped` (exit 0) when no daemon is reachable. |

## Inspection

| Command | Effect |
|---|---|
| `bitrouter route <model> [--config PATH]` | Resolve a model name through the routing table. Tries the running daemon first (live table), falls back to standalone config resolution. Prints the provider/service chain. |
| `bitrouter models [--config PATH] [--provider ID]` | List every routable model the config exposes. Filter by provider. |
| `bitrouter providers list [--config PATH]` | Tab-aligned: `ID  MODELS  ACTIVE  API_BASE`. |
| `bitrouter tools list [--config PATH]` | Enumerate tools advertised by every configured MCP server (one `tools/list` round-trip per server). |
| `bitrouter tools status [--config PATH]` | Health-check each MCP server. Latency or error per row. |
| `bitrouter tools discover <server> [--config PATH]` | Print a YAML stub for the discovered server, paste into `mcp_servers:`. |
| `bitrouter agents list [--remote] [--config PATH]` | Show bundled ACP catalog + which are configured. `--remote` also fetches the official ACP agent registry (cdn.agentclientprotocol.com) and lists its agents with version + install support (`npx`/`uvx` stub-able; `manual` for binary-only). |
| `bitrouter agents check [--config PATH]` | Spawn each configured ACP agent and verify `initialize` round-trip. |
| `bitrouter agents install <id>` | Print a paste-ready YAML stub for `<id>` — resolved from the bundled catalog first, then the ACP registry (`npx`/`uvx` distributions, version-pinned, `env` included). Binary-only registry entries are refused with a manual-install pointer (the registry has no checksums). |
| `bitrouter observe status [--json] [--config PATH] [--socket PATH]` | OTel exporter snapshot: wired / endpoint / sampler / cardinality usage / in-flight spans. JSON output for tooling. |

## ACP sessions

Per-session ACP substrate — one process = one session = one agent. Managers (GUI, AI agents, editors) spawn one process per session and drive it; orchestration is external to the substrate. `bitrouter spawn <agent> --serve|-p` is the newer umbrella over these (same code path); `acp serve|prompt` remain as stable aliases. Both **route the sub-agent's LLM traffic through the daemon by default** — add `--direct` / `--base-url` / `--model` / `--no-start` (see "Harness launch & spawn").

| Command | Effect |
|---|---|
| `bitrouter acp serve --agent <id> [--worktree <name>] [--rm-worktree] [--no-transcript] [--turn-timeout SECS] [--warm] [--idle-timeout SECS] [--direct] [--base-url URL] [--model ID] [--no-start] [--config PATH]` | Run one session as a vanilla ACP Agent over **stdio** until the manager disconnects. Managers spawn this per session and drive standard ACP (`initialize` → `session/new` → `session/prompt` / `session/cancel` / `session/load`). `--warm` keeps the session alive after disconnect and accepts reattach on a per-session unix socket until `--idle-timeout` (default 1800s) elapses with no manager. Logs go to stderr; stdout carries ACP JSON-RPC. Routes the agent's model calls through the daemon by default (`--direct` opts out). |
| `bitrouter acp prompt --agent <id> [--worktree <name>] [--rm-worktree] [--no-transcript] [--turn-timeout SECS] [--no-wait] [--direct] [--base-url URL] [--model ID] [--no-start] [--config PATH] <text>` | Launch a session, send one prompt, stream session updates to **stdout as NDJSON** (one JSON object per line), then exit. First line is the `session` correlation line (see below). Logs go to stderr. `--no-wait` submits and returns `{"type":"submitted"}` without streaming. Routes through the daemon by default (`--direct` opts out). |
| `bitrouter acp attach <record>` | Reattach a terminal to a warm session by record id (or unique prefix): bridges stdio to the session's unix socket (same NDJSON JSON-RPC framing). Run `initialize` → `session/load` to replay the conversation, then continue live. Unix-only. |
| `bitrouter acp sessions` | List the current repo's session records (`.bitrouter/sessions/*.json`), newest first: short record id, agent, status (`running` / `exited` / `dead` when the recorded pid no longer exists), age, worktree. |

**Worktrees**: `--worktree <name>` provisions `.bitrouter/worktrees/<name>` (created with a same-named branch, or reused/attached when the worktree or branch already exists). Worktrees are **retained** on exit — they hold the agent's work — and the retained path is logged to stderr. `--rm-worktree` opts in to removal at shutdown (destroys uncommitted work; only a worktree the session itself created is removed).

**Transcript**: every session appends a durable NDJSON transcript to `.bitrouter/sessions/<record_id>.transcript.ndjson` — prompts, every raw `session/update` (non-lossy), and per-turn results, each line stamped `{seq, ts}` (monotonic `seq`, unix-ms `ts`). Disable with `--no-transcript`.

**session/new relay**: the manager's `session/new` opens the upstream session, relaying its `cwd` (the launch-time `--worktree` wins) and `mcpServers` **verbatim** — the v2-aligned way for a manager to provide fs/terminal-style tooling (ACP v2 removes the client `fs/*`/`terminal/*` surface; client-side MCP servers replace it). Repeated `session/new` answers with the same stable record id.

**session/load**: replays the durable transcript as `session/update` notifications (user prompts as `user_message_chunk`, upstream updates verbatim) before the response — v1 `session/load` with the semantics ACP v2 standardizes as `session/resume {replayFrom: start}`. `loadSession` is advertised exactly when a transcript exists.

**Warm sessions**: with `--warm`, the record advertises a reattach socket (bound under `$BITROUTER_HOME/sock` — short paths; unix `sun_path` caps ~104 bytes) and `acp sessions` shows the session `running` after the manager leaves. Reattach with `bitrouter acp attach <record>`; the socket speaks the stdio framing (ACP's standardized remote transport replaces it when it ships).

**Observability**: when `plugins.bitrouter-observe` opts telemetry in (same config as the daemon), `acp serve|prompt` emit OTel GenAI agent spans — `invoke_agent <agent>` per turn and `execute_tool <tool>` per completed tool call, correlated by `gen_ai.conversation.id` = record id (the join key to the HTTP router plane when the agent's model calls go through bitrouter).

**Turns**: `session/cancel` is session-scoped — it cancels the active turn upstream *and* flushes the queued backlog (queued prompts resolve `stop_reason: "cancelled"`). `--turn-timeout SECS` sets a per-turn deadline: on elapse the agent is asked to cancel cooperatively (3s grace) before the turn errors.

**NDJSON format** (for `acp prompt` / `spawn -p`): the **first** line is a `session` correlation line — `{"type":"session","record_id":"…","agent":"…","via":"http://127.0.0.1:4356"}` (`via` is `null` when `--direct`) — for joining the session record to daemon cost/metering. Each update line is then a self-describing JSON object with a `type` field (snake_case): `message_chunk`, `thought_chunk`, `tool_call`, `tool_call_update`, `usage` (context-window occupancy: `used`, `size`, optional `cost`). The terminal line is `{"type":"result","stop_reason":"end_turn"}` (ACP wire spelling). In `--no-wait` mode only `{"type":"submitted"}` follows the session line. A fail-fast routing failure emits a single `{"type":"error","code":"daemon_unreachable"|"auth_required","via":…,"hint":…}` line instead, before any session is created.

**Result contract** (`spawn -p --result-schema '<JSON Schema>'`, or `@path` to read it from a file; conflicts with `--no-wait`): the schema rides the subagent's prompt as an instruction to end the reply with a ```json fenced block. The reply's **last** ```json block (or a bare-JSON reply) is extracted and validated; on a missing/invalid result the subagent gets **one** repair re-prompt. The terminal line then carries the machine-consumable outcome — success: `{"type":"result","stop_reason":…,"result":{…},"schema_ok":true}`; failure after repair: `…,"result":null,"schema_ok":false,"raw":"<last reply text>"` (the orchestrator is never blocked). Bare `spawn -p` output is unchanged (no `result`/`schema_ok`/`raw` keys). A malformed schema fails fast before any session side effect.

See `references/sessions.md` for the full per-session model (identity, turn queue, v1 limitations).

## TUI

| Command | Effect |
|---|---|
| `bitrouter tui --agent <id> [--worktree <name>]` | Launch the **composite multi-agent TUI** (TUI_SPEC v2): a fixed **left rail** (roster of every agent sorted by who needs you — needs-you `⚠` > review `◆` > attention `●` > working `⣷` > idle `○` > dead `✗` — plus a one-glyph-per-agent radar strip) beside the **primary pane**. `--agent claude\|codex` hosts that harness's **real native TUI in a PTY pane** (the *orchestrator*): keys pass through untouched (locked-mode; `Ctrl-A` is the single manager leader), **`Ctrl-C` interrupts the focused agent — it does NOT quit** (quit via `Ctrl-A x` on the last agent or `:quit`; in manager modes `Ctrl-C` still quits), OSC-52 clipboard writes forward to the outer terminal, and pane resizes SIGWINCH the child. The orchestrator gets the **fleet MCP bridge injected** (`--mcp-config` for claude, `-c mcp_servers…` for codex) so it can `spawn_subagent`/`subagent_diff`/… against this repo (see `references/orchestration.md`); its LLM traffic routes through the daemon like `bitrouter launch`. A configured `agents:` id instead renders that ACP agent from typed events (streamed text commits per line; fenced code blocks syntax-highlighted; tool diffs render `path +N/-M` with tinted `+`/`-` rows and `⋮` hunk gaps); ACP pane headers show `used/size · $cost`. Gated by the default-on `tui` feature. In-process (no daemon owns the sessions) — run `bitrouter serve` alongside; the TUI probes the listen address at startup and shows a warning if nothing is listening. **Isolation**: every agent spawned from the picker gets its **own worktree + branch by default** (`.bitrouter/worktrees/<agent>-<record16>`, branch `bitrouter/<agent>-<record16>`, based on the manager's HEAD) plus a `PORT` from the `worktrees.ports` pool (default 3100–3199, shown as `:PORT`). Worktrees are **retained** on close — cleanup is gated on merged-or-discarded. A configured `worktrees.bootstrap` hook runs in each new worktree; it executes shell, so a CONFIRM overlay shows the command on first spawn each session (`y` run / `n` skip / `Esc` cancel). |
| TUI modes | **NORMAL** (default): keys go to the focused detail pane; `PgUp`/`PgDn` scroll its scrollback (pinned view shows a `⇣N` newer-lines marker; paging back to the tail resumes following; works while a permission is pending); `Ctrl-A` → AGENT, `Ctrl-B` → BROADCAST (ACP panes; a focused PTY pane passes both `Ctrl-B` and `Ctrl-C` through to the child), `Ctrl-C` → interrupt the focused agent (cancel the ACP turn / raw `0x03` to a PTY child). **AGENT** (`Ctrl-A`): `j`/`k`/`↑`/`↓` move the rail cursor · `Enter` open the cursor agent solo (returns to NORMAL) · `s`/`v` split the detail horizontally/vertically (max 4): adds the cursor agent, or — when the cursor agent is already shown, e.g. right after `Ctrl-A` — the most actionable agent not yet shown; a mode-bar notice explains full/no-candidate no-ops · `u` drop the focused slot · `Tab`/`←`/`→`/`1`-`4` switch detail-slot focus · `q` toggle queue focus (rail shows only agents that need you; empty = `✓ all clear`) · `y`/`a`/`d` resolve the cursor agent's pending permission from the rail (allow / allow-always / **d**eny — same pending the pane shows inline, so either surface clears both; rows with a pending expand to `└ <what it wants>`) · `A` cycle the cursor agent's autonomy tier — **manual** (default: everything surfaces) → **assisted** (low-risk auto-allows; high-risk surfaces) → **auto** (everything auto-allows) — shown on the row as `[a]`/`[A]`; every auto-allow is logged into the pane as an `· auto-allowed (…)` line, never silent; risk is classified from the tool call's kind + locations (reads/searches and writes inside the project tree = low; deletes, command execution, network, writes outside the tree, unknown = high) and expanded queue rows are tagged `high ·`/`low ·` with high-risk items ordered first · **review queue**: an agent whose turn ends cleanly with a non-empty worktree diff (and passing `worktrees.checks`; failing checks loop back to the agent up to 2×, then surface) sorts to the rail head as `◆ review` with a `+N/-M` stat row — `D` load the full diff into the pane · `m` merge its branch (requires committed work; serialized) · `p` apply the diff uncommitted · `r` reject (your next typed prompt is the feedback) · `n` new agent (picker) · `x` close the cursor agent · `Esc`. **COMMAND** (`:` on an empty prompt line, or `:` in AGENT mode): fuzzy palette over `spawn agent` / `close agent` / `split horizontal|vertical` / `unsplit` / `broadcast` / `queue` / `autonomy cycle` / `kill done` (closes every exited agent) / `keys help` / `quit`; type to filter (subsequence match) · `↑`/`↓` select · `Enter` run · `Esc`. `?` in AGENT mode (or the `keys help` command) shows a which-key popup listing the current mode's bindings; any key dismisses it. **PICKER**: `↑`/`↓` select · `Enter` spawn · `Esc`. **BROADCAST** (`Ctrl-B`): `Space` toggle the rail-cursor row · `1`-`9` toggle the Nth roster row · `a` select all · type · `Enter` send to selection · `Esc`. A background agent (not visible in the detail) needing attention rings the bell and shows `●` in the rail/radar. Cost HUD / detach are future work. |

## Setup helpers

| Command | Effect |
|---|---|
| `bitrouter init [--config PATH]` | Write a starter `bitrouter.yaml` (default `./bitrouter.yaml`). Refuses to overwrite. Mirrors the zero-config defaults — `skip_auth: true`, `listen: 127.0.0.1:4356`, and common registry providers stubbed as `{}` so they can inherit registry defaults and auto-enable when their credential is available. |
| `bitrouter config validate [--config PATH]` | Validate a config file by running the real parse path: structure (deserialization), `derives` resolution, the upstream-URL (SSRF) gate, and any referenced `policy-lock.yaml`. Exits non-zero on an invalid config — **CI-safe**. Does *not* load the JSON Schema (that artifact, at `dist/schema/bitrouter.config.schema.json` / regenerated with `cargo run -p dist-helper -- generate-schema`, is for IDE autocomplete + the drift check). Unset `${VAR}` references are substituted with a `.invalid` placeholder and reported as warnings, so secrets need not be present; a value that embeds one mid-string is not authoritatively checked. |
| `bitrouter policy create <id> [--dir DIR]` | Write a starter access-control policy file under `--dir` (default `./policies`). Bind to a key with `bitrouter key sign --user <id> --policy <id>`. |
| `bitrouter policy init <name> --preset <preset> --economy <model> [--strong <model>] [--config PATH]` | Create or extend the deterministic `policy-lock.yaml`, bind the named policy to a preset, and leave programmatic writeback locked. The strong model is inferred from an existing preset when omitted. |
| `bitrouter policy check|status [--config PATH]` | Cross-validate the main config and lock, or report the resolved path, semantic digest, writeback mode, policies, and preset bindings. |
| `bitrouter policy show <name> [--config PATH]` | Print one validated effective policy. |
| `bitrouter policy evolve [--apply] [--config PATH]` | Project policy-namespaced adequacy evidence into a deterministic candidate. Dry-run by default; `--apply` requires writeback to be unlocked. Existing routes are never overwritten or removed. |
| `bitrouter policy lock|unlock [--config PATH]` | Forbid or permit programmatic replacement of `policy-lock.yaml`. Manual/Git edits and reload remain allowed while locked. |
| `bitrouter policy reload [--config PATH] [--socket PATH]` | Hot-reload main config and policy lock through the existing daemon control socket. Invalid locks preserve the last-known-good runtime snapshot. |
| `bitrouter key sign --user <id> [--db URL] [--policy ID]` | Mint a `brvk_…` virtual key in the auth DB. Plaintext is shown once; only its SHA-256 hash is stored. Default DB is `sqlite://./bitrouter.db`. |

## Per-provider OAuth

| Command | Effect |
|---|---|
| `bitrouter providers login <provider>` | Per-provider OAuth. Supported providers include **`claude-code`**, **`github-copilot`**, and **`openai-codex`** — runs or adopts the provider's login flow and stores the refreshing token under `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. |
| `bitrouter providers logout <provider>` | Remove the stored OAuth token or credential for `<provider>`. |

## BitRouter Cloud sign-in (`bitrouter cloud …`)

OAuth 2.0 device-flow sign-in against the BitRouter Cloud authorization server. The persisted credential drives both the `bitrouter` provider in the local daemon and the management subcommands below.

| Command | Effect |
|---|---|
| `bitrouter cloud login [--oauth-as URL] [--client-id ID] [--scope SCOPE]` | RFC 8628 device-flow login. Prints an approval URL, polls the token endpoint, and persists access + refresh tokens to `$XDG_DATA_HOME/bitrouter/account-credentials.json` (mode 0600 on Unix). Auto-refreshes within 60 s of access-token expiry on every subsequent call. Defaults: AS `https://api.bitrouter.ai`, client id `bitrouter-cli`, scope set covering `inference:invoke usage:read keys:* billing:read policy:* byok:* namespace:read`. Override the AS or scope for a self-hosted deployment or to opt into sensitive control-plane scopes such as `billing:write`, `user:write`, and `namespace:write`. |
| `bitrouter cloud logout` | Best-effort RFC 7009 revoke at the AS, then delete the local credentials file. |
| `bitrouter cloud whoami` | Print the local credential's AS, client id, scope, subject, expiry, namespace, and cloud base URL. Reads the on-disk file only — no network. |

Side effect: when the credentials file exists, the local daemon auto-adds the `bitrouter` provider to the zero-config providers map, so every model your account is entitled to is routable as `bitrouter:<model-id>` against `localhost:4356` without further configuration.

## BitRouter Cloud management (`bitrouter cloud …`)

Typed wrappers over the `/v1/*` management API on the cloud. Requires `bitrouter cloud login` first. Every leaf accepts `--json` for raw response output; default is a `systemctl`-style key:value block (single resource) or a small table (lists). On a 403 with `missing required scope: <s>`, the CLI prints a copy-pasteable `bitrouter cloud login --scope "<current> <s>"` hint.

| Command | Effect |
|---|---|
| `bitrouter cloud whoami` | Cloud base URL + local subject/scope from the credentials file. Offline. |
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
from one shared catalog, so `launch claude` and `spawn claude-acp` inject
identical gateway env/args.

| Command | Effect |
|---|---|
| `bitrouter launch --agent <claude\|codex> [--config PATH] [--base-url URL] [--no-install] [--no-start] [--check] -- <agent args...>` | Launch a coding-agent CLI's native TUI through BitRouter without editing agent config files. Claude uses child env overrides (`ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`); Codex uses one-shot `-c` provider overrides with `wire_api="responses"`. Prints a one-line session spend summary to stderr on exit. |
| `bitrouter spawn <agent> -p "<text>" [--no-wait] [--result-schema JSON\|@PATH] [session/routing flags]` | Spawn an ACP sub-agent, send one prompt, stream **NDJSON** to stdout, exit. `<agent>` is a catalog id (`claude-acp`, `codex-acp`, `gemini-cli`, `pi-acp`) or a configured `agents:` entry; a catalog id needs no config entry. `--result-schema` adds the machine-consumable result contract (see **Result contract** above). |
| `bitrouter spawn <agent> --serve [--warm] [--idle-timeout SECS] [session/routing flags]` | Serve the sub-agent as a vanilla ACP Agent over stdio (for a GUI/manager). Same as `acp serve` with routing on. |
| `bitrouter spawn <agent> --check [routing flags]` | Preflight harness resolution, the routing decision, and daemon reachability without launching anything. |

**Routing (default on)** for `spawn` and the `acp serve\|prompt` aliases:
- `--direct` — do **not** route through the daemon; the harness uses its own provider auth.
- `--model <id>` — pin the harness's model (its model env var, or `-c model=` for codex).
- `--base-url <URL>` — override the gateway URL (else derived from `server.listen`).
- `--no-start` — never auto-start a local daemon; fail fast if it's down.
- Session flags (`--worktree`/`--rm-worktree`/`--no-transcript`/`--turn-timeout`) match `acp`.
- Auth: routed sub-agents authenticate with `BITROUTER_API_KEY` when set, else a local placeholder (fine under `skip_auth: true`); under `skip_auth: false` a key is required or `spawn` fails fast with `auth_required`.
- Fail-fast: if the daemon is unreachable (after auto-start) or auth is required and absent, `spawn` emits a single structured error **before** any session side effect — NDJSON `{"type":"error","code":"daemon_unreachable"|"auth_required",…}` in `-p` mode, stderr in `--serve` mode — and exits non-zero. Catalog harnesses that can't be routed (e.g. `pi-acp`) and non-catalog agents warn and run direct.
- `bitrouter spawn --agent <claude\|codex> …` is a **deprecated alias** for `bitrouter launch` (prints a migration note).

**`spawn -p` first line** is a `session` correlation line: `{"type":"session","record_id":"…","agent":"…","via":"http://127.0.0.1:4356"}` (`via` is `null` when direct), so an orchestrator can join the session's record to the daemon's cost/metering. Then the normal NDJSON update stream follows.


## Unimplemented in v1.0

These print `not implemented in v1.0` today and are unlikely to land in the proxy binary:

- `bitrouter wallet` — OWS wallet integration lives in the separate `ows` workspace, not in the proxy binary.

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
