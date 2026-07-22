# BitRouter CLI Reference

`bitrouter <subcommand> [flags]`

## Output format

Every command prints a single **formatted JSON object** to **stdout** — success or failure — so output is machine-parseable by default (agent-native first). Global flags:

- `-j`, `--json` — force JSON (the default).
- `--human` — render a human-readable view to stdout instead of JSON.
- `-H` before the subcommand (for example, `bitrouter -H cloud whoami`) — compatibility spelling for `--human`. Under `bitrouter cloud api`, `-H` means `--header`, matching `gh api`.
- `-h`, `--help` — unchanged (`-h` is **not** human output).

All diagnostics — progress, warnings, internal logs, and a human echo of errors — go to **stderr** (colored when stderr is a TTY; honors `NO_COLOR`). So:

```
bitrouter <cmd> 2>/dev/null | jq .
```

always yields one clean JSON value. A failed command emits a uniform error envelope to stdout and exits non-zero:

```json
{ "error": { "kind": "not_found", "message": "…", "context": ["…"], "hint": "…" } }
```

`kind` is a stable taxonomy (`bad_request` / `unauthorized` / `forbidden` / `not_found` / `upstream` / `internal` / …). Under `--human`, the result (success object or error block) is rendered to stdout in the human form and no JSON is printed.

> Non-CLI commands are exempt: `serve` and `mcp serve` are long-running servers, `acp serve` and `acp attach` are stdio JSON-RPC bridges, `acp prompt` streams NDJSON, `cloud api` streams the remote response body, and `spawn` hands its streams to the child agent. Their stdout is a wire protocol, raw response, or the child's terminal—not a JSON result envelope.

Per-provider credential commands are under `bitrouter providers (login|logout)`; BitRouter Cloud sign-in is `bitrouter cloud (login|logout|whoami)`.

## Config resolution

Local router subcommands that load a config accept an optional `-c / --config <path>` flag. When omitted the binary walks this order:

1. `./bitrouter.yaml` in the current directory
2. `$BITROUTER_HOME/bitrouter.yaml` — if the env var is set, the file must exist
3. `~/.bitrouter/bitrouter.yaml` — used if present
4. **Zero-config** — in-memory defaults; auto-enables any provider whose API key is set in the environment

Daemon-control subcommands (`stop`, `reload`, `status`) also accept `--socket <path>` to override the control socket path derived from the config.

---

## Daemon lifecycle

### `bitrouter serve`

Run the HTTP server and control socket **in the foreground**.

```
bitrouter serve [-c <path>]
```

Starts the proxy on the configured listen address (default `127.0.0.1:4356`) and opens a Unix domain control socket. Logs to stdout.

### `bitrouter start`

Spawn `serve` as a **detached background daemon**.

```
bitrouter start [-c <path>] [--log <path>]
```

Logs default to `bitrouter.log` next to the config file (e.g. `~/.bitrouter/bitrouter.log` when the config resolved to `~/.bitrouter/bitrouter.yaml`). Refuses to start if a daemon is already running.

Waits until the daemon answers on its control socket before reporting `✓ … started` (up to 15s), then prints the listen address and routable-model count — so a follow-up command can rely on the daemon being up. If the daemon crashes during startup, the tail of its log is printed and the command exits non-zero; if it is alive but still not ready after 15s, a note is printed and the command exits 0 (the daemon keeps coming up).

### `bitrouter stop`

```
bitrouter stop [-c <path>] [--socket <path>]
```

### `bitrouter restart`

```
bitrouter restart [-c <path>] [--socket <path>] [--log <path>]
```

Stops the running daemon (waiting up to 30s for in-flight requests to drain), then starts a fresh one.

### `bitrouter reload`

```
bitrouter reload [-c <path>] [--socket <path>]
```

Hot-reloads the running daemon's config and routing table without dropping connections. Also triggered by `SIGHUP`.

Any provider API keys present in the current environment are forwarded to the daemon so `export OPENAI_API_KEY=…; bitrouter reload` takes effect immediately.

### `bitrouter status`

```
bitrouter status [-c <path>] [--socket <path>]
```

Prints pid, listen address, number of routable models, and control socket path. Exits cleanly with "stopped" when no daemon is reachable.

---

## Config

### `bitrouter init` (onboarding wizard)

```
bitrouter                            # bare: wizard when unconfigured, else status + hint
bitrouter init                       # (re-)run the wizard interactively
bitrouter init --yes [flags]         # headless: emit the JSON envelope, scaffold the config
bitrouter init --force               # allow overwriting an existing bitrouter.yaml
bitrouter init --reset               # clear stored credentials, then run
```

Bare `bitrouter` (no subcommand) is the front door. It runs a **network-free credential probe** — BYOK env keys (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, `OPENCODE_ZEN_API_KEY`), the cloud session file, and the local credential store — and either launches the guided wizard (nothing configured) or prints a one-line status + a `bitrouter launch` hint (already configured). It never re-onboards a configured user and never silently spawns a daemon or harness. Exit code 0 either way.

The wizard is three steps, each mapping to a flag so an agent can drive it: **credentials** (sign in to BitRouter Cloud, log in to a provider, or paste a BYOK key), **harness** (`claude` / `codex`, installed via the native installer when missing), and **finish** — launch the harness now, start the daemon and print paste-in snippets, or exit. The only durable output is **credentials** (which zero-config auto-detects); the wizard never serializes `bitrouter.yaml` except the canned starter template.

`bitrouter init --yes` runs the whole thing non-interactively and **never blocks on a human**: it consumes the flag-supplied keys, reports-and-skips anything that would need interactive OAuth (in `providers_skipped_interactive`), emits the JSON result envelope on stdout, and reproduces the classic starter-file scaffold (`skip_auth: true`; refuses to overwrite unless `--force`).

| Flag | Step | Description |
| --- | --- | --- |
| `-c`, `--config <path>` | — | Starter-config write path (default `bitrouter.yaml`). |
| `--yes`, `-y` | — | Headless: process the flags, never block, emit the envelope, scaffold the config. |
| `--force` | — | Overwrite an existing `bitrouter.yaml` when scaffolding. |
| `--reset` | — | Clear stored credentials first — cloud session always; provider credentials after a confirm, or unconditionally under `--yes`. |
| `--cloud-login` | 1 | Sign in to BitRouter Cloud (device flow). Skipped-and-reported under `--yes`. |
| `--api-key <brk_…>` | 1 | Seed the cloud credential from a `brk_` key (non-interactive). |
| `--provider <id>` | 1 | Log in to an upstream provider (repeatable). |
| `--provider-api-key <k>` | 1 | Key for the `--provider` at the same position (repeatable). |
| `--use-detected` | 1 | Accept the auto-detected credential(s) without prompting. |
| `--harness <claude\|codex>` | 2 | Harness to drive (repeatable). |
| `--no-install` | 2 | Never install a missing harness. |
| `--after <launch\|serve\|exit>` | 3 | Finish action (default `exit`; `launch` is honored only when the harness is present). |
| `--model <id>` | 3 | Model handed to the harness for this session only (not persisted). |
| `--write-config` | 3 | Write the starter `bitrouter.yaml`. |

The result envelope:

```json
{
  "action": "onboarding",
  "providers_configured": ["bitrouter", "openai"],
  "providers_skipped_interactive": ["github-copilot"],
  "harnesses_installed": ["claude"],
  "after": "launch",
  "snippet": null
}
```

---

## Routing / introspection

### `bitrouter route <model>`

```
bitrouter route gpt-4o [-c <path>] [--socket <path>]
```

Resolves a model name through the routing table and prints the full fallback chain (provider → upstream service id → protocol). Queries the running daemon if reachable; falls back to a local config parse.

### `bitrouter models`

```
bitrouter models [-c <path>] [-p <provider-id>]
```

Lists all routable models. Filter by provider with `--provider`.

### `bitrouter providers list`

```
bitrouter providers list [-c <path>]
```

Prints each configured provider's id, model count, active state, and API base URL.

---

## MCP tool introspection

### `bitrouter tools list`

```
bitrouter tools list [-c <path>]
```

Connects to every `mcp_servers` entry in the config and lists advertised tools with descriptions.

### `bitrouter tools status`

```
bitrouter tools status [-c <path>]
```

Health-checks each configured MCP server with a `tools/list` round-trip. Prints status, latency, and transport.

### `bitrouter tools discover <server>`

```
bitrouter tools discover my-server [-c <path>]
```

Connects to one MCP server and prints a YAML stub suitable for pasting into the `mcp_servers:` block of `bitrouter.yaml`.

---

## ACP agent management

### `bitrouter agents list`

```
bitrouter agents list [-c <path>]
```

Shows the built-in agent catalog alongside which agents are configured in the loaded config.

### `bitrouter agents check`

```
bitrouter agents check [-c <path>]
```

Spawns each configured agent and verifies it responds to `initialize`. Prints latency or error per agent.

### `bitrouter agents install <id>`

```
bitrouter agents install claude-code
```

Prints a YAML stub for the named catalog agent. Paste the output under `agents:` in `bitrouter.yaml`.

### `bitrouter acp`

```
bitrouter acp serve --agent <id> [--worktree <name>] [--warm] [-c <path>]
bitrouter acp prompt --agent <id> [--worktree <name>] [-c <path>] <text>
bitrouter acp sessions
bitrouter acp attach <record>
```

Runs one configured ACP agent session. `serve` exposes a vanilla ACP Agent over stdio until the manager disconnects; with `--warm`, the session can be reattached with `acp attach`. `prompt` launches one session, sends one prompt, and streams self-describing NDJSON updates to stdout. Session records live under `.bitrouter/sessions/`. `acp serve|prompt` are stable aliases of `bitrouter spawn <agent> --serve|-p` (below) and, like it, route the agent's model calls through the daemon by default (`--direct` opts out).

### `bitrouter launch`

```
bitrouter launch -a <agent> [-c <path>] [--base-url <url>] [--no-install] [--no-start] [--check] -- <agent args…>
```

Launches a coding-agent harness (`-a claude` for Claude Code, `-a codex` for Codex CLI) as an **interactive native-TUI** child process with its gateway base URL pointed at BitRouter, so the agent's traffic routes through the router **without touching the agent's own config files**. This is the *main orchestrator* surface — the human drives the harness's own TUI; for headless ACP sub-agents use `bitrouter spawn`. Claude Code gets child-process environment overrides (`ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN`); Codex gets one-shot `-c` config overrides for a `bitrouter` provider (`base_url = <target>/v1`, `wire_api = "responses"`). Following `cargo run`'s convention, everything after `--` is forwarded to the agent verbatim, e.g. `bitrouter launch -a claude -- -p "summarize" --dangerously-skip-permissions` or `bitrouter launch -a codex -- --model openai/gpt-5-codex`.

The agent authenticates to BitRouter with `BITROUTER_API_KEY` when set; otherwise a local placeholder is used (fine under the `skip_auth` default written by `bitrouter init`). A missing agent binary is offered for install via its official native installer (`--no-install`, or a non-TTY stdin, declines).

When the target is the local daemon (a derived base URL on a loopback/wildcard bind) and none is running, `launch` **auto-starts it** — printing a hint, launching a detached `serve`, and waiting for readiness before handing off to the agent. Pass `--no-start` to skip this (a reachability warning is printed instead). An explicit `--base-url` or a non-local bind is never auto-started — BitRouter can't start someone else's daemon — and only gets a warning if it looks unreachable.

After the wrapped agent exits, `launch` prints a one-line session spend summary to stderr (spend during the run + today's total, from the local metering database). Silent when nothing was recorded in the window — e.g. when the run targeted Cloud.

`bitrouter spawn --agent <claude|codex>` is a **deprecated alias** for `launch` (prints a migration note); it will be removed after one or two alpha releases.

### `bitrouter tui`

```
bitrouter tui --agent <id> [--worktree <name>] [--model ID]
```

Launches the **composite multi-agent TUI** (TUI_SPEC_V3 — a pure control tower): a sessions sidebar (orchestrator PTY sessions), a subagents rail (ACP agents sorted by who needs you), and a focused detail pane over a one-line status bar. `--agent claude|codex|…` hosts that harness's real native TUI on a PTY as the *orchestrator*; a configured `agents:` id renders that ACP agent as a **read-only `Monitor`** — **there is no input bar anywhere**: the human never types into a subagent (the orchestrator steers subagents via the injected fleet MCP tools; `bitrouter serve` should be running alongside — the bar's `serve ●/✗` dot is live).

**Keys.** NORMAL is the only hub — no sticky manager mode. A focused PTY session gets full key passthrough; the **only intercepted chord is the one-shot leader** (`tui.leader` in `bitrouter.yaml`, `ctrl-<key>` form, default `Ctrl-Space` — `Ctrl-A`/`Ctrl-B` reach the child as readline keys). Leader leaves (which-key overlay, one key, back to NORMAL): `1`-`9` focus session · `Tab` next actionable subagent · `n` new session (picker) · `p` command palette · `c` close · `a` autonomy tier · `t` attach · `?` keys help. Inline from NORMAL: `y`/`a`/`n` resolve the **top** pending permission (batch-clears, focus advances) · `D`/`m`/`p`/`r` review the focused Monitor's ready diff (diff / merge / apply / **reject — routed by ownership**: orchestrator-spawned → the verdict becomes the subagent's task outcome, `subagent_status` shows `changes_requested` + note; human-spawned via the palette's `spawn subagent` hatch → re-prompted directly) · `Ctrl-C` interrupts the focused agent (quits from NORMAL only when the focused pane's child has already exited) · `PgUp`/`PgDn` scroll · click any row to focus it. Quit via the palette's `quit` or by closing the last pane.

**Status bar.** A gauge, not a cheat-sheet: the left zone follows the focused pane (`ctx N%` context occupancy · model · `$cost`; transient notices claim it and decay), plus the `⌃space menu` affordance; the right zone is global fleet (attention badges `⚠◆●◉` · summed `$` cost · `serve ●/✗`).

Subagents spawned from the picker get worktree isolation + a `PORT` by default (retained on close); a configured `worktrees.bootstrap` hook is human-approved on first use per session. Durable fleet memory lives at `.bitrouter/fleet-state.json`; the TUI's stderr (and its agent children's) goes to `.bitrouter/tui.log` on Unix.

### `bitrouter spawn`

```
bitrouter spawn <agent> -p "<text>" [--no-wait] [--result-schema JSON|@PATH] [routing/session flags]   # one prompt → NDJSON
bitrouter spawn <agent> --serve [--warm] [--idle-timeout SECS] [flags]        # ACP over stdio
bitrouter spawn <agent> --check [routing flags]                              # preflight only
```

Spawns an **ACP-compatible harness as a headless sub-agent**, driven by a program (an orchestrating agent, a GUI, or `bitrouter tui`). `<agent>` is a bundled-catalog id (`claude-acp`, `codex-acp`, `gemini-cli`, `opencode`, `pi-acp`, `hermes-acp`, `openclaw`) or a configured `agents:` entry; a catalog id needs no config entry. This subsumes `bitrouter acp serve|prompt` (which remain as stable aliases) and adds routing.

**Routes the sub-agent's LLM traffic through the daemon by default** — the same per-harness knowledge `launch` uses, from one shared catalog (so `launch claude` and `spawn claude-acp` inject identical gateway env/args). Routing flags: `--direct` (opt out — use the harness's own provider auth), `--model <id>` (pin the model), `--base-url <url>` (override the gateway URL), `--no-start` (never auto-start the daemon). Session flags match `acp` (`--worktree`/`--rm-worktree`/`--no-transcript`/`--turn-timeout`).

Routed sub-agents authenticate with `BITROUTER_API_KEY` when set, else a local placeholder (valid under `skip_auth: true`); under `skip_auth: false` a key is required. If the daemon is unreachable after auto-start, or a required key is missing, `spawn` **fails fast before any session side effect** — a single NDJSON `{"type":"error","code":"daemon_unreachable"|"auth_required",…}` line in `-p` mode (stderr in `--serve` mode), exit non-zero. Catalog harnesses whose routing is config-synthesis only (`opencode`, `pi-acp`, `hermes-acp`, `openclaw` — routed in the `bitrouter tui` orchestrator facet, not headless spawn yet) and non-catalog agents warn and run direct.

`--result-schema '<JSON Schema>'` (or `@path`) adds a machine-consumable result contract to `-p` mode: the schema rides the prompt, the reply's last ```json block is extracted and validated (one repair re-prompt on invalid output), and the terminal `result` line gains `result`/`schema_ok` fields — `result:null, schema_ok:false, raw:"…"` after a failed repair, so the orchestrator is never blocked. Bare `-p` output is unchanged.

In `-p` mode the **first** NDJSON line is a `session` correlation line — `{"type":"session","record_id":"…","agent":"…","via":"http://127.0.0.1:4356"}` (`via` is `null` when `--direct`) — followed by the normal update stream and a terminal `result` line.

### `bitrouter key sign`

```
bitrouter key sign --user <id> [--db <url>] [--policy <policy-id>]
```

Mints a scoped `brvk_` virtual key for a user. The plaintext secret is printed once — only its SHA-256 hash is stored.

| Flag | Default | Description |
| --- | --- | --- |
| `--user` | *(required)* | Owning user id |
| `--db` | `sqlite://./bitrouter.db` | Database URL — `sqlite://`, `postgres://`, or `mysql://` |
| `--policy` | *(none)* | Policy id to bind to the key |

### `bitrouter providers login <provider>`

```
bitrouter providers login claude-code     # Claude Pro/Max subscription via Claude Code
bitrouter providers login openai-codex    # ChatGPT subscription via Codex
bitrouter providers login github-copilot  # GitHub device-code flow
bitrouter providers login openai --api-key sk-…        # BYOK, non-interactive
printf %s "$KEY" | bitrouter providers login anthropic --key-stdin
```

Runs the provider's OAuth flow (PKCE in a browser or device-code, depending on provider) and stores the token in `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. The slot is keyed by `(provider_id, label)` — pass `--label <name>` (defaults to `default`) to keep multiple accounts of the same provider side by side. Other providers fall back to a pasted API key.

For a provider that accepts a pasted key, `--api-key <KEY>` (or `--key-stdin`, which reads one line from stdin) seeds it non-interactively — skipping the method menu and the paste prompt. Both conflict with the OAuth-only `--import-existing` / `--no-browser`, and error if the provider has no API-key method. For the built-in `bitrouter` provider the key seeds the cloud credential, exactly as `cloud login --api-key` does.

For `claude-code`, the login menu defaults to the live Claude Code session. For `openai-codex`, the default is **"Import an existing session from the vendor CLI"** — BitRouter reads the credential Codex already stored in `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) first, then the macOS Keychain, and adopts it with no fresh browser sign-in. The imported token refreshes automatically like any other; choose the browser subscription flow when no local Codex session exists.

For cloud sign-in (signing into your BitRouter Cloud account, not an upstream LLM provider), see [`bitrouter cloud login`](#bitrouter-cloud-login--logout--whoami) below.

### `bitrouter providers logout <provider>`

```
bitrouter providers logout github-copilot
```

Removes every stored credential for the provider (subscription OAuth tokens and pasted API keys alike).

### `bitrouter cloud login` / `logout` / `whoami`

Cloud sign-in, distinct from the per-provider `bitrouter providers login` flow above. Interactive login uses the RFC 8628 OAuth Device Authorization Grant. For CI and other non-interactive environments, pass an existing BitRouter API key with `--api-key`. Both forms persist to the same credential file and are reused by `cloud api`, management commands, the built-in `bitrouter` provider, and account-attributed telemetry.

OAuth browser approval asks which workspace to bind; the resulting credential is **namespace-baked** (workspace-baked). To switch workspaces, re-run `bitrouter cloud login`. OAuth credentials auto-refresh on use. API-key login performs no network request and management commands use the server's `me` namespace alias.

```
bitrouter cloud login [--oauth-as <URL>] [--client-id <ID>] [--scope <SCOPE>]
bitrouter cloud login --api-key <BRK_API_KEY> [--oauth-as <URL>]
bitrouter cloud logout [--oauth-as <URL>] [--client-id <ID>]
bitrouter cloud whoami
```

| Flag | Default | Description |
| --- | --- | --- |
| `--oauth-as` | `https://api.bitrouter.ai` (env: `BITROUTER_OAUTH_AS`) | Authorization server base URL — override only for a self-hosted deployment |
| `--client-id` | `bitrouter-cli` (env: `BITROUTER_OAUTH_CLIENT_ID`) | Public OAuth client id |
| `--scope` | broad developer set (env: `BITROUTER_OAUTH_SCOPE`) | Space-delimited scopes to request. Default includes `inference:invoke`, `usage:read`, `keys:read`/`write`, `billing:read`, `policy:read`/`write`, `byok:read`/`write`, `namespace:read`. Sensitive control-plane scopes such as `billing:write`, `user:write`, and `namespace:write` are opt-in. |
| `--api-key` | *(none)* | Store a `brk_<token_id>.<secret>` credential without browser login or network discovery. Conflicts with `--client-id` and `--scope`; intended for CI. |

Credentials are persisted at `<data-dir>/account-credentials.json` (mode `0600` on Unix). Existing untagged OAuth files remain compatible. `whoami` answers from the local file with no network call and reports `authentication: oauth|api_key` without printing a bearer. OAuth logout attempts RFC 7009 revocation before deleting the file; API-key logout is local-only.

---

## Workflow-state benchmark evidence

These commands export and validate the request-scoped evidence used by policy
benchmarks:

```text
bitrouter workflow-state harbor-outcomes --harbor-run-dir <DIR> --output <JSONL>
bitrouter workflow-state metering-usage --database-url <URL> --output <JSONL> [--since <RFC3339>] [--until <RFC3339>] [--impute-price <SPEC> ...]
bitrouter workflow-state reconcile-metering --database-url <URL> [--api-base <URL>] [--api-key-env <NAME> | --credentials-file <PATH>] --request-id <ID> ... [--price <SPEC> ...] [--max-attempts <N>] [--poll-interval-ms <MS>]
bitrouter workflow-state reliability-report --database-url <URL> --config <PATH> --output <JSON>
bitrouter workflow-state bundle --run-label <LABEL> --traces <JSONL> --cloud-usage <JSONL> --outcomes <JSONL> [--policy-decisions <JSONL>] --output-dir <DIR>
bitrouter workflow-state apply-reward-feedback --database-url <URL> --traces <JSONL> --cloud-usage <JSONL> --outcomes <JSONL> --policy-decisions <JSONL>
```

`reconcile-metering` accepts either the API-key environment named by
`--api-key-env` (default `BITROUTER_API_KEY`) or an owner-only BitRouter Cloud
credential file. The latter resolves static keys and refreshable OAuth without
putting a bearer in the environment. Price specs use
`provider:model=uncached,cache_read,cache_write,output` in micro-USD per token.

`bundle` is fail-closed: every non-empty trace set needs an exact usage join and
computed auditable charge, policy decisions must cover the exact request-id set
when supplied, and outcomes must join through an explicit session/trial key.
Timestamp-only reward attribution is available to analytical in-memory reports,
not accepted benchmark bundles.

---

## Policy

### `bitrouter policy create <id>`

```
bitrouter policy create strict [--dir ./policies]
```

Writes a starter policy file to the policy directory. Bind it to a key with:

```
bitrouter key sign --user <id> --policy strict
```

---

## Cloud account management

`bitrouter cloud …` drives the BitRouter Cloud API using the credential persisted by [`bitrouter cloud login`](#bitrouter-cloud-login--logout--whoami). Sign in first, then call a typed management subcommand or the generic API command.

OAuth credentials are **namespace-baked** — keys, usage, and policies are scoped to the workspace chosen at login. API-key credentials use `/v1/namespaces/me/*`. The path segment is always resolved implicitly; callers never pass a workspace argument. `billing` and `byok` are user-level and reach across all workspaces regardless.

Every leaf accepts `--json` to print the raw response body instead of the human-readable summary. On a 403 whose description is `missing required scope: <s>`, OAuth users receive a copy-pasteable re-login hint that appends the missing scope; API-key users are told to mint or select a key with that scope and log in with it.

### `bitrouter cloud api`

Make an authenticated request to any **relative** endpoint on the origin recorded by `cloud login`, modeled after [`gh api`](https://cli.github.com/manual/gh_api):

```bash
bitrouter cloud api /v1/models
bitrouter cloud api /v1/chat/completions --input request.json
bitrouter cloud api /v1/responses -f model=openai/gpt-5 -F stream=true
```

```text
bitrouter cloud api <ENDPOINT> [-X <METHOD>] [-H <KEY:VALUE>] \
  [-f <KEY=VALUE>] [-F <KEY=VALUE>] [--input <FILE|->] \
  [-i|--include] [--silent|--verbose]
```

| Flag | Behavior |
| --- | --- |
| `-X`, `--method` | Explicit HTTP method. Default is `GET`, or `POST` when fields or `--input` are present. |
| `-H`, `--header` | Append a request header; repeat to send multiple values. A supplied `Authorization` overrides the stored bearer. |
| `-f`, `--raw-field` | Add a string field. Supports `key[subkey]` and `key[]` nesting; `key[]` without `=` creates an empty array. |
| `-F`, `--field` | Add a typed field. `true`, `false`, `null`, and integers become JSON types; `@file` and `@-` read a string value from a file or stdin. |
| `--input` | Use exact file bytes (or stdin with `-`) as the request body. Fields become query parameters. |
| `-i`, `--include` | Prepend the HTTP status line and response headers to stdout. |
| `--silent` | Drain but do not print the response body. Conflicts with `--verbose`. |
| `--verbose` | Print method, URL, status, and headers to stderr. Credential-like header values are redacted. |

With explicit `GET`, fields are query parameters. Otherwise fields form a JSON body unless `--input` owns the body. Only one consumer may read stdin. Non-TTY response bytes and SSE are streamed unchanged; interactive JSON is pretty-printed. On HTTP 4xx/5xx, the response body remains on stdout, the diagnostic goes to stderr, and the process exits non-zero.

Absolute URLs, scheme-relative paths, fragments, and cross-origin redirects are rejected. Redirect following is disabled, so a stored bearer is never forwarded to another origin. Initial documented endpoints are `/v1/models`, `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, and Google-style `:generateContent` / `:streamGenerateContent` routes under `/v1beta/models/*`.

This first release intentionally omits `gh api`'s GraphQL, pagination/slurp, `--jq`, Go templates, cache, hostname, preview, and placeholder expansion features. See the [Cloud API guide](/docs/guides/cloud-api) for copyable requests.

### `bitrouter cloud whoami`

```
bitrouter cloud whoami
```

Prints the cloud identity and the bound namespace alongside the `/v1/*` base URL the CLI will target. Reads the local credentials file only — no network call.

### `bitrouter cloud namespace`

Inspect the workspaces you own and the one this CLI session is baked to. Workspace creation and deletion are Console-only operations (control-plane scope).

```
bitrouter cloud namespace list    [--json]
bitrouter cloud namespace current [--json]
```

`list` fetches all namespaces you own and marks the active one. `current` is offline — it reads the local credential and prints the bound namespace id without a network call. If the credential predates namespace-scoping, it prints `(no namespace — run \`bitrouter cloud login\`)`.

### `bitrouter cloud keys`

Manage `brk_` API keys in the active workspace. All minted keys are workspace-baked to the same namespace as the caller and cannot upscale their scopes beyond the caller's.

```
bitrouter cloud keys list [--json]
bitrouter cloud keys mint --name <NAME> --scope <SCOPE> [--scope <SCOPE> …] [--expires-at <RFC3339>] [--json]
bitrouter cloud keys revoke <ID> [--json]
```

Requested scopes on `mint` must be a subset of your effective scopes (RFC 6749 §3.3 — no upscaling). The plaintext token is shown once in the `mint` response and is not recoverable after.

### `bitrouter cloud usage` / `bitrouter cloud requests`

Read aggregate spend / token counts and page through recent inference requests.

```
bitrouter cloud usage    [--from <RFC3339>] [--to <RFC3339>] [--json]
bitrouter cloud requests [--limit <N>] [--offset <N>] [--json]
```

`usage` defaults to a 30-day rolling window. `requests` clamps the page size to `[1, 100]` and defaults to 25.

### `bitrouter cloud billing`

User-level — not workspace-scoped; reflects the account-wide wallet regardless of which workspace the CLI is signed in to.

```
bitrouter cloud billing balance [--json]
bitrouter cloud billing checkout --amount-cents <N> [--json]
```

`checkout` starts a Stripe credit-purchase session and prints the hosted URL. Requires the `billing:write` scope, which is opt-in — pass `--scope` to `bitrouter cloud login` to request it.

### `bitrouter cloud policy`

Generic CRUD over the typed policy registry (kinds: `budget`, `rate_limit`, `guardrail`, `preset`).

```
bitrouter cloud policy list [--kind <KIND>] [--json]
bitrouter cloud policy get <ID> [--json]
bitrouter cloud policy create --name <NAME> --kind <KIND> --spec <FILE|-> [--json]
bitrouter cloud policy update <ID> [--name <NAME>] [--spec <FILE|->] [--json]
bitrouter cloud policy delete <ID> [--json]
bitrouter cloud policy bind <ID> --principal-type <TYPE> --principal-id <ID> [--json]
bitrouter cloud policy unbind <ID> <BINDING_ID> [--json]
bitrouter cloud policy enable <ID> [--json]
bitrouter cloud policy disable <ID> [--json]
bitrouter cloud policy bindings <ID> [--json]
bitrouter cloud policy effective --principal-type <TYPE> --principal-id <ID> [--json]
bitrouter cloud policy for-principal <TYPE> <ID> [--json]
```

`--spec` reads the flat inner spec body as JSON from a file path or `-` for stdin. Principal types: `namespace`, `api_key`, `oauth_token`, `oauth_client`. `disable` parks a policy without deleting it — the engine skips disabled rows at request time.

### `bitrouter cloud budget` / `bitrouter cloud preset`

Typed wrappers over the budget-kind and preset-kind policy rows — same storage, flat wire shape (no `kind`/`spec` envelope).

```
bitrouter cloud budget list [--json]
bitrouter cloud budget get <ID> [--json]
bitrouter cloud budget create --name <NAME> --window <day|month|total> --limit-micro-usd <N> [--json]
bitrouter cloud budget update <ID> [--name <NAME>] [--window <W>] [--limit-micro-usd <N>] [--json]
bitrouter cloud budget delete <ID> [--json]

bitrouter cloud preset list [--json]
bitrouter cloud preset get <ID> [--json]
bitrouter cloud preset create --name <NAME> [--guardrail <FILE|->] [--budget <FILE|->] [--rate-limit <FILE|->] [--json]
bitrouter cloud preset update <ID> [--name <NAME>] [--guardrail <FILE|->] [--budget <FILE|->] [--rate-limit <FILE|->] [--clear-guardrail] [--clear-budget] [--clear-rate-limit] [--json]
bitrouter cloud preset delete <ID> [--json]
```

Budget `--limit-micro-usd` must be strictly positive (the engine treats `<= 0` as "no policy" and the API refuses it up-front). Preset clauses are independently optional; use `--clear-*` flags to drop a clause from an existing preset.

### `bitrouter cloud byok`

User-level — not workspace-scoped; BYOK provider keys are account-wide. The cloud only stores already-sealed ciphertext — seal against the cloud's current X25519 public key (separate fetch) before calling.

```
bitrouter cloud byok list [--json]
bitrouter cloud byok set    --provider <ID> --ciphertext-b64 <B64> --kek-id <ID> --key-prefix <PREFIX> [--api-base <URL>] [--json]
bitrouter cloud byok delete <PROVIDER> [--json]
```

## Skills

`bitrouter skills …` installs and manages Claude Code skills — directories containing a `SKILL.md` with YAML frontmatter (`name`, `description`). Skills install into the agent skills directory: `~/.claude/skills/<name>/` with `--global`, or `./.claude/skills/<name>/` (project-local) by default.

Sources are auto-detected:

- `owner/repo` — GitHub shorthand (optionally `owner/repo#<branch|tag|sha>`)
- `https://github.com/owner/repo[.git]` — full GitHub URL
- `https://github.com/owner/repo/tree/<ref>/<subdir>` — a skill in a subdirectory
- any other `https://…`, `git://…`, or `git@…` git URL
- `./path`, `../path`, `/abs/path`, `~/path` — a local directory (copied, not cloned)
- a bare `name` — resolved against a namespace's registry hub (`-n/--namespace <NSID>` required; `--registry <URL>`, default `https://api.bitrouter.ai`). The hub is per-namespace: `<registry>/v1/namespaces/<NSID>/skills/hub`.

Git sources are shallow-cloned via the system `git` binary (must be on `PATH`). Plain-HTTP sources are refused (skills are executable content); symlinks in a source tree are skipped, and skill names are validated to prevent path traversal.

### `bitrouter skills add <source>`

```
bitrouter skills add <SOURCE> [--skill <NAME>] [-g|--global] [-y|--yes] [--registry <URL>] [-n|--namespace <NSID>]
```

Clones/copies the source, discovers its `SKILL.md`, and installs it. `--skill <NAME>` selects one skill by frontmatter name when the source exposes several. Installing over an existing skill requires `-y/--yes`. Resolving a bare `name` requires `-n/--namespace <NSID>` (the registry hub is per-namespace).

### `bitrouter skills list` / `remove`

```
bitrouter skills list   [-g|--global]
bitrouter skills remove <NAME> [-g|--global]
```

`list` prints installed skills (name + path); `remove` deletes one by name.

### `bitrouter skills find <query>`

```
bitrouter skills find <QUERY> [--registry <URL>] [-n|--namespace <NSID>]
```

Searches a namespace's registry hub (`-n/--namespace <NSID>` required), matching `query` against name, description, keywords, and tags.

### `bitrouter skills init <name>`

```
bitrouter skills init <NAME> [-o|--output <PATH>]
```

Scaffolds a starter `SKILL.md` (default `./SKILL.md`). Refuses to overwrite an existing file.

### `bitrouter skills update`

```
bitrouter skills update [<NAME>] [-g|--global] [--registry <URL>] [-n|--namespace <NSID>]
```

Re-installs installed skills from a namespace's registry hub to their latest version (`-n/--namespace <NSID>` required; all installed skills, or just `<NAME>`). Skills absent from the registry are skipped; a per-skill failure is reported without aborting the rest.
