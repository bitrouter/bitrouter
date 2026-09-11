# BitRouter CLI Reference

`bro <subcommand> [flags]`

## Output format

Every command prints a single **formatted JSON object** to **stdout** — success or failure — so output is machine-parseable by default (agent-native first). Global flags:

- `-j`, `--json` — force JSON (the default).
- `--human` — render a human-readable view to stdout instead of JSON.
- `--context <name>` — run a supported read action against a named remote
  BitRouter target; `local` explicitly selects normal local behavior.
- `-H` before the subcommand (for example, `bro -H cloud whoami`) — compatibility spelling for `--human`. Under `bro cloud api`, `-H` means `--header`, matching `gh api`.
- `-h`, `--help` — unchanged (`-h` is **not** human output).

All diagnostics — progress, warnings, internal logs, and a human echo of errors — go to **stderr** (colored when stderr is a TTY; honors `NO_COLOR`). So:

```
bro <cmd> 2>/dev/null | jq .
```

always yields one clean JSON value. A failed command emits a uniform error envelope to stdout and exits non-zero:

```json
{ "error": { "kind": "not_found", "message": "…", "context": ["…"], "hint": "…" } }
```

`kind` is a stable taxonomy (`bad_request` / `unauthorized` / `forbidden` / `not_found` / `upstream` / `internal` / …). Under `--human`, the result (success object or error block) is rendered to stdout in the human form and no JSON is printed.

> Non-reporting commands are exempt: `serve` and `mcp serve` are long-running servers; `acp serve` is a stdio JSON-RPC bridge; `run` streams NDJSON by default; `code` and `launch` own the terminal; and `cloud api` streams the remote response body.

Per-provider credential commands are under `bro providers (login|logout)`; BitRouter Cloud sign-in is `bro cloud (login|logout|whoami)`.

## Logging (`RUST_LOG`)

Diagnostics are emitted with `tracing` and filtered by **`RUST_LOG`**, using standard [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) syntax. When `RUST_LOG` is unset the filter defaults to `info`; a malformed value falls back to `info` rather than failing to start.

Most targets are Rust module paths (`bitrouter`, `bitrouter_sdk`, …), so `RUST_LOG=warn,bitrouter=debug` works as you would expect. Three targets are **pinned explicitly** and are *not* module paths:

| Target | What it carries |
| --- | --- |
| `bitrouter::observe::http` | DEBUG diagnostics from the HTTP ingress layer: one line per request, plus one when an inbound `traceparent` arrives but does not parse. **Silent unless OTel is configured** — the ingress layer is only installed when an exporter exists. |
| `bitrouter::observe::cardinality` | WARN when the metric-dimension cardinality limiter recovers from a poisoned lock. |
| `bitrouter::observe::span_attributes` | DEBUG per span attribute a deployment forwarded that the span schema reserves — see below. |

These use `::` separators (not `_`) precisely because they are not module paths: they are stable selectors that survive the code moving between crates. Two consequences for operators:

- **`RUST_LOG=bitrouter_observe=debug` selects nothing at all.** There is no `bitrouter-observe` crate; the exporter lives in `bitrouter-telemetry`, so that is a dead selector rather than a narrower one — and the module-path fallback would be `bitrouter_telemetry::otel::…`, which is exactly what the pins below exist to make irrelevant. The pinned `bitrouter::observe::*` targets below are the stable way to reach this instrumentation: use `RUST_LOG=bitrouter::observe::http=debug` (or a plain `info` default, which includes it).
- **Turn on `bitrouter::observe::span_attributes` when a forwarded attribute does not appear on a span.** A deployment can attach its own attributes to the root `chat` span, but the span schema reserves its own vocabulary: keys under `bitrouter.` or `gen_ai.`, and any key the schema already declares (`$screen_name`, `error.type`, `server.address`, …), are **dropped rather than stamped**, so one deployment cannot redefine what an attribute means for everyone else. The drop is deliberate and per-request, hence DEBUG rather than WARN: `RUST_LOG=info,bitrouter::observe::span_attributes=debug` names each dropped key. The full reserved region is `crates/bitrouter-sdk/span-schema.json`.
- **`RUST_LOG` no longer affects tracing.** This used to be the opposite, and the reversal is worth stating because the old advice is still in circulation: the ingress span was a `tracing` span bridged into OpenTelemetry, so a filter that dropped `bitrouter::observe::http` at INFO also dropped the SERVER span, and every `chat` span exported as an orphan root with no error reported anywhere. A blanket `RUST_LOG=warn` was enough to do it. The ingress span is now an OpenTelemetry span in its own right and never passes through the `tracing` subscriber, so **no filter can suppress it**. Set `RUST_LOG` for the logs you want; traces are unaffected either way.

## Ignored configuration

`Config::plugins` is an unvalidated map and the JSON Schema declares it
`additionalProperties: true`, so a `plugins.<id>` block the binary does not
read is **silently ignored** — a typo like `plugins.bitrouter-guardrail`
(singular) drops the operator's declared block / redact patterns and the
process starts anyway. Two places report it:

- `bro config validate` lists them under `ignored_config`. It does not
  fail validation — an ignored block is a misconfiguration, not a malformed
  config, and this command is CI-gating.
- Every runtime surface logs one WARN per unread id on start: the daemon, and
  `bro acp serve`, `bro run`, and `bro code <agent>`, none of which build the
  daemon's `App` but all of which read the same config. This is the path that
  matters: validation is opt-in, the runtime always runs.

The ids the binary reads are `bitrouter-guardrails`, `bitrouter-policy` and
`bitrouter-telemetry`. A dead sub-key under a live id is reported too, so a
rename that carries an obsolete setting along with it is not silent either.

**Renamed in this release** — the old names are ignored, and the daemon warns
when it sees one set:

| Old | New |
| --- | --- |
| `plugins.bitrouter-observe.*` | `plugins.bitrouter-telemetry.*` |
| `BITROUTER_OBSERVE_CONTENT_CAPTURE` | `BITROUTER_TELEMETRY_CONTENT_CAPTURE` |
| `BITROUTER_OBSERVE_CONTENT_ATTR_MAX_BYTES` | `BITROUTER_TELEMETRY_CONTENT_ATTR_MAX_BYTES` |

`plugins.bitrouter-observe.otlp_endpoint`, the v0 flat shim, is **removed**
rather than carried over: it existed to keep a v0 config building, and v0 never
had a `plugins.bitrouter-telemetry` key for it to live under.

The `bitrouter::observe::*` log targets above, the `io.bitrouter.observe`
instrumentation scope, and the `bitrouter` meter name are **not** renamed and
will not be. They are wire and `RUST_LOG` contract — a rename there is silently
wrong for every dashboard and every existing selector, with no safety net
possible.

## Config resolution

Local router subcommands that load a config accept an optional `-c / --config <path>` flag. When omitted the binary walks this order:

1. `./bitrouter.yaml` in the current directory
2. `$BITROUTER_HOME/bitrouter.yaml` — if the env var is set, the file must exist
3. `~/.bitrouter/bitrouter.yaml` — used if present
4. **Zero-config** — in-memory defaults; auto-enables any provider whose API key is set in the environment

Daemon-control subcommands (`stop`, `reload`, `status`) also accept `--socket <path>` to override the control socket path derived from the config.

## Remote contexts

```console
bro context add workstation \
  --endpoint https://router.example/control/v1 \
  --token-env WORKSTATION_BITROUTER_TOKEN
bro context list
bro context show workstation
bro --context workstation status
bro --context workstation requests
bro --context workstation models --provider openai
bro --context workstation route openai/gpt-5
bro --context workstation providers list
bro --context workstation observe status
bro --context workstation policy status       # active by default
bro --context workstation policy show default --view disk
bro --context workstation agents list
bro --context workstation reload
bro context remove workstation
```

Contexts live in `contexts.toml` under `$BITROUTER_HOME`, or under
`~/.bitrouter` when that variable is unset. They contain the endpoint and token
environment-variable **name**, never the bearer value. Endpoints must use HTTPS;
plain HTTP is accepted only for `localhost`/loopback, including an SSH-forwarded
port. The client performs the `/control/v1/capabilities` handshake before its
first action (and caches it for a long-lived TUI), follows no redirects, and
never falls back to this computer's config, socket, or database after a remote
error. A named context is the complete target: supported remote commands reject
local `--config` and `--socket` flags before opening local configuration,
environment, or metering state.

Remote read actions are `status`, `requests`, `models`, `route`, `providers
list`, `observe status`, `policy status`, `policy show`, and `agents list`, as
well as the operations-only `code` surface. A remote policy command defaults to
`--view active`; local policy reads default to `--view disk`. Code requests the
active policy explicitly and opens named policy detail through a temporary
picker and inspector.

`agents list` is a catalog read only. `agents list --remote`, agent checks and
launches, ACP sessions, native harness UIs, and `code <agent>` stay local. The
only remote mutation is the explicit `reload` action; it reloads server-owned
files and never uploads config or forwards the client's environment. Provider,
key, configuration, policy-publication, and service lifecycle administration
remain local.

Local CLI compatibility adapters retain the established `providers list`
`api_base` field and `observe status` socket, endpoint, and service fields.
Those host details are omitted from remote administration and dashboard
reports, which use the redacted typed read contracts.

---

## Daemon lifecycle

### `bro serve`

Run the HTTP server and control socket **in the foreground**.

```
bro serve [-c <path>]
```

Starts the proxy on the configured listen address (default `127.0.0.1:4356`) and opens a Unix domain control socket. Logs to stdout.

An opt-in remote-control listener exposes typed administration reads and a
separately authorized reload action to a trusted operator through a private
tunnel or TLS reverse proxy:

```yaml
control:
  enabled: true
  listen: 127.0.0.1:4358
  credentials:
    - id: workstation-admin
      token_env: WORKSTATION_ADMIN_CONTROL_TOKEN
      scopes: [control:read, control:reload]
```

`ControlConfig.credentials` is optional. When it is absent or empty, the legacy
`BITROUTER_CONTROL_TOKEN` remains valid with `control:read` only. When explicit
credentials are configured, only their `id`, `token_env`, and `scopes` entries
are accepted; the legacy token is not an additional credential. Each token must
be at least 32 bytes. `control:read` grants the host-wide operational reports;
`control:reload` permits the guarded reload operation and requires the read
scope alongside it.

This listener is disabled by default, accepts loopback addresses only, and is
never affected by inference `server.skip_auth`. Its typed HTTP actions live
under `/control/v1`, and the BitRouter origin MCP service is mounted at
`/mcp-control` with the same bearer and browser-Origin checks. Remote ACP
sessions are not exposed. Changes under `control:` are restart-only because
they govern listener creation and binding.

### `bro start`

Spawn `serve` as a **detached background daemon**.

```
bro start [-c <path>] [--log <path>]
```

Logs default to `bitrouter.log` next to the config file (e.g. `~/.bitrouter/bitrouter.log` when the config resolved to `~/.bitrouter/bitrouter.yaml`). Refuses to start if a daemon is already running.

Waits until the daemon answers on its control socket before reporting `✓ … started` (up to 15s), then prints the listen address and routable-model count — so a follow-up command can rely on the daemon being up. If the daemon crashes during startup, the tail of its log is printed and the command exits non-zero; if it is alive but still not ready after 15s, a note is printed and the command exits 0 (the daemon keeps coming up).

### `bro stop`

```
bro stop [-c <path>] [--socket <path>]
```

### `bro restart`

```
bro restart [-c <path>] [--socket <path>] [--log <path>]
```

Stops the running daemon (waiting up to 30s for in-flight requests to drain), then starts a fresh one.

### `bro reload`

```
bro reload [-c <path>] [--socket <path>]
bro --context <name> reload
```

Hot-reloads the running daemon's config and routing table without dropping connections. Also triggered by `SIGHUP`.

Any provider API keys present in the current environment are forwarded to the daemon so `export OPENAI_API_KEY=…; bro reload` takes effect immediately.

With `--context`, reload submits a guarded operation against the server's own
current configuration. It does not forward client environment variables. The
report includes a request id, server instance, generation, and per-subsystem
result. A running, failed, partially applied, or unknown operation exits
non-zero; use its operation lookup details and the target's state before retrying.

### `bro status`

```
bro status [-c <path>] [--socket <path>]
bro requests [--limit N] [--since RFC3339 --until RFC3339] [--model ID] [--provider ID]
bro requests --human           # the same, as a table
```

Prints pid, listen address, number of routable models, the distinct providers behind them, the control socket path, and the **spend position**. Exits cleanly with "stopped" when no daemon is reachable.

The same report the origin MCP server's `status` tool returns — one shared type, so `bro status --json` and that tool's structured content are the same bytes.

**`spend` — what has gone, and what is left.** Two independent facts, each present only where the deployment can answer it:

| Key | Filled by | Means |
|---|---|---|
| `spend.spent` | any deployment (the local metering database) | Money already gone: `estimated_micro_usd` over `window` (today, since 00:00 UTC), `requests`, and `unpriced` |
| `spend.limit` | a deployment with a cap (today: a metered cloud account's prepaid credit) | Money still available: `balance_micro_usd`, `pending_micro_usd`, `remaining_micro_usd` |

`spent` is an **estimate and a floor**, not a total. It is priced from BitRouter's own registry at settle time, and requests with no charge evidence are excluded rather than summed as zero — summing them would report a floor as a price. `unpriced` counts exactly those, so a non-zero value means the figure understates by an unknown amount; the human view marks it `floor, not a total`. A `spend.limit` is the opposite kind of number: an authoritative ledger the account is settled against. Read `unpriced` before treating the two as comparable.

The read is best-effort and never fails the command: no config, no database file, or an unreadable one gives no `spend` key at all — which is a different answer from `estimated_micro_usd: 0` over `0` requests, meaning "nothing spent today". It also works with **no daemon running**, so a `stopped` report still carries spend: what a past daemon spent is on disk and does not stop being true when it exits.

The figure is **machine-wide**, not per-caller: it rolls up every caller of this daemon, the same scope `requests` reports. Per-session spend is `bro code <agent>`'s cost line.

`bro status --json` gained `spend` additively; every pre-existing key is unchanged.

`bro requests` reports what the router has actually done: newest-first
settled requests — time, model, the provider that **actually** served, tokens
in/out, cost, latency, status — plus daemon state and the window's spend and
trailing-minute rate. It reads the metering store directly, so it also works
with **no daemon running**. `--since` and `--until` are an RFC3339 pair with a
maximum seven-day interval; absent both, the server selects today from UTC
midnight. `--model` and `--provider` filter the same report before the limit.
`status --requests` remains a hidden compatibility spelling.

Like every other report it uses JSON by default and `--human` for the table. Repeat it with `watch -n1 bro requests --human` for a live view.

The spend rollup covers every caller of the daemon, not one session. `bro code <agent>`'s cost line is the per-session figure.

**Spend is reported only where there is evidence.** An unpriced request shows `?`, not `$0.00`; the same honesty rule applies to `code <agent>` session cost.

Each row also carries `episode_id` — the trajectory episode to hand to `bro trajectory inspect`, or `null` when trajectory capture recorded nothing for it (capture is opt-in and off by default, so `null` is the common case). It is the thread from a settled request to its structural record, which is otherwise reachable only by an episode id nothing else hands out.

Portable — there is no terminal-only path left to gate.

> **`--requests` emits JSON by default as of 1.0.0-alpha.28.** It previously printed the table unconditionally, ignoring `--json` — the only `status` path that did. Scripts that parsed the table need `--human`; anything that wanted the data now gets one clean JSON object with a stable `rows[]`.
>
> **Replaces `--watch` (`-w`), removed in 1.0.0-alpha.28.** That flag opened a self-refreshing ratatui view with cursor keys plus `r` (reload) and `e` (`$EDITOR` on `bitrouter.yaml`). Both of those keys ran commands you can still run directly — `bro reload`, and your editor — and the piped form of `--watch` printed what `--requests --human` prints now.

---

## Config

### `bro init` (onboarding wizard)

```bash
bro
bro init --yes --harness codex --after exit
bro init --harness claude --model anthropic/claude-sonnet-4-6
```

Bare `bro` opens first-run onboarding when no default ACP harness is
saved. Credentials alone do not complete setup. The wizard saves `chat.agent`
and optional `chat.model`, then either opens BitRouter's ACP TUI, starts the
daemon, or exits. Subsequent bare invocations immediately open the saved TUI.

Interactive setup lists every active, public provider from the registry in one
alphabetical list. BitRouter Cloud (`bitrouter`) is an ordinary provider row.
The fetched/cached registry is supplemented by the binary's committed snapshot,
so a fresh installation also has a catalog offline. A custom or disabled registry
remains authoritative. Configured credentials are marked; select additional
providers to sign in, then choose **Continue to harness setup** (End jumps there).

Every setup choice uses the same searchable, eight-row scrolling selector:
Up/Down moves the pointer, Enter selects, typing filters by label or provider id,
Backspace edits, Ctrl-U clears, and Home/End or Page Up/Down navigates long lists.
The list fits smaller terminals. Digits are search text, never choice shortcuts.
This includes provider/ACP login methods, the registry-derived ACP harness list,
the finish action and reset confirmation. Esc or Ctrl-C cancels before setup is
saved; credentials from already completed logins remain available.

Configuration resolves from `./bitrouter.yaml`, then
`$BITROUTER_HOME/bitrouter.yaml`, then `~/.bitrouter/bitrouter.yaml`. With no
existing file, onboarding writes to the BitRouter home. `init -c PATH` selects
an explicit destination. Existing configuration values are preserved while
updating chat defaults; `--force` replaces them with the starter configuration.
Writes are atomic. First-run defaults bind `127.0.0.1:4356` with `skip_auth: true`.

`init --yes` saves configuration without interactive credential prompts and
exits by default. The default harness is `codex-acp`; `--harness claude` selects
`claude-acp`. Repeated `--harness` flags use the first as the default. An explicit
`--after launch` opens the ACP TUI even when setup itself was headless.
Without a terminal, bare unconfigured invocation prints setup instructions
and an inert onboarding envelope; it does not silently complete the wizard.

| Flag | Description |
| --- | --- |
| `-c`, `--config PATH` | Configuration destination; otherwise use the resolution chain above. |
| `--yes`, `-y` | Process flags without interactive setup prompts. |
| `--force` | Reset existing configuration to starter defaults before saving. |
| `--reset` | Clear BitRouter credentials before setup; does not remove vendor CLI credentials. |
| `--cloud-login` | Cloud device login; reported-and-skipped headlessly. |
| `--api-key KEY` | Seed a BitRouter Cloud API key. |
| `--provider ID` | Log in to a provider (repeatable); interactive OAuth is skipped headlessly. |
| `--provider-api-key KEY` | Key paired with the provider at the same position. |
| `--use-detected` | Accept detected credentials. |
| `--harness claude\|codex` | Built-in ACP harness; the first selection becomes the default. |
| `--after launch\|serve\|exit` | Open the ACP TUI, start the daemon, or exit. |
| `--model ID` | Persist the default model for future TUI sessions. |

The JSON envelope includes `action`, `providers_configured`,
`providers_skipped_interactive`, `harnesses_installed` (selected built-in ACP
ids, not native CLI installations), `after`, and `snippet`.

Retryable route candidates advance immediately unless an operator opts into an
upstream fallback delay schedule:

```yaml
upstream:
  fallback_backoff_ms: [1000, 2000, 4000, 8000, 16000, 30000]
```

The first value applies before the second candidate, the second before the
third, and the final value repeats if the configured chain is longer. The
schedule applies only after an error the fallback policy classifies as
retryable; it does not retry non-retryable 4xx responses and does not change
route selection. An empty or omitted schedule preserves the existing behavior.

---

## Routing / introspection

### `bro route <model>`

```
bro route gpt-4o [--prompt <text>] [-c <path>] [--socket <path>]
```

Resolves a model name through the routing table and prints the full fallback chain (provider → upstream service id → protocol). Queries the running daemon if reachable — its `route` verb resolves the model exactly as given, since the daemon's policy table runs on real requests rather than on this preview — and otherwise falls back to a local config parse, **policy table included**, so `effective_model` there is what would actually run.

`--prompt` supplies the request text the policy table keys on: it routes by the agent-loop step a request represents, so the model it selects can differ with the prompt. Omit it for a bare model resolution. It is consulted on the config path only; a `live` answer is the same with or without it.

The report is the shared `route` action's, so `bro route --json` is byte-identical to the MCP `route_preview` tool's structured content:

| Field | Meaning |
|---|---|
| `requested_model` | what you asked about |
| `effective_model` | what would actually run — differs when the policy table selects another model |
| `effective_effort` | the reasoning effort policy selected, when it selected one |
| `resolved_via` | `live` \| `config` \| `zero_config` — the same words `bro models` uses |
| `policy_decision` | the static decision behind `effective_model`. Absent on `live`: the daemon's `route` verb does not replay policy, so there is no decision to show and `effective_model` equals `requested_model` there |
| `provider_chain[]` | `provider` / `service_id` / `api_protocol`, preferred hop first. Never the provider's credential |
| `estimated_cost` | the first hop's per-token rate card, including any steeper long-context brackets. Rates, not a total: nothing was sent |

Read-only throughout — nothing is sent upstream.

### `bro models`

```
bro models [-c <path>] [-p <provider-id>]
```

Lists all routable models, each with **every** provider that can serve it — the
fallback chain, in order. Filter to one provider with `--provider`.
Subscription providers are explicit-route-only, so their rows use a pinned
`provider:canonical-model` selector (for example,
`openai-codex:openai/gpt-5.6-sol`). Copying any displayed selector into
`bro route` therefore previews the route without implicitly opting a bare
canonical request into a personal subscription.

Queries the running daemon if reachable and falls back to a local config parse,
the same order `bro route` uses: the live routing table reflects `reload`s
and what the daemon actually resolved at start-up, where a static parse is what
the file says now. The parse is resolved the way the daemon resolves its own —
built-in defaults, then providers whose credential lives in the OAuth store
rather than the config (`claude-code`, `google-ai`) re-activated — so a
subscription-backed provider is listed with no daemon running. `--json` reports
which view answered as `resolved_via: "live" | "config"`, and the human view
annotates a `config` listing.

The config fallback probes each `auto_discover: true` provider's `/models`
endpoint (bounded: 2s connect, 5s per request; failures leave that provider with
no models rather than failing the command). The daemon path does no such probing.

Same report type as the origin MCP server's `list_models` tool, so
`bro models --json` and the tool's structured content are the same bytes.

### `bro providers list`

```
bro providers list [-c <path>] [--socket <path>]
```

Local compatibility output prints each provider's id, model count,
routing-active state, and `api_base`. Named remote contexts and the dashboard
use the redacted accepted catalog, which omits API bases and credentials.
`active` means the accepted routing configuration includes the provider, not
that a connectivity probe succeeded.

---

## MCP upstream diagnostics

### `bro mcp check [server]`

```bash
bro mcp check                 # every configured upstream
bro mcp check my-server       # one configured upstream
```

Performs one `tools/list` round trip per selected server and reports transport,
reachability, latency, negotiated tools capability, and advertised tool names.
This is the canonical diagnostic. The `tools` commands below are hidden
compatibility spellings.

### Hidden compatibility: `bro tools`

### `bro tools list`

```
bro tools list [-c <path>]
```

Connects to every `mcp_servers` entry in the config and lists advertised tools with descriptions.

### `bro tools status`

```
bro tools status [-c <path>]
```

Health-checks each configured MCP server with a `tools/list` round-trip. Prints status, latency, and transport.

### `bro tools discover <server>`

```
bro tools discover my-server [-c <path>]
```

Connects to one MCP server and prints a YAML stub suitable for pasting into the `mcp_servers:` block of `bitrouter.yaml`.

---

## Origin MCP server

`bro mcp serve` runs BitRouter itself as an **origin** MCP server, so an
MCP-capable client (Claude Code, Claude Desktop, Cursor, …) can call BitRouter's
own capabilities as tools. This is the inverse of `bro mcp check` and the
`mcp_servers:` config block, where BitRouter is the MCP *client* proxying
upstream servers.

### `bro mcp serve`

```
bro mcp serve
```

Long-running: its stdout is the JSON-RPC wire, not a result envelope. Stdio is
the canonical transport for hosts, native launchers, and plugin manifests.
Network-capable hosts connect directly to the running daemon's authenticated
Streamable HTTP `/mcp-control` endpoint. Standalone `--transport http` is
retired and returns an error rather than starting a second listener.

**Transport**

| Mode | Wire | Listener |
|---|---|---|
| `stdio` (default) | newline-delimited JSON-RPC over stdin/stdout — what an MCP client launches as a subprocess | — |
| Streamable HTTP | served only by the daemon at `/mcp-control`; the hidden standalone `--transport http` compatibility input returns an error | `control.listen` |

**Backends**

| `--backend` | Routes to | Notes |
|---|---|---|
| `local` (stdio default) | the local BYOK daemon at `--local-url` (default `http://127.0.0.1:4356`) | canonical local origin profile |
| `cloud` | BitRouter Cloud at `--cloud-url` (default `https://api.bitrouter.ai`) | hidden compatibility profile over stdio using `--token` / `BITROUTER_TOKEN`; network hosts should connect directly |
| `skills` | the installed-skills tree under the current directory | stdio only — it serves the launching process's own skill library |

**Tools**

Control and introspection only. There is **no inference tool**: to run a
completion, call the daemon's HTTP API (`/v1/messages`,
`/v1/chat/completions`) — the transport built for it, with streaming, the full
parameter surface, and the metering path. `bro mcp serve` tells you which
models to send there (`list_models`), where they would go (`route_preview`),
and what it has cost (`status`).

| Tool | Wired on | What it answers |
|---|---|---|
| `list_models` | every profile | Every routable model with **all** the providers that can serve it, not just the first. Optional `provider` argument filters, exactly as `bro models --provider` does. Returns the same report type as `bro models`, advertised as the tool's `output_schema`. On stdio + local it reads the daemon's live routing table over the control socket and falls back to a static config parse, so **it answers with no daemon running**; `resolved_via` says which view it is. Other profiles answer with the backend's own `GET /v1/models`, which does need the daemon (or the metered account) up |
| `status` | stdio + local, and any cloud profile | Daemon liveness (pid, listen address, model count, providers, control socket) plus the spend position — `spend.spent` on any deployment, `spend.limit` on a metered one. Returns the same report type as `bro status`, advertised as the tool's `output_schema`. A stopped daemon is `running: false`, not a tool error. Not wired on HTTP + local: only a process on the daemon's own machine can read its control socket |
| `route_preview` | stdio + local | How a model/prompt *would* route — the effective model the policy table selects, the provider chain, the decision behind it, and the first hop's rate card — without sending anything upstream. Returns the same report type as `bro route`, advertised as the tool's `output_schema`. Config is read **per call**, so an edited `bitrouter.yaml` is visible to a long-running server |
| `skills_search` | every **stdio** profile | Every skill on this machine, optionally narrowed by `query`. Returns the same report type as `bro skills list`, advertised as the tool's `output_schema`. Reads the project *and* user-global roots, and marks any skill it found but cannot serve with `valid: false` plus a `problem` |
| `skills_get` | every **stdio** profile | One skill's frontmatter metadata and `SKILL.md` body |

Only wired capabilities register their tools, so the profiles stay disjoint by
construction: an HTTP client never sees `route_preview` or the skills tools
(both read the serving machine's own routing table and skill library, which has
no meaning on a multi-tenant transport).

The skills tools ride the **transport**, not the backend: a stdio server is a
subprocess of the caller whose machine it is, which is the same argument that
makes `--backend skills` stdio-only. So a `bro mcp install`-ed client —
which launches `bro mcp serve` — sees skills too; before, only
`--backend skills` did, and an installed client never saw one.
`--backend skills` survives as the narrow gateway-subprocess profile that
carries *nothing else*.

Every stdio profile also serves SEP-2640's `skills/list` / `skills/get` JSON-RPC
methods plus `resources/list` / `resources/read` over the skill files, for hosts
that consume the extension rather than the tool pair. `skills/list` publishes
only the skills that are actually loadable; `skills_search` and
`bro skills list` show the rest, marked, so an author can see why a skill
on disk is unusable. Each published entry carries a complete `resources`
manifest with a `digest` and a byte `size` per file.

Spend reaches an MCP client as **typed structured content** under `status`'s
`spend`, read from the local metering database — the same ledger
`bro status` and `bro cost` report from, so the surfaces cannot
disagree about what has been spent.

### Hidden compatibility: `bro mcp install`

```
bro mcp install --client claude|cursor [--config PATH]
```

Renders the client config block that launches `bro mcp serve` over stdio.
With `--config`, merges it into that file; without, prints it to stdout.

---

## Hidden compatibility: MCP registry discovery

These commands are hidden from normal help during the compatibility window.
`mcp_servers:` remains the declarative source of truth; this legacy browser does
not write config.

### `bro mcp search <query>`

```
bro mcp search filesystem [--limit N]
```

Searches registry names server-side and prints rows of `name / version / install / description`. The install column classifies support: `remote` (zero-install `streamable-http` entry), `npx` / `uvx` (auto-stub-able, version-pinned stdio package), `manual` (another package type or an entry that is not safe to auto-stub), `-` (no distribution).

### `bro mcp list`

```
bro mcp list [--limit N]
```

Lists registry servers with the same install-support column (default 50 rows).

### `bro mcp add <name>`

```
bro mcp add com.pulsemcp/remote-filesystem
```

Prints a YAML stub to review and paste under `mcp_servers:`. This legacy helper is hidden during the compatibility window; `mcp_servers:` remains the declarative source of truth.

---

## ACP agent management

### `bro agents list`

```
bro agents list [-c <path>]
```

Shows the built-in agent catalog alongside which agents are configured in the loaded config.

### `bro agents inspect`

```bash
bro agents inspect claude
```

Opens a fresh harness-native session, waits briefly for its advertised slash
commands, and reports which source answers each command.

### `bro agents check`

```
bro agents check [agent] [-c <path>]
```

With an agent or friendly alias, preflights that adapter and routing target.
With no agent, spawns each configured adapter and verifies `initialize`.

### `bro agents scaffold <id>`

```
bro agents scaffold claude-code
```

Prints a YAML stub for the named catalog or registry agent. Paste the output
under `agents:` in `bitrouter.yaml`. `agents install` remains a hidden alias.

### `bro agents conformance <id>`

```
bro agents conformance local/claude-acp
```

Runs the `acp_compat_1` ACP-compatibility suite and prints the `conformance:`
block to record under the agent's entry in `registry/runtimes/<runtime>.yaml`.
`<id>` is `<runtime>/<harness>`; `local/` is the default runtime and may be
elided.

Two tiers. **handshake** — the agent answers `initialize` and settles on the
ACP version its registry entry declares. **routability** — the agent's LLM
traffic reaches BitRouter when its routing block is applied, carrying the
gateway credential and the pinned model.

No provider credentials are needed: the agent is launched with its own routing
pointed at an ephemeral loopback gateway that records what arrived. It does
spawn the agent, so the package or binary must be installed. A harness routed
only on the interactive launch path (opencode, pi, hermes, openclaw) reports
routability as `skipped` — its ACP facet launches direct, so there is no routed
ACP traffic to observe. Exits non-zero when a tier fails or when nothing was
verified, and prints no record in either case.

### `bro run` — headless agent

```
bro run <agent> [prompt|-] [--prompt-file PATH] [--load ID|--resume ID]
              [--cwd PATH] [--format ndjson|text|quiet]
              [--approve-all|--approve-reads|--deny-all]
              [--permission-policy JSON|@PATH] [--result-schema JSON|@PATH]
              [--turn-timeout <secs>] [routing flags] [-c <path>]
```

The canonical always-headless agent surface. It opens one ACP session, sends
one prompt from the positional argument, a file, or stdin, and streams NDJSON
by default; text and quiet formats are explicit. `--load` replays a native
session's history and `--resume` continues without replay when advertised.
Every NDJSON event carries `version: 1` and a monotonic `seq`; `json` remains a
temporary format-value alias for `ndjson`.
Permissions, result validation, routing, timeouts, session identity, and exit
codes are the same implementation used by the compatibility `acp prompt` and
`spawn <agent> -p` forms.

### `bro acp`

```
bro acp serve <agent> [-c <path>]
```

Exposes an ACP-compatible adapter over protocol-pure stdio until the ACP client
disconnects; one controller connection can carry multiple
harness-native sessions. Hidden `acp prompt` and `spawn` spellings remain only
for migration. BitRouter keeps no session records.

### `bro code` — coding conversation

```bash
bro code [-c <path>]
bro code <agent> [--load <id>|--resume <id>] [--model <id>] [--turn-timeout <secs>] [--direct] [--base-url <url>] [--no-start] [-c <path>]
bro code --socket <path>
bro --context <name> code
```

Bare local `code` opens an empty conversation and a searchable **Choose agent**
picker. Explicit `code <agent>` connects directly. Dismissing a picker restores
the draft and reading position. A draft written before connecting remains a
draft after agent selection and needs an explicit send.

The conversation, multiline composer, and **agent, route, activity, and
attributed session cost** stay visible. Ctrl-P opens commands and temporary
inspectors; there are no permanent page tabs. Reports run on demand through the
same typed actions as the CLI. Host request history is labelled by its scope;
it is not presented as the current session's traffic or cost.

Named remote contexts and explicit `--socket` operation targets open an
operations-only status inspector. Ctrl-P exposes status, models, host requests,
route preview, providers, telemetry, active policy, agent catalog, reload state,
and an explicit **Reload now** action. Remote requests use authenticated HTTP
and never fall back to local data. These targets have no ACP composer, agent
launcher, or session route mutation. Closing their root inspector exits.

**Keys**

| Key | Effect |
| --- | --- |
| `Enter` | Send at idle; while working, preserve the draft and explain queueing |
| `Shift-Enter` / `Alt-Enter` / `Ctrl-J` | Insert a newline (`Ctrl-J` is the fallback) |
| `Tab` | Accept open completion; otherwise queue a follow-up during work |
| `Ctrl-P` / leading `/` | Search the command palette / slash completions |
| Arrows, Home/End | Edit at the grapheme cursor; Up/Down at draft boundaries visits process-local history |
| `Ctrl-G` | Open `$VISUAL` or `$EDITOR` at idle with no pending permission |
| `PageUp` / `PageDown` | Read transcript history without incoming updates moving the reading position |
| `F2` | Explicitly focus the oldest pending permission |
| Permission digits / arrows, then `Enter` | Highlight an offered choice, then explicitly confirm it |
| `Esc` | Close a temporary surface; in the working composer, request cancellation |
| `Ctrl-C` | Close a picker/inspector; cancel a working turn; clear an idle draft; exit if idle and empty |
| `Ctrl-D` | Exit only from an idle, empty composer; preserve nonempty drafts |
| `Ctrl-L` | Redraw without clearing the conversation |

Bracketed paste preserves line breaks and does not submit. Queued prompts are
local to this UI process, dispatch serially only after normal `end_turn`, and
pause after refusal, limits, errors, cancellation, or disconnect. Resolve queued
work before switching agents or sessions. Queueing does not claim native
mid-turn steering support.

**New session** in Ctrl-P starts a fresh transcript with the same agent and
retains the launch's `--model`, routing options and `--turn-timeout`. It closes
the previous ACP connection before opening the replacement; it does not load
or replay earlier history. Selecting the same agent also retains these launch
settings. Selecting a different agent starts with that agent's default launch
options. After registering an evolution trial, use **New session** to begin a
new native session eligible for enrollment; the old session is not reassigned.

Permissions show the agent's actual labels and never preselect approval.
Dismissing a permission uses its offered reject-once option, otherwise the ACP
cancelled outcome. Cancelling the turn resolves outstanding requests as
`Cancelled`, retains the original prompt until settlement or bounded teardown,
and does not imply effects were rolled back.

Agent settings come from initial ACP metadata and later updates. They are
separate from session `/route` and `/route reset` controls. Route controls need
advertised session-scoped extension methods; `/preview` only inspects configured
resolution. Cost is cumulative native-session usage, labelled router-attributed
or agent-reported; missing or unknown provenance is unreported.

Local coding sessions also offer **`/evolution`** in the command palette. It
opens the local router's evolution status, mode and judge-model controls,
session checkpoints, manual rubric review, candidate creation and policy-block evidence.
The serving daemon must be running, and checkpoint review requires a recorded
ACP session. These controls are not offered on remote or explicit-socket
operations-only targets. Selecting a judge preserves the current mode; select
automatic mode separately to enable background judging.

The status inspector shows judge overhead across recorded sessions and the
retry subset already included in that amount. Each evaluation job shows its
cost and any incomplete attempts. These are estimates from current metering
evidence, refreshed after reconciliation. A missing record, unknown price,
interrupted attempt or incomplete fallback cost keeps the total unknown; a
known subtotal is still shown. Failed, superseded and older checkpoint jobs
retain their spend. Judge costs are separate from coding spend and do not
establish net routing savings. CLI `acp evolution status` also includes per-session
totals and per-request evidence status in `judge_costs`.
Deleting recorded content removes cached judge text; retained cost metadata
continues to account for those attempts. Status identifies this subset when
the original job details are unavailable. Legacy deleted jobs without cost
reservations cannot be reconstructed.

Under **Session checkpoints**, **Evaluate the current recorded prefix** freezes
the observed prefix at idle and opens an unsaved manual draft. Existing
checkpoints can be reopened, including historical prefixes. For each rubric,
select a score or explicit uncertainty/non-applicability, cite original evidence
and explain the applicability and score. The rubric menu shows its weight,
applicability instructions and scoring anchors above the choices, wrapped to
the terminal width. The picker accepts custom scores
between 0 and 1. Read the full evidence before choosing citations; a positive
verification score requires a recorded tool observation. Add overall feedback,
then choose **Review and submit**. Close the preview with Escape to reach
**Save this evaluation** or return to editing. No model is invoked by this flow.

**Evaluation history** shows every stored revision for the opened checkpoint:
manual/automatic source, evaluator, scores, original explanations and citations,
submission selection and replacement/retraction metadata. Reading a revision
does not change the draft. The draft starts from this checkpoint's selected
revision when it is current; an older checkpoint prefers its latest manual
revision, then its latest automatic revision. A retraction leaves the draft
unscored, and unsupported rubric formats remain visible in history without
silently converting their scores. Historical corrections are retained without
replacing the selected evaluation of a newer prefix. History reflects the state
when the review was opened; reopen it to see subsequently stored revisions.

The quality range reflects missing evidence, not statistical confidence.
Unknown values remain unknown. Saving uses the displayed checkpoint and
expected assessment revision: continued content is not silently evaluated,
and a stale draft cannot replace a newer assessment. Repeat delivery of an
unchanged submission is idempotent. Errors retain the draft and source picker;
reading evidence returns to the review. Drafts remain only in the current TUI
process and are cleared when discarded or when another session opens.

Under **Create a candidate experiment**, select the current preset/virtual route
and its candidate route. Add multiple related changes to one block, choose an
evaluation source, and supply the trial reason and relationship to other blocks.
The connected agent defines the source scope. **Review experiment** validates
the live routes and displays their model/fallback chains, prompt-default presence,
quality gates and trial limits. Escape from the preview returns to **Register
this experiment**, **Refresh preview** and **Edit candidate**.

Registration preserves evolution mode. While enabled, future recorded sessions
can enter the trial; the current session is not reassigned. The chosen evaluator
defines which numeric scores are comparable. A changed judge, control state or
route can require a refreshed preview. An unchanged registration retry returns
the existing experiment without resetting its evidence. Errors preserve the
draft; manual review and candidate drafts are mutually exclusive. Candidate
drafts can be resumed after a coding-transport disconnect, and are cleared on
discard or opening another session. They are not persisted across TUI restarts.

The block inspector displays effective evidence and offers reconciliation using
the same live-route and quality gates as the background worker. It shows usable
session-group counts separately for quality, cost and duration, plus the reviewed
experiment's configured minimum per arm. Reaching that minimum does not by itself
permit adoption. Related forks share a group; repeated assessments and prior
strength do not add observed groups. Older daemons may omit the configured
minimum; the UI does not substitute a guessed default. Its publication
history includes allocation, adoption, automatic withdrawal and operator reasons.
**Restore supported baseline** lets you enter a reason, inspect the target and
confirm withdrawal of the current experiment. It also works while evolution is
Off. Reason and evaluation fields accept pasted text; Enter confirms the input.
A stale experiment or revision must be reviewed again; a retry of the same
confirmed request returns its recorded result without withdrawing a newer trial.
Serving checks baseline support and live dependencies on subsequent requests;
changed route dependencies use configured routing. Already dispatched calls and
session overrides retain their existing behavior. The CLI
`acp evolution register` remains available for complete JSON definitions,
including fingerprints, explicit dependencies and custom experiment parameters.
Initial TUI blocks use default TS parameters. **Start the next experiment**
revises an existing block while retaining its matchers, settings and prior
evidence; the draft inherits supported baselines and lets you edit candidate
routes. **Experiment history** inspects archived versions. Review and
reconciliation stay bound to the selected version.

**Open session** uses native listing when advertised and native-ID entry for
load/resume-only agents. `--load` replays native history; `--resume` continues
without replay and labels that distinction. BitRouter keeps no durable session
catalog. `tui` and `chat` remain hidden compatibility aliases; interactive paths
share this loop and piped compatibility output remains plain text.

### ACP workers, local CLI discovery, and native launch

The maintained adapters are pinned to `@agentclientprotocol/codex-acp@1.10.0`
and `@agentclientprotocol/claude-agent-acp@0.75.1`. Node.js 22+ and `npx` are
required; npm obtains the pinned adapter on first use. No `agents:` entry is
needed for either built-in id.

Codex and Claude subscription login metadata and model defaults also ship in
the binary. With the default public registry, missing provider/model entries
are filled from this snapshot; published metadata takes precedence. A custom
registry URL or `registry.enabled: false` opts out. A stored subscription login
auto-enables its provider, so no manual `providers:` entry is required.

When `openai-codex` is active, Codex ACP can use its native default model
without `chat.model` or `--model`. Requests marked by the maintained adapter
map declared native model names to the Codex subscription at gateway ingress;
the CLI retains its native model metadata and picker. Generic API requests do
not opt into subscriptions this way. Explicit provider/canonical model names,
presets and user-defined virtual models are preserved. Reloading the daemon
updates the native-model mapping along with the active provider catalog.

At ACP session startup, BitRouter probes local CLIs with `--version` (two-second
limit). Codex >=0.153.3 is passed to its adapter via `CODEX_PATH`; Claude Code
>=2.1.257 is passed via `CLAUDE_CODE_EXECUTABLE`. Missing, old, failing, or
unresponsive CLIs leave the adapter's bundled runtime in use. Explicit env and
agent-transport overrides win. Custom adapter versions are not modified. The
ACP adapter remains the protocol peer; the native launch commands below are a
separate compatibility facet.

```
bro launch <agent> [--model <id>] [-c <path>] [--base-url <url>] [--no-install] [--no-start] [--check] -- <agent args…>
bro claude [options] -- <claude args…>
bro codex [options] -- <codex args…>
```

Launches a coding-agent harness as an **interactive native-TUI** child process with its gateway base URL pointed at BitRouter, so the agent's traffic routes through the router **without touching the agent's own config files**. This is the native-harness compatibility surface — the human drives the harness's own TUI; for a headless prompt use `bro run`.

Before handing over, `launch` prints one line stating what the harness actually got — whether it is routed, and whether the tools/skills gateways reached it. That ceiling is the harness's, not BitRouter's: `pi` exposes no MCP mechanism to inject into.

```
launch: claude · routed via bitrouter (http://127.0.0.1:4356) · tools ✓ skills ✓
launch: pi · routed via bitrouter (…) · tools ✗ skills ✗ (pi has no MCP mechanism)
```

The positional agent takes any catalog harness with an interactive binary. `claude` and `codex` also have top-level shortcuts; `claude-code` aliases `claude`. The old `-a/--agent` spelling remains hidden during migration. Each harness is routed by its catalog mechanism:

| Harness | How it reaches BitRouter |
| --- | --- |
| `claude` | child env (`ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_MODEL` for `--model`) |
| `codex` | one-shot `-c` overrides for a `bitrouter` provider (`base_url = <target>/v1`, `wire_api = "responses"`) |
| `opencode` | synthesized `OPENCODE_CONFIG` JSON declaring a `bitrouter` openai-compatible provider |
| `pi` | synthesized `PI_CODING_AGENT_DIR` with a `models.json`, selected by `--provider bro --model …` |
| `hermes` | synthesized `HERMES_HOME` with a `config.yaml` (loopback `custom` provider + `CUSTOM_API_KEY`) |
| `openclaw` | synthesized `OPENCLAW_STATE_DIR` + `OPENCLAW_CONFIG_PATH` profile (run as `tui --local`) |
| `grok`, `agy` | **not routed** — own-auth subscription clients (see below) |


The synthesized files are throwaway, written under the working tree's self-ignoring `.bitrouter/launch/`; the user's own `~/.config` is never touched. Their model lists come from the daemon's `/v1/models` (best-effort — an unreachable daemon just yields an empty list and the harness keeps its own defaults).

**Gateway MCP servers.** `launch` also injects BitRouter's two MCP-shaped gateways into the harness: `bitrouter_tools` (the daemon's aggregate endpoint at `mcp.aggregate.route`, fanning out to every configured `mcp_servers` upstream — omitted when `mcp.aggregate.enabled: false`) and `bitrouter_skills` (this binary as `mcp serve --backend skills`, over the installed-skills root). Injection reaches the harnesses that have a mechanism for it — `claude` (`--mcp-config`), `codex` (`-c mcp_servers.*`), and `opencode` and `hermes` (their synthesized config files). `pi`, `openclaw`, `grok`, and `agy` expose no injectable MCP surface and launch without the gateways.

`--model <id>` pins the harness's model through whatever mechanism it has. Following `cargo run`'s convention, everything after `--` is forwarded verbatim, e.g. `bro launch claude -- -p "summarize" --dangerously-skip-permissions`.

**`grok` and `agy` are own-auth harnesses.** They launch with their own subscription auth and are **never redirected** — the startup line says `own-auth · not routed · not metered`, and `--check` reports it as a `routing` warning. They also remain **providers**: subscription clients whose sessions the daemon borrows to serve *other* requests (`supergrok` / `google-ai`), which is a separate stack and unaffected.

The agent authenticates to BitRouter with `BITROUTER_API_KEY` when set; otherwise a local placeholder is used (fine under the `skip_auth` default written by `bro init`). A missing `claude` / `codex` binary is offered for install via its official native installer (`--no-install`, or a non-TTY stdin, declines); the other harnesses have no bundled installer and error with a pointer to their upstream project.

When the target is the local daemon (a derived base URL on a loopback/wildcard bind) and none is running, `launch` **auto-starts it** — printing a hint, launching a detached `serve`, and waiting for readiness before handing off to the agent. Pass `--no-start` to skip this (a reachability warning is printed instead). An explicit `--base-url` or a non-local bind is never auto-started — BitRouter can't start someone else's daemon — and only gets a warning if it looks unreachable.

After the wrapped agent exits, `launch` prints a one-line session spend summary to stderr (spend during the run + today's total, from the local metering database). Silent when nothing was recorded in the window — e.g. when the run targeted Cloud.

`bro spawn --agent <claude|codex>` is a **deprecated alias** for `launch` (prints a migration note); it will be removed after one or two alpha releases.

### Hidden `spawn` compatibility

The old `spawn` umbrella remains parseable for one migration window but is
absent from normal help. Its modes call the canonical implementations:
`spawn <agent> -p TEXT` maps to `run`, `spawn <agent> --serve` maps to
`acp serve`, and `spawn <agent> --check` maps to `agents check`. New scripts
and integrations must use the canonical commands above; protocol-serving
compatibility forms keep stdout reserved for ACP frames.

### `bro policy`

```text
bro policy init NAME --preset PRESET --economy MODEL [--economy-effort LEVEL] \
  [--strong MODEL] [--strong-effort LEVEL]
bro policy check|status|show [--config PATH]
bro policy compile --output FILE [--eval-snapshot SHA256] [--snapshot-time UNIX_MS]
bro policy diff ACTIVE CANDIDATE
bro policy publish CANDIDATE [--config PATH] [--socket PATH]
bro policy verify --evidence [--config PATH]
bro policy evolve [--config PATH] [--apply | --output FILE]
bro policy reload [--config PATH] [--socket PATH]
bro policy rollback DIGEST [--config PATH] [--socket PATH]
```

The BitRouter process, not the policy lock, owns adaptive behavior:

```yaml
policy:
  path: ./policy-lock.yaml
  mode: frozen # or adaptive
```

`policy init` creates the named policy in `adaptive` mode so an explicit
`optimize run` can publish its controller decision. Live routes still use only
the signed lock; Eval rows never change request routing on their own. Operators
can set `mode: frozen` to prohibit low-level or direct publication while
continuing to record observations and evaluator results. Invoking `optimize
run` is explicit authorization to activate adaptive mode and autonomously
publish its successor when the controller decides to do so. Dry-run compilation
and candidate export remain available. The mode controls write authority, not
request-time learning.

`policy publish` promotes the exact compiled v3 candidate after validating its
parent digest, certificates, and current config. A stale candidate or frozen
process leaves the active bytes unchanged. `policy evolve --apply` remains the
legacy migration shortcut; use `compile` + `publish` whenever an eval snapshot
is part of the candidate lineage.

The lock contains deterministic routes, tiers, and learning thresholds, but no activation or freeze switch. Older `policy.writeback: locked|evolve` input remains readable as `frozen|adaptive`; newly written configuration uses `policy.mode`. The old `policy lock`, `policy unlock`, and `policy evolve --freeze` surfaces have been removed.

### `bro optimize`

```text
bro optimize run [--policy auto] [--candidate-tier TIER] \
  [--exploration-ppm 100000] [--minimum-tasks 3] [--maximum-tasks 20] \
  [--minimum-pass-rate-ppm 900000] \
  [--evaluator-config-digest sha256:...] \
  [--config bitrouter.yaml] [--socket PATH]
bro optimize status [--policy auto] [--config bitrouter.yaml]
```

Optimization is driven by history from normal use, not by a bundled workflow
runner. Initialize a policy, run a coding agent or Terminal Bench normally,
submit externally evaluated results, and advance the controller one step:

```bash
bro policy init auto --preset auto --economy provider:model
# run the coding agent or Terminal Bench normally through bitrouter/auto
bro eval result submit result.json --config bitrouter.yaml
bro optimize run --policy auto --config bitrouter.yaml
bro optimize status --policy auto --config bitrouter.yaml
```

Repeat normal traced work, external Eval submission, and `optimize run` until
that command reports the controller decision `converged`. Use `optimize status`
to observe the signed policy state without changing files or the database: it
reports `exploring` while an experiment is active and `idle` otherwise, but
does not infer convergence from Eval history. Calling `optimize run` grants
autonomous authority for exactly one deterministic controller step: it may
publish `explore`, `promote`, or `retreat`, or leave the lock unchanged for
`hold` or `converged`. There is no separate review or publish approval.
Publication uses the current policy digest as a compare-and-swap parent and
reloads a reachable daemon; a stale parent or failed reload leaves or restores
the prior active state. When `--candidate-tier` is omitted, the controller uses
the signed policy's `adequacy.explore_tier`; pass `--candidate-tier TIER` only
to override it for that step.

The first run can cold-start signed exploration from champion-only history.
That history ranks opportunities by request frequency and cost contribution,
but cannot prove an unexecuted challenger is better and therefore cannot
promote one directly. Later runs use complete `task` or `episode` cohorts for
quality and cost. Request subjects help rank the next opportunity but never
enter the gate. Promotion requires the configured quality/pass gate, no hard
violation, and a lower mean complete-unit cost for the challenger. Complete
cost prefers `trajectory.cost.usd_micros` and accepts evaluator-authored
`cost.usd_micros` in micro-USD; per-request price does not gate promotion.

During exploration, router-authored decision evidence contains an optional
signed `experiment` reference with the experiment id, `control` or
`challenger` arm, `task` or `episode` assignment unit, assignment-id digest,
and challenger propensity. Evaluators must copy that object verbatim and must
never invent or edit it. Optimizer cohort membership comes from this router
evidence, not the evaluator-owned `cohort` string.

Router-authored decisions may also contain `route_measurement`. This versioned
object records every tier/model/effort target declared by the same immutable
policy snapshot, the semantic action chosen before tool, progress, or
continuation guards, and its logging probability in integer ppm. The ordinary
`selected_*` fields remain the effective post-guard route. Deterministic routes
use one million ppm; assigned experiments use the signed arm probabilities. If
an experiment lacks a stable task or episode identity, the router records a
deterministic champion action instead of inventing randomized evidence.

The generic Eval Exchange and low-level `policy compile`, `policy diff`,
`policy publish`, `policy rollback`, and `policy verify` commands remain
available for independent evaluation, migration, audit, and operator-managed
policy workflows. They are not extra approval stages for `optimize run`.

### `bro trajectory`

Durable trajectory progress control is an explicit local opt-in:

```yaml
trajectory:
  enabled: true
  retention_days: 30
  outbox_batch_size: 100
```

`enabled` defaults to `false`; `retention_days` defaults to `30` and must be
positive; `outbox_batch_size` defaults to `100` and must be between 1 and 1000.
A signed policy lock containing any `progress_guard` is rejected unless
trajectory capture is enabled. Every trajectory setting is restart-only:
changing `enabled`, `retention_days`, or `outbox_batch_size` during reload is
rejected while the last-known-good runtime remains active. Restart the daemon
to apply any trajectory setting change.

Progress-guard clauses have two timing models. `max_recovery_count` is an edge
trigger: it compares the prospective cumulative recovery count only when the
current request enters `recovery` from another projection. Consecutive
`recovery` projections remain protected but do not count or activate again; the
configured hold, not the cumulative counter by itself, determines how long the
escalation persists. The episode request, elapsed-time, and known-cost
thresholds are monotonic once reached.
Every genuine trigger activates hold. If the current candidate is any declared
protected tier, it is preserved exactly; otherwise the escalation tier is
selected. An active hold uses the same selection rule without resetting its
duration. Unknown cost remains unknown and cannot satisfy a cost threshold.

The feature is source- and task-neutral. It stores event structure, bounded
categorical routing facts, exact counters, and keyed/content digests. It does
not store API keys, bearer credentials, prompts, system instructions, tool
arguments, file bodies, or provider-private metadata. Operational Eval records
are redacted digest/count evidence and an `inconclusive` verdict; they are not a
quality score and do not infer task identity or capability from private data.

```text
bro trajectory inspect EPISODE_ID
bro trajectory replay EPISODE_ID
bro trajectory prune --before RFC3339 [--dry-run]

bro trajectory --config PATH inspect EPISODE_ID
bro trajectory --config PATH replay EPISODE_ID
bro trajectory --config PATH prune --before RFC3339 [--dry-run]
```

`--config PATH` is optional and may appear before or after the trajectory leaf
command. When omitted, these commands use the standard config resolution chain.
They always open that selected source's local database and do not accept an
owner argument. A relative SQLite URL is anchored to the selected config home:
the config file's directory, or the implicit BitRouter home (normally
`~/.bitrouter`) for zero-config. It never depends on or changes the caller's
working directory; absolute/memory SQLite and server database URLs retain their
existing meaning.

`inspect` first resolves the globally unique episode, then performs every read
inside its owner scope. It reports correlation source, history completeness,
current structural health, typed route clauses, and event digests. Persisted
trajectory request IDs and Responses native-parent IDs are installation-keyed,
owner-bound opaque identities; external request IDs on the wire, upstream, and
in metering retain their existing semantics. `replay` validates a stable
episode snapshot and compares the newest persisted route checkpoint digest with
a fresh replay. Corrupt histories expose only the first intrinsically invalid
event id/sequence and a stable reason code; a pending route awaiting its guard
is not itself corruption. Concurrent appends are retried and are never reported
as corruption. Use global `--json` or `--human` for either view.

`prune --dry-run` returns exact eligible counts without mutation. A real prune
removes delivered outbox rows older than the exclusive cutoff, then terminal
episode history whose last capture is older than the cutoff. Every request in
an episode must be settled or failed, and any associated pending outbox row
preserves the entire episode. Deletes are bounded by `outbox_batch_size`,
owner-scoped, identity-checked, and transactional. The daemon applies the same
rules at startup using `retention_days`; it never fabricates an
`episode_closed` event. If a client later cites a native parent that retention
already removed, BitRouter starts a new `unresolved` / `incomplete` episode
rather than pretending the lost prefix is complete.

For recovery, back up the configured database, run `prune --dry-run`, inspect
important episodes, and run `replay` before destructive pruning. A replay
contention error means the episode kept changing during the bounded read; retry
after traffic quiets. Stable corruption reason codes mean the durable history
needs operator investigation or restoration from backup.

Progress metrics are descriptive: request/settlement counts, elapsed time,
projection/tier/unprotected streaks, recovery/hold counters, and optional
authoritative token/cost totals. Missing metering stays absent rather than
becoming zero. `history_complete=false` means the visible prefix is not proven
complete and guards follow their configured incomplete-history behavior.

### `bro eval`

```text
bro eval subject put FILE [--config PATH]
bro eval subject get EVAL_ID [--config PATH]
bro eval subject list [--config PATH]
bro eval result submit FILE [--config PATH]
bro eval snapshot freeze [--at RFC3339] [--config PATH]
bro eval snapshot get SHA256 [--config PATH]
bro eval status [--config PATH]
```

Generic eval sits outside the inference hot path. Routed requests create
redacted subjects automatically. A task-native runner, human, enterprise
system, or agentic evaluator submits the same immutable `EvaluationResult` via
CLI or authenticated REST. BitRouter validates evaluator authority and metric
scope, retains rejected/disputed outcomes, and admits trusted results into a
content-addressed snapshot. It does not bundle or execute a universal judge.
The local CLI operates in the `local` ownership scope. Authenticated REST
operations are isolated to the virtual key's owning user, including list/get,
result submission, status, and snapshot access.

A snapshot commits the exact subject and result digests, is bound to its owner
scope, and excludes held-out, rejected, and disputed results. For episodes with
multiple route decisions, `decision_credit.metric_ids` determines which
decision receives verdict, cost, latency, or violation evidence; omitted credit
is implicit only for a single-decision subject.

The daemon exposes the same library operations at
`GET/POST /v1/evals/subjects`, `GET /v1/evals/subjects/{eval_id}`,
`POST /v1/evals/results`, `POST /v1/evals/snapshots`,
`GET /v1/evals/snapshots/{evidence_root}`, and `GET /v1/evals/status`.
These endpoints mutate evidence only; no evaluator can edit or publish a lock.

### `bro key sign`

```
bro key sign --user <id> [--db <url>] [--policy <policy-id>]
```

Mints a scoped `brvk_` virtual key for a user. The plaintext secret is printed once — only its SHA-256 hash is stored.

| Flag | Default | Description |
| --- | --- | --- |
| `--user` | *(required)* | Owning user id |
| `--db` | `sqlite://./bitrouter.db` | Database URL — `sqlite://`, `postgres://`, or `mysql://` |
| `--policy` | *(none)* | Policy id to bind to the key |

### `bro providers login <provider>`

```
bro providers login claude-code     # Claude Pro/Max subscription via Claude Code
bro providers login openai-codex    # ChatGPT subscription via Codex
bro providers login github-copilot  # GitHub device-code flow
bro providers login openai --api-key sk-…        # BYOK, non-interactive
printf %s "$KEY" | bro providers login anthropic --key-stdin
```

Runs the provider's OAuth flow (PKCE in a browser or device-code, depending on provider) and stores the token in `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. The slot is keyed by `(provider_id, label)` — pass `--label <name>` (defaults to `default`) to keep multiple accounts of the same provider side by side. Other providers fall back to a pasted API key.

For a provider that accepts a pasted key, `--api-key <KEY>` (or `--key-stdin`, which reads one line from stdin) seeds it non-interactively — skipping the method menu and the paste prompt. Both conflict with the OAuth-only `--import-existing` / `--no-browser`, and error if the provider has no API-key method. For the built-in `bitrouter` provider the key seeds the cloud credential, exactly as `cloud login --api-key` does.

For `claude-code`, the login menu defaults to the live Claude Code session. For `openai-codex`, the default is **"Import an existing session from the vendor CLI"** — BitRouter reads the credential Codex already stored in `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) first, then the macOS Keychain, and adopts it with no fresh browser sign-in. The imported token refreshes automatically like any other; choose the browser subscription flow when no local Codex session exists.

For cloud sign-in (signing into your BitRouter Cloud account, not an upstream LLM provider), see [`bro cloud login`](#bitrouter-cloud-login--logout--whoami) below.

### `bro providers logout <provider>`

```
bro providers logout github-copilot
```

Removes every stored credential for the provider (subscription OAuth tokens and pasted API keys alike).

### `bro cloud login` / `logout` / `whoami`

Cloud sign-in, distinct from the per-provider `bro providers login` flow above. Interactive login uses the RFC 8628 OAuth Device Authorization Grant. For CI and other non-interactive environments, pass an existing BitRouter API key with `--api-key`. Both forms persist to the same credential file and are reused by `cloud api`, management commands, the built-in `bitrouter` provider, and account-attributed telemetry.

OAuth browser approval asks which workspace to bind; the resulting credential is **namespace-baked** (workspace-baked). To switch workspaces, re-run `bro cloud login`. OAuth credentials auto-refresh on use. API-key login performs no network request and management commands use the server's `me` namespace alias.

```
bro cloud login [--oauth-as <URL>] [--client-id <ID>] [--scope <SCOPE>]
bro cloud login --api-key <BRK_API_KEY> [--oauth-as <URL>]
bro cloud logout [--oauth-as <URL>] [--client-id <ID>]
bro cloud whoami
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
bro workflow-state classifier-bakeoff --fixtures <DIR> [--submission <JSON>] --output <JSON>
bro workflow-state metering-usage --database-url <URL> --output <JSONL> [--since <RFC3339>] [--until <RFC3339>] [--impute-price <SPEC> ...]
bro workflow-state reconcile-metering --database-url <URL> [--api-base <URL>] [--api-key-env <NAME>] [--credentials-file <PATH>] --request-id <ID> ... [--price <SPEC> ...] [--max-attempts <N>] [--poll-interval-ms <MS>]
bro workflow-state reliability-report --database-url <URL> --config <PATH> --output <JSON>
bro workflow-state policy-oracle --traces <JSONL> --cloud-usage <JSONL> --policy-lock <YAML> --policy <NAME> --effective-cost-factor <0..1> --target-savings <0..1> ... --output <JSON>
bro workflow-state bundle --run-label <LABEL> --traces <JSONL> --cloud-usage <JSONL> [--outcomes <JSONL>] [--policy-decisions <JSONL>] --output-dir <DIR>
bro workflow-state apply-reward-feedback --database-url <URL> --traces <JSONL> --cloud-usage <JSONL> --outcomes <JSONL> --policy-decisions <JSONL>
```

`classifier-bakeoff` is a research-only, read-only route-context evaluation.
With no `--submission`, it records the compiled deterministic scorecard as an
uncalibrated baseline; its heuristic margin is never reported as probability.
An external submission must contain exactly one canonically ordered prediction
for every frozen fixture and bind the dataset, input projection, model artifact,
features, training split, and (when applicable) calibration split. Task family,
next-step role, progress, and shadow risk use separate heads. OOD and abstention
are explicit, and the shadow risk head is evidence only: deterministic rules and
signed policy remain authoritative.

The report contains per-head exact counts and macro-F1, per-slice coverage and
accepted error risk, fixed-point Brier/ECE for calibrated candidates, OOD
confusion counts, resource measurements when supplied, and a versioned
`classification_surrogate_loss`. This is a fixed label-error penalty, not
policy replay or measured routing cost/quality loss. Report and artifact schema
v2 replace the former `decision_weighted_loss` field, correct ECE to the
`0..1_000_000` ppm range, and emit `accepted_error_risk_ppm: null` when a slice
has no accepted predictions. Regenerate v1 reports from the original fixtures
and submissions; their ECE values cannot be repaired from the reported scalar.
Submission and manifest versions are unchanged.

The checked-in fixtures are an evaluation contract and
must not be used as both training and evaluation data; the command rejects
matching split commitments. Synthetic unit-test predictions are test vectors,
not classifier research results or production promotion evidence.

`reconcile-metering` reads the API-key environment named by `--api-key-env`
(default `BITROUTER_API_KEY`) first; a non-empty value takes precedence over
the optional owner-only BitRouter Cloud credential file. That file must contain
a static API key: OAuth credentials are rejected and are never refreshed for
settlement. Price specs use
`provider:model=uncached,cache_read,cache_write,output` in micro-USD per token.
Repeat the same provider/model pair when a gateway may have applied one of
several frozen schedules. A computed receipt is accepted only when exactly one
distinct candidate reconstructs its final micro-USD charge; no match or an
ambiguous rounding collision remains `unknown`.

`policy-oracle` performs an immutable cost-only replay of a candidate lock over
baseline traces and exact request settlement. `--effective-cost-factor` is the
candidate-to-baseline cost ratio after expected token, retry, and turn
inflation. The report includes cost-weighted route coverage, projected savings,
ranked eligible requests, the highest-cost routes still left on the default
tier, and the minimum covered requests needed for each repeated
`--target-savings`. It is an upper-bound prioritization report, not a quality
claim or a claim that the live trajectory will remain unchanged.

`bundle` is fail-closed: every non-empty trace set needs an exact request-ID
usage join and computed auditable charge; supplied policy decisions and outcomes
must each cover that same request-ID set exactly once. Omit `--outcomes` when
the terminal evidence is task- or episode-scoped and will be attributed through
the Eval Exchange. Omitting it records zero request-scoped outcomes; it never
broadcasts a task reward across the request set. Session/trial metadata and
timestamps are benchmark diagnostics, not strict join keys. Reward-feedback
admission also requires completed requests and authoritative settlement; it
does not use diagnostic identity fields for learning.

Bundles also write `routing-baselines.json` and embed the same report in
`run-artifact.json`. For each compatible candidate-set digest, the report
contains an always-tier control for every declared target and a deterministic,
content-blind control with exactly the observed selected-tier counts. Only
hashed decision identities are emitted. Baseline report schema v2 requires the
effective `(tier, model, effort)` to match a declared candidate. Continuation
pins to an undeclared effort are counted as `selected_target_mismatch`, as are
missing efforts when the candidate declares one; the pre-guard measurement is
not rewritten. Declared post-guard targets remain eligible, even when their
logging probability was zero. Dataset and baseline ID domains are versioned
to v2; regenerate controls from the original decisions. Legacy decisions
without measurement are counted as exclusions. These controls measure routing allocation; they do
not estimate the unexecuted models' quality or authorize a policy change.

---

## Policy

### `bro policy create <id>`

```
bro policy create strict [--dir ./policies]
```

Writes a starter policy file to the policy directory. Bind it to a key with:

```
bro key sign --user <id> --policy strict
```

---

## Cloud account management

`bro cloud …` drives the BitRouter Cloud API using the credential persisted by [`bro cloud login`](#bitrouter-cloud-login--logout--whoami). Sign in first, then call a typed management subcommand or the generic API command. Typed subcommands cover the common terminal workflows: namespace inspection, API keys, usage and request history, billing balance and checkout, policies, budgets, presets, and BYOK. Use `bro cloud api <relative-endpoint>` for the rest of the Cloud API surface, including public provider and usage discovery, settlement receipts, routing presets, OAuth clients, billing ledgers, checkout status, and namespace/account lifecycle endpoints.

OAuth credentials are **namespace-baked** — keys, usage, and policies are scoped to the workspace chosen at login. API-key credentials use `/v1/namespaces/me/*`. The path segment is always resolved implicitly; callers never pass a workspace argument. `billing` and `byok` are user-level and reach across all workspaces regardless.

Every leaf accepts `--json` to print the raw response body instead of the human-readable summary. On a 403 whose description is `missing required scope: <s>`, OAuth users receive a copy-pasteable re-login hint that appends the missing scope; API-key users are told to mint or select a key with that scope and log in with it.

### `bro cloud api`

Make an authenticated request to any **relative** endpoint on the origin recorded by `cloud login`, modeled after [`gh api`](https://cli.github.com/manual/gh_api):

```bash
bro cloud api /v1/models
bro cloud api /v1/chat/completions --input request.json
bro cloud api /v1/responses -f model=openai/gpt-5 -F stream=true
```

```text
bro cloud api <ENDPOINT> [-X <METHOD>] [-H <KEY:VALUE>] \
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

Absolute URLs, scheme-relative paths, fragments, and cross-origin redirects are rejected. Redirect following is disabled, so a stored bearer is never forwarded to another origin. Documented endpoints include `/v1/models`, `/v1/providers`, `/v1/stats/usage`, `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, Google-style `:generateContent` / `:streamGenerateContent` routes under `/v1beta/models/*`, namespace-scoped management routes under `/v1/namespaces/*`, and user-level routes under `/v1/account`, `/v1/billing/*`, and `/v1/byok/*`.

This first release intentionally omits `gh api`'s GraphQL, pagination/slurp, `--jq`, Go templates, cache, hostname, preview, and placeholder expansion features. See the [Cloud API guide](/docs/guides/cloud-api) for copyable requests.

### `bro cloud whoami`

```
bro cloud whoami
```

Prints the cloud identity and the bound namespace alongside the `/v1/*` base URL the CLI will target. Reads the local credentials file only — no network call.

### `bro cloud namespace`

Inspect the workspaces you own and the one this CLI session is baked to. The typed CLI only inspects workspaces; creation and deletion require the Console or `bro cloud api` with the appropriate control-plane scope.

```
bro cloud namespace list    [--json]
bro cloud namespace current [--json]
```

`list` fetches all namespaces you own and marks the active one. `current` is offline — it reads the local credential and prints the bound namespace id without a network call. If the credential predates namespace-scoping, it prints `(no namespace — run \`bro cloud login\`)`.

### `bro cloud keys`

Manage `brk_` API keys in the active workspace. All minted keys are workspace-baked to the same namespace as the caller and cannot upscale their scopes beyond the caller's.

```
bro cloud keys list [--json]
bro cloud keys mint --name <NAME> --scope <SCOPE> [--scope <SCOPE> …] [--expires-at <RFC3339>] [--json]
bro cloud keys revoke <ID> [--json]
```

Requested scopes on `mint` must be a subset of your effective scopes (RFC 6749 §3.3 — no upscaling). The plaintext token is shown once in the `mint` response and is not recoverable after.

### `bro cloud usage` / `bro cloud requests`

Read aggregate spend / token counts and page through recent inference requests.

```
bro cloud usage    [--from <RFC3339>] [--to <RFC3339>] [--json]
bro cloud requests [--limit <N>] [--offset <N>] [--json]
```

`usage` defaults to a 30-day rolling window. `requests` clamps the page size to `[1, 100]` and defaults to 25.

### `bro cloud billing`

User-level — not workspace-scoped; reflects the account-wide wallet regardless of which workspace the CLI is signed in to.

```
bro cloud billing balance [--json]
bro cloud billing checkout --amount-cents <N> [--json]
```

`checkout` starts a Stripe credit-purchase session and prints the hosted URL. Requires the `billing:write` scope, which is opt-in — pass `--scope` to `bro cloud login` to request it.

Use `bro cloud api /v1/billing/transactions` for the billing ledger, and `/v1/billing/checkout/sessions/<session-id>/status` for checkout status.

### `bro cloud policy`

Generic CRUD over the typed policy registry (kinds: `budget`, `rate_limit`, `guardrail`, `preset`).

```
bro cloud policy list [--kind <KIND>] [--json]
bro cloud policy get <ID> [--json]
bro cloud policy create --name <NAME> --kind <KIND> --spec <FILE|-> [--json]
bro cloud policy update <ID> [--name <NAME>] [--spec <FILE|->] [--json]
bro cloud policy delete <ID> [--json]
bro cloud policy bind <ID> --principal-type <TYPE> --principal-id <ID> [--json]
bro cloud policy unbind <ID> <BINDING_ID> [--json]
bro cloud policy enable <ID> [--json]
bro cloud policy disable <ID> [--json]
bro cloud policy bindings <ID> [--json]
bro cloud policy effective --principal-type <TYPE> --principal-id <ID> [--json]
bro cloud policy for-principal <TYPE> <ID> [--json]
```

`--spec` reads the flat inner spec body as JSON from a file path or `-` for stdin. Principal types: `namespace`, `api_key`, `oauth_token`, `oauth_client`. `disable` parks a policy without deleting it — the engine skips disabled rows at request time.

### `bro cloud budget` / `bro cloud preset`

Typed wrappers over the budget-kind and preset-kind policy rows — same storage, flat wire shape (no `kind`/`spec` envelope).

```
bro cloud budget list [--json]
bro cloud budget get <ID> [--json]
bro cloud budget create --name <NAME> --window <day|month|total> --limit-micro-usd <N> [--json]
bro cloud budget update <ID> [--name <NAME>] [--window <W>] [--limit-micro-usd <N>] [--json]
bro cloud budget delete <ID> [--json]

bro cloud preset list [--json]
bro cloud preset get <ID> [--json]
bro cloud preset create --name <NAME> [--guardrail <FILE|->] [--budget <FILE|->] [--rate-limit <FILE|->] [--json]
bro cloud preset update <ID> [--name <NAME>] [--guardrail <FILE|->] [--budget <FILE|->] [--rate-limit <FILE|->] [--clear-guardrail] [--clear-budget] [--clear-rate-limit] [--json]
bro cloud preset delete <ID> [--json]
```

Budget `--limit-micro-usd` must be strictly positive (the engine treats `<= 0` as "no policy" and the API refuses it up-front). Preset clauses are independently optional; use `--clear-*` flags to drop a clause from an existing preset.

### `bro cloud byok`

User-level — not workspace-scoped; BYOK provider keys are account-wide. The cloud only stores already-sealed ciphertext — seal against the cloud's current X25519 public key (separate fetch) before calling.

```
bro cloud byok list [--json]
bro cloud byok set    --provider <ID> --ciphertext-b64 <B64> --kek-id <ID> --key-prefix <PREFIX> [--api-base <URL>] [--json]
bro cloud byok delete <PROVIDER> [--json]
```

## Skills

`bro skills …` inspects Agent Skills — directories containing a `SKILL.md` with YAML frontmatter (`name`, `description`). The agent skills directory is `~/.claude/skills/` with `--global`, or `./.claude/skills/` (project-local) by default.

BitRouter **reads** the installed-skills directory; it does not install into it.
Getting a skill onto disk is the ecosystem's job — `npx skills add`, or the
Claude Code / Codex plugin marketplaces. BitRouter is a skills *server* and
*gateway*, not an installer: see `docs/SKILLS_MCP_SPEC.md` §2.

The `add`, `remove`, `find`, and `update` verbs were removed for that reason.
To serve installed skills over MCP, see `bro mcp serve` (every stdio
profile carries them) or the narrower `--backend skills`.

### `bro skills list`

```
bro skills list [-g|--global]
```

Prints the skills under the project root, or under `~/.claude/` with `-g`. Each
row carries the skill's `name`, `description`, its directory (`dir`) and its
`skill_md`, plus `valid` and — when it is not — a `problem` saying why.

Discovery covers all three conventional layouts of the chosen root:
`<root>/SKILL.md`, `<root>/skills/<name>/`, and `<root>/.claude/skills/<name>/`.
It used to read only the last, so a `./skills/foo` skill was invisible here while
the agent could see it.

A skill whose frontmatter does not parse, whose directory name does not match
`frontmatter.name`, or whose name/description falls outside the Agent Skills
bounds is listed with `valid: false` and the reason. It is *not* served over
SEP-2640's `skills/list`, which requires an entry a host can verify — so this
listing is where you find out why a skill you wrote is not loading.

This is the same report the `skills_search` MCP tool returns, so
`--json` here and that tool's structured content are the same bytes.

### `bro skills init <name>`

```
bro skills init <NAME> [-o|--output <PATH>]
```

Scaffolds a starter `<NAME>/SKILL.md`; `--output` may choose another path whose
file is still named `SKILL.md` and whose parent directory equals `<NAME>`.
Refuses to overwrite an existing file. Names follow the Agent Skills grammar:
1–64 lowercase ASCII letters, digits, or non-leading/trailing single hyphens.

## Local ACP recordings

Opt in with `acp_recording.enabled: true` in the selected config. This records
observable ACP content from `code`, `run`, and `acp serve` in the local database,
independently of `trajectory.enabled`. It does not invoke an evaluator.

```bash
bro acp recordings [--config PATH] list --agent codex-acp
bro acp recordings [--config PATH] show --agent codex-acp NATIVE_SESSION_ID
bro acp recordings [--config PATH] delete --agent codex-acp NATIVE_SESSION_ID
```

The source is the resolved configured agent ID, and the session ID remains
harness-native. `show` returns canonical events, separate replay audit, gaps,
and observed metering/route links. Output follows the global JSON/`--human`
convention. Content persists until explicit deletion; deletion fences further
recording for that native identity and does not touch native harness history
or metering. See [the recording contract](ACP_CANONICAL_CAPTURE_SPEC.md) for
visibility, write-failure, and cost-accounting semantics.

## ACP checkpoints and assessment history

Use the source and native ID from `acp recordings list`, and the current `head`
from `acp recordings show`. Creation rejects a stale expected watermark.

```bash
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] create --watermark N
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] list
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] show CHECKPOINT_ID
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] resources CHECKPOINT_ID [--refresh]
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] submit assessment.json
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] history
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] effective
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] family
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] rubric prepare CHECKPOINT_ID
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] rubric submit rubric.json
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] judge CHECKPOINT_ID --model MODEL
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] judge-job JOB_ID [--resume]
```

Their evidence comes from existing local records only. `judge` explicitly sends
the frozen evidence to the selected configured model. `create` freezes event references
and observes local resource records. `show` verifies and resolves original source
versions. `resources` lists immutable metering observations; `--refresh` adds an
observation if local data changed, without changing the content checkpoint.

Local recording registers request-inventory coverage with the serving daemon
before forwarding its first event. An older/unavailable daemon or a remote
endpoint leaves this coverage unacknowledged; recording itself remains usable.
The resource output includes `gateway_coverage` with its scope, revision fences
and incompleteness reasons. `metering_complete` applies only to BitRouter-managed
model requests and requires closed prompt boundaries, healthy acknowledged
capture, resolved request membership and complete settled attempt costs.
Unknown prices, unfinished requests and incompletely priced fallbacks remain
unknown. Refresh after late settlement to obtain a new immutable resource
observation. A known subtotal alone cannot authorize evolution promotion.

`submit` imports a JSON revision envelope. Copy checkpoint and current revision
IDs from command output. The pipeline and selection digests identify the scoring
configuration that produced the imported labels; this layer does not generate
templates or scores. For example (replace identifiers and digest placeholders):

```json
{
  "submission_id": "human-review-1",
  "checkpoint_id": "CHECKPOINT_ID",
  "expected_revision": null,
  "source": "human",
  "evaluator_id": "local-user",
  "evaluator_version": "1",
  "assessment": {
    "pipeline_config_digest": "LOWERCASE_SHA256_DIGEST",
    "selection_digest": "LOWERCASE_SHA256_DIGEST",
    "scores": {
      "completion": { "status": "scored", "value_ppm": 1000000 },
      "correctness": { "status": "unknown" },
      "pr_delivery": { "status": "not_applicable" }
    },
    "evidence": [],
    "explanation": "Judgment based on the recorded checkpoint."
  },
  "reason": "Initial manual assessment"
}
```

Use `expected_revision: null` only when no selection exists. For a correction,
provide the current revision ID and a new submission ID. A retraction uses
`assessment: null`, the current checkpoint/revision IDs, Human source and a
reason. Evidence entries have `node_id` and `digest`, matching checkpoint
references. Scores range from 0 to 1,000,000 ppm; unknown is not zero.

Retries with the same submission ID and input are idempotent. Older workers
cannot overwrite a newer selection. Appending a session marks its previous
assessment stale, while preserving the old checkpoint. Deletion invalidates
dependent checkpoints and removes associated assessment text. Only `judge` and
`judge-job --resume` invoke a model; checkpoint and manual scoring commands do
not publish routes. See [the checkpoint contract](ACP_CHECKPOINT_SPEC.md).

`judge` creates a durable job for the checkpoint/model contract. It uses the
fixed rubric library, sends no tools or ACP routing identity, and imposes no
product token or cost ceiling. A completed retry returns the same job without
another model call. `judge-job` inspects its status, request IDs, cached output
and selected revision; `--resume` retries the recorded input/model after its
worker lease expires. A cached valid response is reused after a restart.
Uncertain attempts retain distinct request IDs for cost accounting. There are
at most three model attempts per job; this operational retry limit is not an
evaluation budget. Failed jobs are inspectable and cannot overwrite a newer
manual revision. Deleting source content removes cached judge text as well.

`rubric prepare` exports the fixed coding rubric library, the current revision
ID, and original cited evidence from the selected checkpoint. Evidence carries
`projection_version: "recorded-acp-quality-evidence-v2"`. Protocol routing/usage
metadata, provider setup and private thought chunks are omitted, including nested
session-result metadata. Stop reasons and error code/messages are retained; the
canonical source and citation digests remain unchanged. Task/tool content is
preserved and may still reveal identity clues. The output contains conversation
content and must be treated as a local content export.

`rubric submit` accepts `submission_id`, `checkpoint_id`, `expected_revision`,
`source`, `evaluator_id`, `evaluator_version`, and `evaluation`. The evaluation
contains `rubric_version: "coding-checkpoint-rubric-v2"`, `items`, `diagnostics`,
`severe_violation`, `violation_evidence`, and `summary`. Every library item must
appear once, with `criterion_id`, `applicability` (`applicable`, `not_applicable`,
or `unknown`), `selection_reason`, `score`, `evidence`, and `explanation`. Scores
and citations use the checkpoint forms described above. Diagnostics carry
`criterion_id`, an evidence-backed `role` (`introduced`, `discovered`, `repaired`,
`inherited`, or `unknown`), `evidence`, and `explanation`.

Mandatory rubrics cannot be excluded; unknown applicability remains unknown;
positive verification needs a tool observation, not just an assistant claim.
Weights are fixed by the library. Returned lower and upper quality values bound
missing rubric values, not statistical confidence. Programmatic validation checks
the contract and references; it does not prove that every semantic judgment is
correct. Manual submission makes no model call. A model acting as a reference
reviewer must use `source: "agentic"`, not `human`.

Rubric v2 binds responsibility to the recorded task: review-only findings are
scored under delivery, and `review_resolution` applies only to an obligation to
resolve findings or obtain acceptance. Explicitly forbidden/deferred execution
is excluded from verification for that checkpoint. Required checks blocked by
the environment remain applicable with unknown scores; they are not automatically
code failures. Positive executable verification needs relevant execution evidence,
not a source-file read. Constraints describe how work was performed and do not
count a missing deliverable a second time.

The judge is `recorded-evidence-judge-v2`. Rubric and evidence versions are included
in measurement contracts. Historical v1 labels remain inspectable but are not
converted or pooled into v2 learning. Incomplete jobs whose input contract changed
are retired before reserving another request. A committed historical revision
remains intact even if its old job was not finalized. Explicitly judging again
creates the current-version job; this upgrade does not bulk-judge history.

### `bro acp evolution`

Control checkpoint feedback and policy-block experiments through the existing
local serving daemon. Use the same config as that daemon; these commands do not
start it automatically.

```bash
bro acp evolution [--config PATH] status
bro acp evolution [--config PATH] mode off
bro acp evolution [--config PATH] mode manual
bro acp evolution [--config PATH] mode automatic --judge-model MODEL
bro acp evolution [--config PATH] register block.json
bro acp evolution [--config PATH] revise next-block.json --expected-experiment EXPERIMENT_ID
bro acp evolution [--config PATH] restore BLOCK_ID --expected-experiment EXPERIMENT_ID --expected-revision REVISION --reason "Reason for withdrawal"
bro acp evolution [--config PATH] learning BLOCK_ID [--experiment EXPERIMENT_ID]
bro acp evolution [--config PATH] improve BLOCK_ID [--experiment EXPERIMENT_ID]
```

Evolution defaults to `off`. `manual` discovers stopped recorded checkpoints
without invoking a model; submit their scores with `acp checkpoints ... rubric
submit`. `automatic` also judges those checkpoints using the configured model.
The model is retained across mode changes, so `--judge-model` may be omitted
after one is saved. Changing it starts a new feedback epoch and fences older
automatic jobs. There is no product token or cost ceiling for the judge.

The daemon polls recorded prompt stops in the background. It considers stops
after the current mode/model epoch began, so enabling automatic mode does not
bulk-judge historical sessions. Continued sessions produce later immutable
checkpoints and replace their effective learning contribution. Explicit `judge`
commands can evaluate older checkpoints. Recording must be enabled separately.

Resource membership `native-head-resources-v2` includes this session's model
requests admitted through its checkpoint head, including auxiliary calls after
a prompt stops at that same head. They update cost without another content
evaluation. A new prompt advances the head; inherited fork prefixes still exclude
later parent work. Unresolved or unfinished calls remain incomplete. Older
resource records are preserved and refreshed before learning uses their costs.
Complete cost describes the observed inventory; final cost through worker exit
also requires confirmed capture closure and settled request outcomes.

Trial assignment requires a new recorded session. ACP setup notifications about
commands, configuration, mode or session metadata preserve that eligibility.
Earlier assistant content, tools, usage or unknown/malformed updates prevent
late enrollment. A worker warning emitted as assistant text also takes this
conservative path; a custom model alias that triggers such a warning can remain
on its baseline for the session. The enrollment's `admission_reason` explains
this exclusion, and later requests do not resample it.

`status` reports the control state, worker progress, scheduled checkpoints and
judge job summaries, including failed attempts. Cached model text is available
through the session-scoped `judge-job` command rather than this summary. A
restart resumes eligible durable work; shutdown preserves uncertain attempt IDs.
Queued work made obsolete by an append, manual correction or mode change is
marked `superseded`. Completed canonical assessments can have their job receipts
recovered while off, without a model call or a new assessment selection.

`register` imports a complete `BlockDefinition` JSON object and checks its policy
selectors and dependencies against the live serving configuration. Registration
does not change the feedback mode. `learning` reports evidence and proposed
allocation without publishing. `improve` may publish an eligible allocation,
adoption or withdrawal; the background worker performs the same reconciliation
while evolution is enabled. Both paths require current evidence and live route
dependencies. Turning evolution off fences new automatic judging, trial
assignment and publication, while retaining adopted baselines.

`revise` starts the next experiment for an existing block. Read its current
`experiment_id` from `status` and pass it as `--expected-experiment`. The new
definition must preserve the block ID, agent source and complete matcher set.
Its baseline routes must inherit the currently supported baseline: the previous
candidate if adopted, otherwise the previous baseline. If the previous live
route contract or declared dependency changed, use the configured selectors as
baselines. Routes and the predecessor are rechecked before registration.
Registration preserves the mode and starts fresh learner/cohort state.

Prior versions remain under `archived_experiments`; existing sessions retain
their version and arm. `--experiment` targets a specific current or archived
version, while omission selects the current version. Archived trials accept no
new sessions and cannot gain a new adoption. Their feedback can still retract
an adoption and withdraw later versions that inherited it. Routing falls back
to the last supported baseline, subject to live dependency checks. Other blocks
are unchanged except where they declare dependencies on a withdrawn revision.

The local TUI exposes **Start the next experiment** and **Experiment history**.
A revision draft inherits its matchers and supported baselines; edit candidate
routes and the trial reason, then review and register. The preview states when
a route change requires rebasing to configured selectors. Reconciliation from
an evidence view stays bound to the displayed experiment, even after a newer
version is registered.

`learning` distinguishes the original trial evidence from quality monitoring of
new sessions after adoption. `improve` and the worker can withdraw an adopted
block when valid monitoring evidence crosses its quality floor or records a
severe violation. Continued monitoring does not add randomized trial samples or
prove ongoing comparative cost savings. Original trial corrections can also
invalidate adoption. Sessions first admitted while off are not enrolled for
monitoring retroactively. The TUI's block inspector displays these separately.

During cold start, learner v2 retains bounded initial allocation until both arms
have the configured minimum independent quality, cost and latency evidence.
Pending/cumulative trial limits and quality withdrawal still apply. Existing
reduced exposure is not automatically raised. A persisted incompatible learner
plan holds new trial admission until local reconciliation; this does not require
another judge call or reassign existing sessions.

See [the evolution contract](ACP_EVOLUTION_SPEC.md) for the block, reward and
publication semantics. Local coding TUI sessions expose mode controls, manual
rubric editing and existing-block reconciliation through `/evolution` as
described above. These controls do not establish that automatic promotion is
safe on real coding tasks.
