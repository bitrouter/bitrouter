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
| `bitrouter verify <model>` | L1 TEE-attestation check for a confidential model (NEAR AI): prints a per-check breakdown (GPU NRAS, Intel TDX DCAP quote, report_data binding, compose, event-log RTMR3 anchor, base measurements MRTD/RTMR0-2, policy pin, debug-disabled, TCB level) and a VERIFIED / UNVERIFIED verdict. Reads `NEAR_BASE`, `NEAR_KMS_ROOTS`, `NEAR_IMAGE_DIGESTS`/`NEAR_WORKLOAD_IDS` (≥1 pin required), `NEAR_BASE_MEASUREMENTS` (≥1 required — comma-separated hex of `MRTD‖RTMR0‖RTMR1‖RTMR2`, 192 bytes each; pins the firmware base registers the guest-extended RTMR3 sits above), and `NVIDIA_EAT_KEY_PEM` from the environment — the verifier refuses to run unpinned. The TCB floor requires an up-to-date platform by default; set `NEAR_TCB_ALLOWED_ADVISORIES` (comma-separated Intel advisory IDs, e.g. `INTEL-SA-00615`) to accept specific out-of-date microcode. |
| `bitrouter providers list [--config PATH]` | Tab-aligned: `ID  MODELS  ACTIVE  API_BASE`. |
| `bitrouter tools list [--config PATH]` | Enumerate tools advertised by every configured MCP server (one `tools/list` round-trip per server). |
| `bitrouter tools status [--config PATH]` | Health-check each MCP server. Latency or error per row. |
| `bitrouter tools discover <server> [--config PATH]` | Print a YAML stub for the discovered server, paste into `mcp_servers:`. |
| `bitrouter agents list [--remote] [--config PATH]` | Show bundled ACP catalog + which are configured. `--remote` also fetches the official ACP agent registry (cdn.agentclientprotocol.com) and lists its agents with version + install support (`npx`/`uvx` stub-able; `manual` for binary-only). |
| `bitrouter agents check [--config PATH]` | Spawn each configured ACP agent and verify `initialize` round-trip. |
| `bitrouter agents install <id>` | Print a paste-ready YAML stub for `<id>` — resolved from the bundled catalog first, then the ACP registry (`npx`/`uvx` distributions, version-pinned, `env` included). Binary-only registry entries are refused with a manual-install pointer (the registry has no checksums). |
| `bitrouter observe status [--json] [--config PATH] [--socket PATH]` | OTel exporter snapshot: wired / endpoint / sampler / cardinality usage / in-flight spans. JSON output for tooling. |

## ACP sessions

Per-session ACP substrate — one process = one session = one agent. Managers (GUI, AI agents, editors) spawn one process per session and drive it; orchestration is external to the substrate.

| Command | Effect |
|---|---|
| `bitrouter acp serve --agent <id> [--worktree <name>] [--rm-worktree] [--no-transcript] [--turn-timeout SECS] [--warm] [--idle-timeout SECS] [--config PATH]` | Run one session as a vanilla ACP Agent over **stdio** until the manager disconnects. Managers spawn this per session and drive standard ACP (`initialize` → `session/new` → `session/prompt` / `session/cancel` / `session/load`). `--warm` keeps the session alive after disconnect and accepts reattach on a per-session unix socket until `--idle-timeout` (default 1800s) elapses with no manager. Logs go to stderr; stdout carries ACP JSON-RPC. |
| `bitrouter acp prompt --agent <id> [--worktree <name>] [--rm-worktree] [--no-transcript] [--turn-timeout SECS] [--no-wait] [--config PATH] <text>` | Launch a session, send one prompt, stream session updates to **stdout as NDJSON** (one JSON object per line), then exit. Logs go to stderr. `--no-wait` submits and returns `{"type":"submitted"}` without streaming. |
| `bitrouter acp attach <record>` | Reattach a terminal to a warm session by record id (or unique prefix): bridges stdio to the session's unix socket (same NDJSON JSON-RPC framing). Run `initialize` → `session/load` to replay the conversation, then continue live. Unix-only. |
| `bitrouter acp sessions` | List the current repo's session records (`.bitrouter/sessions/*.json`), newest first: short record id, agent, status (`running` / `exited` / `dead` when the recorded pid no longer exists), age, worktree. |

**Worktrees**: `--worktree <name>` provisions `.bitrouter/worktrees/<name>` (created with a same-named branch, or reused/attached when the worktree or branch already exists). Worktrees are **retained** on exit — they hold the agent's work — and the retained path is logged to stderr. `--rm-worktree` opts in to removal at shutdown (destroys uncommitted work; only a worktree the session itself created is removed).

**Transcript**: every session appends a durable NDJSON transcript to `.bitrouter/sessions/<record_id>.transcript.ndjson` — prompts, every raw `session/update` (non-lossy), and per-turn results, each line stamped `{seq, ts}` (monotonic `seq`, unix-ms `ts`). Disable with `--no-transcript`.

**session/new relay**: the manager's `session/new` opens the upstream session, relaying its `cwd` (the launch-time `--worktree` wins) and `mcpServers` **verbatim** — the v2-aligned way for a manager to provide fs/terminal-style tooling (ACP v2 removes the client `fs/*`/`terminal/*` surface; client-side MCP servers replace it). Repeated `session/new` answers with the same stable record id.

**session/load**: replays the durable transcript as `session/update` notifications (user prompts as `user_message_chunk`, upstream updates verbatim) before the response — v1 `session/load` with the semantics ACP v2 standardizes as `session/resume {replayFrom: start}`. `loadSession` is advertised exactly when a transcript exists.

**Warm sessions**: with `--warm`, the record advertises a reattach socket (bound under `$BITROUTER_HOME/sock` — short paths; unix `sun_path` caps ~104 bytes) and `acp sessions` shows the session `running` after the manager leaves. Reattach with `bitrouter acp attach <record>`; the socket speaks the stdio framing (ACP's standardized remote transport replaces it when it ships).

**Observability**: when `plugins.bitrouter-observe` opts telemetry in (same config as the daemon), `acp serve|prompt` emit OTel GenAI agent spans — `invoke_agent <agent>` per turn and `execute_tool <tool>` per completed tool call, correlated by `gen_ai.conversation.id` = record id (the join key to the HTTP router plane when the agent's model calls go through bitrouter).

**Turns**: `session/cancel` is session-scoped — it cancels the active turn upstream *and* flushes the queued backlog (queued prompts resolve `stop_reason: "cancelled"`). `--turn-timeout SECS` sets a per-turn deadline: on elapse the agent is asked to cancel cooperatively (3s grace) before the turn errors.

**NDJSON format** (for `acp prompt`): each update line is a self-describing JSON object with a `type` field (snake_case): `message_chunk`, `thought_chunk`, `tool_call`, `tool_call_update`, `usage` (context-window occupancy: `used`, `size`, optional `cost`). The terminal line is `{"type":"result","stop_reason":"end_turn"}` (ACP wire spelling). In `--no-wait` mode only `{"type":"submitted"}` is emitted.

See `references/sessions.md` for the full per-session model (identity, turn queue, v1 limitations).

## Setup helpers

| Command | Effect |
|---|---|
| `bitrouter init [--config PATH]` | Write a starter `bitrouter.yaml` (default `./bitrouter.yaml`). Refuses to overwrite. Mirrors the zero-config defaults — `skip_auth: true`, `listen: 127.0.0.1:4356`, all built-in providers stubbed as `{}` so they auto-enable when their env var is set. |
| `bitrouter config validate [--config PATH]` | Validate a config file by running the real parse path: structure (deserialization), `derives` resolution, and the upstream-URL (SSRF) gate. Exits non-zero on an invalid config — **CI-safe**. Does *not* load the JSON Schema (that artifact, at `dist/schema/bitrouter.config.schema.json` / regenerated with `cargo run -p dist-helper -- generate-schema`, is for IDE autocomplete + the drift check). Unset `${VAR}` references are substituted with a `.invalid` placeholder and reported as warnings, so secrets need not be present; a value that embeds one mid-string is not authoritatively checked. |
| `bitrouter providers use <id>` | **No-op** in v1 (kept for v0 compatibility). Prints a hint to edit `bitrouter.yaml` instead. |
| `bitrouter policy create <id> [--dir DIR]` | Write a starter policy file under `--dir` (default `./policies`). Bind to a key with `bitrouter key sign --user <id> --policy <id>`. |
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

## Harness spawn

| Command | Effect |
|---|---|
| `bitrouter spawn --agent <claude\|codex> [--config PATH] [--base-url URL] -- <agent args...>` | Launch a coding-agent CLI through BitRouter without editing agent config files. Claude uses child env overrides; Codex uses one-shot `-c` provider overrides with `wire_api="responses"`. After the agent exits, prints a one-line session spend summary to stderr (silent when nothing was recorded locally). |


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
