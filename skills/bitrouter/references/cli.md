# CLI reference

Every subcommand the v1 binary actually exposes. Anything not listed here doesn't exist — don't suggest `bitrouter doctor`, `bitrouter providers add`, `bitrouter cloud connect`, or `bitrouter auth status` (cloud identity is `bitrouter auth whoami`, see below).

## Daemon lifecycle

| Command | Effect |
|---|---|
| `bitrouter serve [--config PATH]` | Run the HTTP server + control socket **in the foreground**. Ctrl-C to stop. |
| `bitrouter start [--config PATH] [--log PATH]` | Spawn `serve` as a detached background process. Stdout/stderr go to `~/.bitrouter/bitrouter.log` unless `--log` overrides. Refuses to start over a live daemon. |
| `bitrouter stop [--config PATH] [--socket PATH]` | Graceful shutdown via the control socket. |
| `bitrouter restart [--config PATH] [--log PATH] [--socket PATH]` | Stop, wait up to 30s for in-flight requests to drain, then start. Escalates to SIGKILL on timeout. |
| `bitrouter reload [--config PATH] [--socket PATH]` | Hot-reload the running daemon's config + routing table. **Also re-pushes provider env vars** from the current shell into the daemon, so `export OPENAI_API_KEY=new...; bitrouter reload` rotates the key without a restart. SIGHUP to the daemon process has the same effect. |
| `bitrouter status [--config PATH] [--socket PATH]` | `systemctl status`-style block: pid / listen / model count / socket. Reports `stopped` (exit 0) when no daemon is reachable. |

## Inspection

| Command | Effect |
|---|---|
| `bitrouter route <model> [--config PATH]` | Resolve a model name through the routing table. Tries the running daemon first (live table), falls back to standalone config resolution. Prints the provider/service chain. |
| `bitrouter models [--config PATH] [--provider ID]` | List every routable model the config exposes. Filter by provider. |
| `bitrouter verify <model>` | L1 TEE-attestation check for a confidential model (NEAR AI): prints a per-check breakdown (GPU NRAS, Intel TDX DCAP quote, report_data binding, compose, event-log RTMR3 anchor, policy pin, debug-disabled, TCB level) and a VERIFIED / UNVERIFIED verdict. Reads `NEAR_BASE`, `NEAR_KMS_ROOTS`, `NEAR_IMAGE_DIGESTS`/`NEAR_WORKLOAD_IDS` (≥1 pin required — the verifier refuses to run unpinned), and `NVIDIA_EAT_KEY_PEM` from the environment. The TCB floor requires an up-to-date platform by default; set `NEAR_TCB_ALLOWED_ADVISORIES` (comma-separated Intel advisory IDs, e.g. `INTEL-SA-00615`) to accept specific out-of-date microcode. |
| `bitrouter providers list [--config PATH]` | Tab-aligned: `ID  MODELS  ACTIVE  API_BASE`. |
| `bitrouter tools list [--config PATH]` | Enumerate tools advertised by every configured MCP server (one `tools/list` round-trip per server). |
| `bitrouter tools status [--config PATH]` | Health-check each MCP server. Latency or error per row. |
| `bitrouter tools discover <server> [--config PATH]` | Print a YAML stub for the discovered server, paste into `mcp_servers:`. |
| `bitrouter agents list [--config PATH]` | Show bundled v1 ACP catalog + which are configured. |
| `bitrouter agents check [--config PATH]` | Spawn each configured ACP agent and verify `initialize` round-trip. |
| `bitrouter agents install <id>` | Print a paste-ready YAML stub for catalog entry `<id>`. |
| `bitrouter observe status [--json] [--config PATH] [--socket PATH]` | OTel exporter snapshot: wired / endpoint / sampler / cardinality usage / in-flight spans. JSON output for tooling. |

## Setup helpers

| Command | Effect |
|---|---|
| `bitrouter init [--config PATH]` | Write a starter `bitrouter.yaml` (default `./bitrouter.yaml`). Refuses to overwrite. Mirrors the zero-config defaults — `skip_auth: true`, `listen: 127.0.0.1:4356`, all built-in providers stubbed as `{}` so they auto-enable when their env var is set. |
| `bitrouter config validate [--config PATH]` | Validate a config file by running the real parse path: structure (deserialization), `derives` resolution, and the upstream-URL (SSRF) gate. Exits non-zero on an invalid config — **CI-safe**. Does *not* load the JSON Schema (that artifact, at `schemas/bitrouter.config.schema.json` / regenerated with `cargo xtask generate-schema`, is for IDE autocomplete + the drift check). Unset `${VAR}` references are substituted with a `.invalid` placeholder and reported as warnings, so secrets need not be present; a value that embeds one mid-string is not authoritatively checked. |
| `bitrouter providers use <id>` | **No-op** in v1 (kept for v0 compatibility). Prints a hint to edit `bitrouter.yaml` instead. |
| `bitrouter policy create <id> [--dir DIR]` | Write a starter policy file under `--dir` (default `./policies`). Bind to a key with `bitrouter key sign --user <id> --policy <id>`. |
| `bitrouter key sign --user <id> [--db URL] [--policy ID]` | Mint a `brvk_…` virtual key in the auth DB. Plaintext is shown once; only its SHA-256 hash is stored. Default DB is `sqlite://./bitrouter.db`. |

## Per-provider OAuth

| Command | Effect |
|---|---|
| `bitrouter login <provider>` | Per-provider OAuth. Today the only supported provider is **`github-copilot`** — runs the GitHub device flow and stores `ghu_…` under `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. |
| `bitrouter logout <provider>` | Remove the stored OAuth token for `<provider>`. |
| `bitrouter login` (no arg) | Legacy shim. Prints a pointer to `bitrouter auth login` (for cloud sign-in) and `bitrouter key sign --user <id>` (for local virtual keys). |
| `bitrouter logout` (no arg) | Legacy shim. Pointer to `bitrouter auth logout`. |
| `bitrouter whoami` (no arg) | Legacy shim. Pointer to `bitrouter auth whoami` (offline local read) and `bitrouter cloud whoami` (local read + cloud base URL). |

## BitRouter Cloud sign-in (`bitrouter auth …`)

OAuth 2.0 device-flow sign-in against the BitRouter Cloud authorization server. The persisted credential drives both the `bitrouter` provider in the local daemon and the management subcommands below.

| Command | Effect |
|---|---|
| `bitrouter auth login [--oauth-as URL] [--client-id ID] [--scope SCOPE]` | RFC 8628 device-flow login. Prints an approval URL, polls the token endpoint, and persists access + refresh tokens to `$XDG_DATA_HOME/bitrouter/account-credentials.json` (mode 0600 on Unix). Auto-refreshes within 60 s of access-token expiry on every subsequent call. Defaults: AS `https://api.bitrouter.ai`, client id `bitrouter-cli`, scope set covering `inference:invoke usage:read keys:* billing:read policy:* byok:* account:read`. Override the AS or scope for a self-hosted deployment or to opt into the sensitive scopes (`billing:write`, `account:write`, `clients:*`). |
| `bitrouter auth logout` | Best-effort RFC 7009 revoke at the AS, then delete the local credentials file. |
| `bitrouter auth whoami` | Print the local credential's AS, client id, scope, subject, expiry. Reads the on-disk file only — no network. |

Side effect: when the credentials file exists, the local daemon auto-adds the `bitrouter` provider to the zero-config providers map, so every model your account is entitled to is routable as `bitrouter:<model-id>` against `localhost:4356` without further configuration.

## BitRouter Cloud management (`bitrouter cloud …`)

Typed wrappers over the `/v1/*` management API on the cloud. Requires `bitrouter auth login` first. Every leaf accepts `--json` for raw response output; default is a `systemctl`-style key:value block (single resource) or a small table (lists). On a 403 with `missing required scope: <s>`, the CLI prints a copy-pasteable `bitrouter auth login --scope "<current> <s>"` hint.

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
| `bitrouter cloud oauth-client list/register/update/delete` | Registered OAuth clients on the account. Confidential clients return `client_secret` exactly once at `register`. Scope: `clients:read` / `clients:write` (opt-in via `--scope`). |

## ACP bridge

| Command | Effect |
|---|---|
| `bitrouter agent-proxy <agent> [--config PATH]` | Stdio bridge between an ACP-aware editor and an upstream agent declared under `agents:`. Editor spawns this binary as a subprocess. Routes JSON-RPC newline-framed over stdio. |

## Unimplemented in v1.0

These print `not implemented in v1.0` today and are unlikely to land in the proxy binary:

- `bitrouter wallet` — OWS wallet integration lives in the separate `ows` workspace, not in the proxy binary.

The bare `bitrouter login` / `bitrouter logout` / `bitrouter whoami` (no argument) are **not** in this list — they print a redirect (exit 0) pointing the user at the right surface (`bitrouter auth …` for cloud sign-in, `bitrouter key sign` for local virtual keys, `bitrouter cloud whoami` for cloud identity).

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
| SIGHUP | Hot-reload config + routing table (same as `bitrouter reload`). |
| SIGINT / SIGTERM | Graceful shutdown: flush OTel exporter, remove pid file, exit 0. |
| SIGKILL | No cleanup — pid file will be stale and `bitrouter status` will report it. `bitrouter start` cleans up stale pid files automatically before launching. |
