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
| `bitrouter tui --agent <id> [--worktree <name>] [--model ID]` | Launch the **composite multi-agent TUI** (TUI_SPEC_V3 — a pure control tower): a **sessions sidebar** (left: every orchestrator PTY session, its harness binary over a dim model line; `Ctrl-Space n` (the leader) or the palette's `new session` spawns more — they inherit `--model`) and a **subagents rail** (right: roster of every ACP agent sorted by who needs you — needs-you `⚠` > review `◆` > attention `●` > done-unseen `◉` > working `⣷` > idle `○` > dead `✗` — each row over a dim `state · harness` meta line, plus a one-glyph-per-agent radar strip) around the **primary pane**; the **sidebars run the full terminal height** and are **collapsible** (palette `toggle sessions`/`toggle subagents`); when both don't fit beside a usable center (~48 cols) they fold one at a time — the panel with content wins, then the rail by default; with room, both show even when empty (an empty panel is an affordance). `--agent claude\|codex\|opencode\|pi\|hermes\|openclaw\|grok\|agy` (catalog id `antigravity` also resolves to `agy`) hosts that harness's **real native TUI in a PTY pane** (the *orchestrator*): keys pass through untouched (locked-mode; the intercepted chords are the **one-shot leader** — `tui.leader` in `bitrouter.yaml`, `ctrl-<key>` form, default `Ctrl-Space` — so `Ctrl-A`/`Ctrl-B` reach the child as readline keys, plus **`PgUp`/`PgDn`**, which page the pane's **host-owned emulator scrollback** on the main screen (forwarded to the child on the alternate screen, which keeps no host history). **Mouse** is forwarded to any inner app that enabled mouse reporting — wheel, clicks, drags, and motion reach it as real SGR/X10 events at pane-relative coordinates, so e.g. `claude` scrolls its own transcript and its clickable UI works; the wheel over a *non-mouse* pane pages host scrollback (main screen) or sends arrow keys (alt screen)), **`Ctrl-C` interrupts the focused agent — it NEVER quits** (on a pane whose child already exited it posts a notice pointing at quit; in overlay modes it cancels the overlay like `Esc`; quit via the palette's `quit`, or `Ctrl-Space c` closing the last agent), OSC-52 clipboard writes forward to the outer terminal, and pane resizes SIGWINCH the child. The orchestrator gets the **fleet MCP bridge injected** (`--mcp-config` for claude, `-c mcp_servers…` for codex, the synthesized `OPENCODE_CONFIG` file for opencode, the synthesized `HERMES_HOME/config.yaml` `mcp_servers:` block for hermes; **pi and openclaw have no injectable MCP mechanism** — they run without fleet tools) so it can `spawn_subagent`/`subagent_diff`/… against this repo (see `references/orchestration.md`); its LLM traffic routes through the daemon like `bitrouter launch` (opencode, pi, hermes, and openclaw route via **synthesized config** — an `OPENCODE_CONFIG` JSON / a `PI_CODING_AGENT_DIR` with `models.json` / a `HERMES_HOME` with `config.yaml` (loopback `custom` provider + `CUSTOM_API_KEY`) / an `OPENCLAW_STATE_DIR`+`OPENCLAW_CONFIG_PATH` profile whose `openclaw.json` declares a `bitrouter` provider (openclaw runs its embedded runtime, `tui --local`) — all under `.bitrouter/`, model lists filled from the daemon's `/v1/models`). `--model ID` pins the orchestrator's model (a daemon-routable id, e.g. the explicit `provider:model` form); without it claude/codex keep their own configured model and opencode/pi default to the daemon's first advertised model. **grok and agy are own-auth harnesses**: they are subscription clients whose sessions the daemon itself borrows (`supergrok` / `google-ai` providers), so they launch with their own auth — never redirected, no fleet MCP injection (no non-invasive mechanism) — and `--model` forwards as their native flag (`-m` / `--model`). A configured `agents:` id instead renders that ACP agent as a **read-only `Monitor`** from typed events (streamed text commits per line; fenced code blocks syntax-highlighted; tool diffs render `path +N/-M` with tinted `+`/`-` rows and `⋮` hunk gaps) — **no composer; the human never types into a subagent** (steer it from the orchestrator, or attach with `Ctrl-Space t`). The focused pane's `ctx …% · model · $cost` shows in the status bar's left zone. Gated by the default-on `tui` feature. In-process (no daemon owns the sessions) — run `bitrouter serve` alongside; the TUI probes the listen address at startup (warning notice if nothing is listening) and every ~5s thereafter — the status bar's `serve ●/✗` dot is live, so a daemon dying mid-session is visible immediately. **Isolation**: every agent spawned from the picker gets its **own worktree + branch by default** (`.bitrouter/worktrees/<agent>-<record16>`, branch `bitrouter/<agent>-<record16>`, based on the manager's HEAD) plus a `PORT` from the `worktrees.ports` pool (default 3100–3199, shown as `:PORT`). Worktrees are **retained** on close — cleanup is gated on merged-or-discarded. A configured `worktrees.bootstrap` hook runs in each new worktree; it executes shell, so a CONFIRM overlay shows the command on first spawn each session (`y` run / `n` skip / `Esc` cancel). |
| TUI modes | **NORMAL is the only hub** (TUI_SPEC_V3: supervision is inline; no sticky manager mode). A focused PTY session owns the keyboard (locked-mode passthrough — except `PgUp`/`PgDn`/the wheel, which page the pane's own scrollback; the status bar carries the `⇢ keys go to the orchestrator` hint); a focused `Monitor` is **read-only** — there is no input bar, and typing lands on a notice pointing at the owner. Inline from NORMAL: `y`/`a`/`n` resolve the **top** pending decision (queue head — risk-sorted, oldest first) and advance focus to the next (batch clear; rows with a pending expand to `└ <what it wants>` tagged `high ·`/`low ·`); `D`/`m`/`p`/`r` review the **focused** `Monitor` when its turn ended with a ready diff (`◆ review` + `+N/-M` stat row): load diff · merge · apply · **reject — routed by ownership**: an orchestrator-spawned subagent's reject becomes its **task outcome** (`subagent_status` reports `state: changes_requested` + `review_verdict`; never a prompt), a human-spawned (palette-hatch) subagent is re-prompted directly with the canned rejection note; `Ctrl-C` interrupts the focused agent (raw `0x03` to a PTY child / cancel the ACP turn); `PgUp`/`PgDn` scroll the focused pane's scrollback — a `Monitor`'s line ring (pinned view shows `⇣N`; paging back to the tail resumes following) or a PTY session's **host-owned emulator history** (pinned view shows a `↑ SCROLLBACK` hint; typing or paging back to the bottom returns to the live tail); the **wheel** pages that same history for a `Monitor` or a non-mouse PTY, but forwards to a mouse-reporting inner app, which scrolls itself; **click any sessions/rail row to focus it** (no mode). **LEADER** (one-shot prefix, `tui.leader`, default `Ctrl-Space`): press it → a which-key overlay → exactly one leaf → back to NORMAL — `1`-`9` focus session N · `Tab` focus the next actionable subagent · `n` new session (harness picker) · `p` command palette · `c` close the focused pane (a live orchestrator-owned monitor refuses — close it there; on an attach pane it detaches) · `a` cycle the focused pane's autonomy tier (**manual** → **assisted** low-risk auto-allows → **auto**; every auto-allow is logged `· auto-allowed (…)`, never silent; orchestrator-owned monitors keep their policy in the bridge) · `t` attach the focused live human-owned monitor (native PTY in its worktree, resuming the provider conversation when known) · `?` keys help · `Esc` cancel. **COMMAND** (`:` at a focused Monitor, or `Ctrl-Space p`): fuzzy palette over `spawn subagent` (the **only** direct human-spawn path — not a first-class key) / `new session` / `close agent` / `split horizontal|vertical` (adds the most actionable unshown agent, max 4) / `unsplit` / `autonomy cycle` / `kill done` / `toggle sessions` / `toggle subagents` / `keys help` / `quit`. **PICKER**: `↑`/`↓` · `Enter` · `Esc`. **CONFIRM**: worktree-bootstrap approval (`y`/`n`/`Esc`). The **status bar** is a gauge, not a cheat-sheet: left zone follows the **focused pane** (`ctx N%` context gauge · model · `$cost`; a transient notice claims the zone and decays ~8s) plus the minimal `⌃space menu` affordance; right zone is **global fleet** (attention badges `⚠◆●◉` · summed `$` cost · live `serve ●/✗` dot — no bare session count; the rail lists them). Background attention still rings the bell, posts outer-terminal notifications while unfocused, badges the terminal title (`bitrouter ⚠1 ◆1 ◉2`), and shows time-in-state on rail rows. Durable fleet memory at `.bitrouter/fleet-state.json` and the `.bitrouter/tui.log` stderr repoint are unchanged. |

## Setup helpers

| Command | Effect |
|---|---|
| `bitrouter init [flags]` | Guided onboarding wizard: **credentials** → **harness** → **finish** (launch / serve+snippet / exit). Interactive by default; `--yes` runs it headlessly — process the flags below, never block on a human, emit the JSON result envelope (`action: onboarding`, `providers_configured`, `providers_skipped_interactive`, `harnesses_installed`, `after`, `snippet`), and scaffold the starter `bitrouter.yaml` (`skip_auth: true`, `listen: 127.0.0.1:4356`, common providers stubbed `{}`). The scaffold refuses to overwrite unless `--force`; `--reset` clears stored credentials first (cloud session always, provider creds after a confirm / unconditionally under `--yes`). Flags mirror every prompt: `--cloud-login`, `--api-key <brk_…>` (cloud), `--provider <id>` + `--provider-api-key <k>` (repeatable), `--use-detected`, `--harness claude\|codex` (repeatable), `--no-install`, `--after launch\|serve\|exit`, `--model <id>`, `--write-config`, `-c/--config PATH`. Under `--yes`, anything needing interactive OAuth (bare `--cloud-login`, a `--provider` with no key) is reported in `providers_skipped_interactive`, not attempted. |
| `bitrouter config validate [--config PATH]` | Validate a config file by running the real parse path: structure (deserialization), `derives` resolution, the upstream-URL (SSRF) gate, and any referenced `policy-lock.yaml`. Exits non-zero on an invalid config — **CI-safe**. Does *not* load the JSON Schema (that artifact, at `dist/schema/bitrouter.config.schema.json` / regenerated with `cargo run -p dist-helper -- generate-schema`, is for IDE autocomplete + the drift check). Unset `${VAR}` references are substituted with a `.invalid` placeholder and reported as warnings, so secrets need not be present; a value that embeds one mid-string is not authoritatively checked. |
| `bitrouter policy create <id> [--dir DIR]` | Write a starter access-control policy file under `--dir` (default `./policies`). Bind to a key with `bitrouter key sign --user <id> --policy <id>`. |
| `bitrouter policy init <name> --preset <preset> --economy <model> [--strong <model>] [--config PATH]` | Create or extend the deterministic `policy-lock.yaml`, bind the named policy to a preset, and leave programmatic writeback locked. The strong model is inferred from an existing preset when omitted. |
| `bitrouter policy check|status [--config PATH]` | Cross-validate the main config and lock, or report the resolved path, semantic digest, writeback mode, policies, and preset bindings. |
| `bitrouter policy show <name> [--config PATH]` | Print one validated effective policy. |
| `bitrouter policy evolve [--apply \| --output FILE [--freeze]] [--config PATH]` | Project policy-namespaced adequacy evidence into a deterministic candidate. Dry-run by default; `--apply` requires writeback to be unlocked. `--output` writes a separate atomic candidate while locked and refuses the active lock path; `--freeze` disables future exploration after materializing qualified routes. Existing routes are never overwritten or removed. |
| `bitrouter policy lock|unlock [--config PATH]` | Forbid or permit programmatic replacement of `policy-lock.yaml`. Manual/Git edits and reload remain allowed while locked. |
| `bitrouter policy reload [--config PATH] [--socket PATH]` | Hot-reload main config and policy lock through the existing daemon control socket. Invalid locks preserve the last-known-good runtime snapshot. |
| `bitrouter key sign --user <id> [--db URL] [--policy ID]` | Mint a `brvk_…` virtual key in the auth DB. Plaintext is shown once; only its SHA-256 hash is stored. Default DB is `sqlite://./bitrouter.db`. |

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

Non-TTY JSON, binary bodies, and SSE stream byte-for-byte to stdout. HTTP 4xx/5xx preserves the body on stdout, writes the error to stderr, and exits non-zero. Initial tested endpoints: models, Chat Completions, Messages, Responses, `generateContent`, and `streamGenerateContent`.

Side effect: when the credentials file exists, the local daemon auto-adds the `bitrouter` provider to the zero-config providers map, so every model your account is entitled to is routable as `bitrouter:<model-id>` against `localhost:4356` without further configuration.

## BitRouter Cloud management (`bitrouter cloud …`)

Typed wrappers over the `/v1/*` management API on the cloud. Requires either login form first. OAuth credentials use their baked namespace; API keys use `/v1/namespaces/me/*`. Every leaf accepts `--json` for raw response output; default is a `systemctl`-style key:value block (single resource) or a small table (lists). On a 403 with `missing required scope: <s>`, OAuth users receive a copy-pasteable `--scope` re-login hint; API-key users are directed to a key with that scope.

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
| `bitrouter spawn <agent> -p "<text>" [--no-wait] [--result-schema JSON\|@PATH] [session/routing flags]` | Spawn an ACP sub-agent, send one prompt, stream **NDJSON** to stdout, exit. `<agent>` is a catalog id (`claude-acp`, `codex-acp`, `gemini-cli`, `opencode`, `pi-acp`, `hermes-acp`, `openclaw`) or a configured `agents:` entry; a catalog id needs no config entry. `--result-schema` adds the machine-consumable result contract (see **Result contract** above). |
| `bitrouter spawn <agent> --serve [--warm] [--idle-timeout SECS] [session/routing flags]` | Serve the sub-agent as a vanilla ACP Agent over stdio (for a GUI/manager). Same as `acp serve` with routing on. |
| `bitrouter spawn <agent> --check [routing flags]` | Preflight harness resolution, the routing decision, and daemon reachability without launching anything. |

**Routing (default on)** for `spawn` and the `acp serve\|prompt` aliases:
- `--direct` — do **not** route through the daemon; the harness uses its own provider auth.
- `--model <id>` — pin the harness's model (its model env var, or `-c model=` for codex).
- `--base-url <URL>` — override the gateway URL (else derived from `server.listen`).
- `--no-start` — never auto-start a local daemon; fail fast if it's down.
- Session flags (`--worktree`/`--rm-worktree`/`--no-transcript`/`--turn-timeout`) match `acp`.
- Auth: routed sub-agents authenticate with `BITROUTER_API_KEY` when set, else a local placeholder (fine under `skip_auth: true`); under `skip_auth: false` a key is required or `spawn` fails fast with `auth_required`.
- Fail-fast: if the daemon is unreachable (after auto-start) or auth is required and absent, `spawn` emits a single structured error **before** any session side effect — NDJSON `{"type":"error","code":"daemon_unreachable"|"auth_required",…}` in `-p` mode, stderr in `--serve` mode — and exits non-zero. Catalog harnesses whose routing is config-synthesis only (`opencode`, `pi-acp`, `hermes-acp`, `openclaw` — routed in the `bitrouter tui` orchestrator facet, not headless spawn yet) and non-catalog agents warn and run direct; `hermes-acp` routes headless too if you export a synthesized `HERMES_HOME` + `CUSTOM_API_KEY`, and `openclaw` follows `OPENCLAW_STATE_DIR`/`OPENCLAW_CONFIG_PATH` to a profile whose gateway it auto-starts.
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
