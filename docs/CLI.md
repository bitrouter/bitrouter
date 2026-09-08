# BitRouter CLI Reference

`bitrouter <subcommand> [flags]`

## Output format

Every command prints a single **formatted JSON object** to **stdout** — success or failure — so output is machine-parseable by default (agent-native first). Global flags:

- `-j`, `--json` — force JSON (the default).
- `--human` — render a human-readable view to stdout instead of JSON.
- `--context <name>` — run a supported read action against a named remote
  BitRouter target; `local` explicitly selects normal local behavior.
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

> Non-reporting commands are exempt: `serve` and `mcp serve` are long-running servers; `acp serve` is a stdio JSON-RPC bridge; `run` streams NDJSON by default; `code` and `launch` own the terminal; and `cloud api` streams the remote response body.

Per-provider credential commands are under `bitrouter providers (login|logout)`; BitRouter Cloud sign-in is `bitrouter cloud (login|logout|whoami)`.

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

- `bitrouter config validate` lists them under `ignored_config`. It does not
  fail validation — an ignored block is a misconfiguration, not a malformed
  config, and this command is CI-gating.
- Every runtime surface logs one WARN per unread id on start: the daemon, and
  `bitrouter acp serve`, `bitrouter run`, and `bitrouter code <agent>`, none of which build the
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
bitrouter context add workstation \
  --endpoint https://router.example/control/v1 \
  --token-env WORKSTATION_BITROUTER_TOKEN
bitrouter context list
bitrouter context show workstation
bitrouter --context workstation status
bitrouter --context workstation requests
bitrouter --context workstation models --provider openai
bitrouter --context workstation route openai/gpt-5
bitrouter --context workstation providers list
bitrouter --context workstation observe status
bitrouter --context workstation policy status       # active by default
bitrouter --context workstation policy show default --view disk
bitrouter --context workstation agents list
bitrouter --context workstation reload
bitrouter context remove workstation
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

### `bitrouter serve`

Run the HTTP server and control socket **in the foreground**.

```
bitrouter serve [-c <path>]
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
bitrouter --context <name> reload
```

Hot-reloads the running daemon's config and routing table without dropping connections. Also triggered by `SIGHUP`.

Any provider API keys present in the current environment are forwarded to the daemon so `export OPENAI_API_KEY=…; bitrouter reload` takes effect immediately.

With `--context`, reload submits a guarded operation against the server's own
current configuration. It does not forward client environment variables. The
report includes a request id, server instance, generation, and per-subsystem
result. A running, failed, partially applied, or unknown operation exits
non-zero; use its operation lookup details and the target's state before retrying.

### `bitrouter status`

```
bitrouter status [-c <path>] [--socket <path>]
bitrouter requests [--limit N] [--since RFC3339 --until RFC3339] [--model ID] [--provider ID]
bitrouter requests --human           # the same, as a table
```

Prints pid, listen address, number of routable models, the distinct providers behind them, the control socket path, and the **spend position**. Exits cleanly with "stopped" when no daemon is reachable.

The same report the origin MCP server's `status` tool returns — one shared type, so `bitrouter status --json` and that tool's structured content are the same bytes.

**`spend` — what has gone, and what is left.** Two independent facts, each present only where the deployment can answer it:

| Key | Filled by | Means |
|---|---|---|
| `spend.spent` | any deployment (the local metering database) | Money already gone: `estimated_micro_usd` over `window` (today, since 00:00 UTC), `requests`, and `unpriced` |
| `spend.limit` | a deployment with a cap (today: a metered cloud account's prepaid credit) | Money still available: `balance_micro_usd`, `pending_micro_usd`, `remaining_micro_usd` |

`spent` is an **estimate and a floor**, not a total. It is priced from BitRouter's own registry at settle time, and requests with no charge evidence are excluded rather than summed as zero — summing them would report a floor as a price. `unpriced` counts exactly those, so a non-zero value means the figure understates by an unknown amount; the human view marks it `floor, not a total`. A `spend.limit` is the opposite kind of number: an authoritative ledger the account is settled against. Read `unpriced` before treating the two as comparable.

The read is best-effort and never fails the command: no config, no database file, or an unreadable one gives no `spend` key at all — which is a different answer from `estimated_micro_usd: 0` over `0` requests, meaning "nothing spent today". It also works with **no daemon running**, so a `stopped` report still carries spend: what a past daemon spent is on disk and does not stop being true when it exits.

The figure is **machine-wide**, not per-caller: it rolls up every caller of this daemon, the same scope `requests` reports. Per-session spend is `bitrouter code <agent>`'s cost line.

`bitrouter status --json` gained `spend` additively; every pre-existing key is unchanged.

`bitrouter requests` reports what the router has actually done: newest-first
settled requests — time, model, the provider that **actually** served, tokens
in/out, cost, latency, status — plus daemon state and the window's spend and
trailing-minute rate. It reads the metering store directly, so it also works
with **no daemon running**. `--since` and `--until` are an RFC3339 pair with a
maximum seven-day interval; absent both, the server selects today from UTC
midnight. `--model` and `--provider` filter the same report before the limit.
`status --requests` remains a hidden compatibility spelling.

Like every other report it uses JSON by default and `--human` for the table. Repeat it with `watch -n1 bitrouter requests --human` for a live view.

The spend rollup covers every caller of the daemon, not one session. `bitrouter code <agent>`'s cost line is the per-session figure.

**Spend is reported only where there is evidence.** An unpriced request shows `?`, not `$0.00`; the same honesty rule applies to `code <agent>` session cost.

Each row also carries `episode_id` — the trajectory episode to hand to `bitrouter trajectory inspect`, or `null` when trajectory capture recorded nothing for it (capture is opt-in and off by default, so `null` is the common case). It is the thread from a settled request to its structural record, which is otherwise reachable only by an episode id nothing else hands out.

Portable — there is no terminal-only path left to gate.

> **`--requests` emits JSON by default as of 1.0.0-alpha.28.** It previously printed the table unconditionally, ignoring `--json` — the only `status` path that did. Scripts that parsed the table need `--human`; anything that wanted the data now gets one clean JSON object with a stable `rows[]`.
>
> **Replaces `--watch` (`-w`), removed in 1.0.0-alpha.28.** That flag opened a self-refreshing ratatui view with cursor keys plus `r` (reload) and `e` (`$EDITOR` on `bitrouter.yaml`). Both of those keys ran commands you can still run directly — `bitrouter reload`, and your editor — and the piped form of `--watch` printed what `--requests --human` prints now.

---

## Config

### `bitrouter init` (onboarding wizard)

```bash
bitrouter
bitrouter init --yes --harness codex --after exit
bitrouter init --harness claude --model anthropic/claude-sonnet-4-6
```

Bare `bitrouter` opens first-run onboarding when no default ACP harness is
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

### `bitrouter route <model>`

```
bitrouter route gpt-4o [--prompt <text>] [-c <path>] [--socket <path>]
```

Resolves a model name through the routing table and prints the full fallback chain (provider → upstream service id → protocol). Queries the running daemon if reachable — its `route` verb resolves the model exactly as given, since the daemon's policy table runs on real requests rather than on this preview — and otherwise falls back to a local config parse, **policy table included**, so `effective_model` there is what would actually run.

`--prompt` supplies the request text the policy table keys on: it routes by the agent-loop step a request represents, so the model it selects can differ with the prompt. Omit it for a bare model resolution. It is consulted on the config path only; a `live` answer is the same with or without it.

The report is the shared `route` action's, so `bitrouter route --json` is byte-identical to the MCP `route_preview` tool's structured content:

| Field | Meaning |
|---|---|
| `requested_model` | what you asked about |
| `effective_model` | what would actually run — differs when the policy table selects another model |
| `effective_effort` | the reasoning effort policy selected, when it selected one |
| `resolved_via` | `live` \| `config` \| `zero_config` — the same words `bitrouter models` uses |
| `policy_decision` | the static decision behind `effective_model`. Absent on `live`: the daemon's `route` verb does not replay policy, so there is no decision to show and `effective_model` equals `requested_model` there |
| `provider_chain[]` | `provider` / `service_id` / `api_protocol`, preferred hop first. Never the provider's credential |
| `estimated_cost` | the first hop's per-token rate card, including any steeper long-context brackets. Rates, not a total: nothing was sent |

Read-only throughout — nothing is sent upstream.

### `bitrouter models`

```
bitrouter models [-c <path>] [-p <provider-id>]
```

Lists all routable models, each with **every** provider that can serve it — the
fallback chain, in order. Filter to one provider with `--provider`.
Subscription providers are explicit-route-only, so their rows use a pinned
`provider:canonical-model` selector (for example,
`openai-codex:openai/gpt-5.6-sol`). Copying any displayed selector into
`bitrouter route` therefore previews the route without implicitly opting a bare
canonical request into a personal subscription.

Queries the running daemon if reachable and falls back to a local config parse,
the same order `bitrouter route` uses: the live routing table reflects `reload`s
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
`bitrouter models --json` and the tool's structured content are the same bytes.

### `bitrouter providers list`

```
bitrouter providers list [-c <path>] [--socket <path>]
```

Local compatibility output prints each provider's id, model count,
routing-active state, and `api_base`. Named remote contexts and the dashboard
use the redacted accepted catalog, which omits API bases and credentials.
`active` means the accepted routing configuration includes the provider, not
that a connectivity probe succeeded.

---

## MCP upstream diagnostics

### `bitrouter mcp check [server]`

```bash
bitrouter mcp check                 # every configured upstream
bitrouter mcp check my-server       # one configured upstream
```

Performs one `tools/list` round trip per selected server and reports transport,
reachability, latency, negotiated tools capability, and advertised tool names.
This is the canonical diagnostic. The `tools` commands below are hidden
compatibility spellings.

### Hidden compatibility: `bitrouter tools`

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

## Origin MCP server

`bitrouter mcp serve` runs BitRouter itself as an **origin** MCP server, so an
MCP-capable client (Claude Code, Claude Desktop, Cursor, …) can call BitRouter's
own capabilities as tools. This is the inverse of `bitrouter mcp check` and the
`mcp_servers:` config block, where BitRouter is the MCP *client* proxying
upstream servers.

### `bitrouter mcp serve`

```
bitrouter mcp serve
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
parameter surface, and the metering path. `bitrouter mcp serve` tells you which
models to send there (`list_models`), where they would go (`route_preview`),
and what it has cost (`status`).

| Tool | Wired on | What it answers |
|---|---|---|
| `list_models` | every profile | Every routable model with **all** the providers that can serve it, not just the first. Optional `provider` argument filters, exactly as `bitrouter models --provider` does. Returns the same report type as `bitrouter models`, advertised as the tool's `output_schema`. On stdio + local it reads the daemon's live routing table over the control socket and falls back to a static config parse, so **it answers with no daemon running**; `resolved_via` says which view it is. Other profiles answer with the backend's own `GET /v1/models`, which does need the daemon (or the metered account) up |
| `status` | stdio + local, and any cloud profile | Daemon liveness (pid, listen address, model count, providers, control socket) plus the spend position — `spend.spent` on any deployment, `spend.limit` on a metered one. Returns the same report type as `bitrouter status`, advertised as the tool's `output_schema`. A stopped daemon is `running: false`, not a tool error. Not wired on HTTP + local: only a process on the daemon's own machine can read its control socket |
| `route_preview` | stdio + local | How a model/prompt *would* route — the effective model the policy table selects, the provider chain, the decision behind it, and the first hop's rate card — without sending anything upstream. Returns the same report type as `bitrouter route`, advertised as the tool's `output_schema`. Config is read **per call**, so an edited `bitrouter.yaml` is visible to a long-running server |
| `skills_search` | every **stdio** profile | Every skill on this machine, optionally narrowed by `query`. Returns the same report type as `bitrouter skills list`, advertised as the tool's `output_schema`. Reads the project *and* user-global roots, and marks any skill it found but cannot serve with `valid: false` plus a `problem` |
| `skills_get` | every **stdio** profile | One skill's frontmatter metadata and `SKILL.md` body |

Only wired capabilities register their tools, so the profiles stay disjoint by
construction: an HTTP client never sees `route_preview` or the skills tools
(both read the serving machine's own routing table and skill library, which has
no meaning on a multi-tenant transport).

The skills tools ride the **transport**, not the backend: a stdio server is a
subprocess of the caller whose machine it is, which is the same argument that
makes `--backend skills` stdio-only. So a `bitrouter mcp install`-ed client —
which launches `bitrouter mcp serve` — sees skills too; before, only
`--backend skills` did, and an installed client never saw one.
`--backend skills` survives as the narrow gateway-subprocess profile that
carries *nothing else*.

Every stdio profile also serves SEP-2640's `skills/list` / `skills/get` JSON-RPC
methods plus `resources/list` / `resources/read` over the skill files, for hosts
that consume the extension rather than the tool pair. `skills/list` publishes
only the skills that are actually loadable; `skills_search` and
`bitrouter skills list` show the rest, marked, so an author can see why a skill
on disk is unusable. Each published entry carries a complete `resources`
manifest with a `digest` and a byte `size` per file.

Spend reaches an MCP client as **typed structured content** under `status`'s
`spend`, read from the local metering database — the same ledger
`bitrouter status` and `bitrouter cost` report from, so the surfaces cannot
disagree about what has been spent.

### Hidden compatibility: `bitrouter mcp install`

```
bitrouter mcp install --client claude|cursor [--config PATH]
```

Renders the client config block that launches `bitrouter mcp serve` over stdio.
With `--config`, merges it into that file; without, prints it to stdout.

---

## Hidden compatibility: MCP registry discovery

These commands are hidden from normal help during the compatibility window.
`mcp_servers:` remains the declarative source of truth; this legacy browser does
not write config.

### `bitrouter mcp search <query>`

```
bitrouter mcp search filesystem [--limit N]
```

Searches registry names server-side and prints rows of `name / version / install / description`. The install column classifies support: `remote` (zero-install `streamable-http` entry), `npx` / `uvx` (auto-stub-able, version-pinned stdio package), `manual` (another package type or an entry that is not safe to auto-stub), `-` (no distribution).

### `bitrouter mcp list`

```
bitrouter mcp list [--limit N]
```

Lists registry servers with the same install-support column (default 50 rows).

### `bitrouter mcp add <name>`

```
bitrouter mcp add com.pulsemcp/remote-filesystem
```

Prints a YAML stub to review and paste under `mcp_servers:`. This legacy helper is hidden during the compatibility window; `mcp_servers:` remains the declarative source of truth.

---

## ACP agent management

### `bitrouter agents list`

```
bitrouter agents list [-c <path>]
```

Shows the built-in agent catalog alongside which agents are configured in the loaded config.

### `bitrouter agents inspect`

```bash
bitrouter agents inspect claude
```

Opens a fresh harness-native session, waits briefly for its advertised slash
commands, and reports which source answers each command.

### `bitrouter agents check`

```
bitrouter agents check [agent] [-c <path>]
```

With an agent or friendly alias, preflights that adapter and routing target.
With no agent, spawns each configured adapter and verifies `initialize`.

### `bitrouter agents scaffold <id>`

```
bitrouter agents scaffold claude-code
```

Prints a YAML stub for the named catalog or registry agent. Paste the output
under `agents:` in `bitrouter.yaml`. `agents install` remains a hidden alias.

### `bitrouter agents conformance <id>`

```
bitrouter agents conformance local/claude-acp
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

### `bitrouter run` — headless agent

```
bitrouter run <agent> [prompt|-] [--prompt-file PATH] [--load ID|--resume ID]
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

### `bitrouter acp`

```
bitrouter acp serve <agent> [-c <path>]
```

Exposes an ACP-compatible adapter over protocol-pure stdio until the ACP client
disconnects; one controller connection can carry multiple
harness-native sessions. Hidden `acp prompt` and `spawn` spellings remain only
for migration. BitRouter keeps no session records.

### `bitrouter code` — coding conversation

```bash
bitrouter code [-c <path>]
bitrouter code <agent> [--load <id>|--resume <id>] [--model <id>] [--turn-timeout <secs>] [--direct] [--base-url <url>] [--no-start] [-c <path>]
bitrouter code --socket <path>
bitrouter --context <name> code
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
bitrouter launch <agent> [--model <id>] [-c <path>] [--base-url <url>] [--no-install] [--no-start] [--check] -- <agent args…>
bitrouter claude [options] -- <claude args…>
bitrouter codex [options] -- <codex args…>
```

Launches a coding-agent harness as an **interactive native-TUI** child process with its gateway base URL pointed at BitRouter, so the agent's traffic routes through the router **without touching the agent's own config files**. This is the native-harness compatibility surface — the human drives the harness's own TUI; for a headless prompt use `bitrouter run`.

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
| `pi` | synthesized `PI_CODING_AGENT_DIR` with a `models.json`, selected by `--provider bitrouter --model …` |
| `hermes` | synthesized `HERMES_HOME` with a `config.yaml` (loopback `custom` provider + `CUSTOM_API_KEY`) |
| `openclaw` | synthesized `OPENCLAW_STATE_DIR` + `OPENCLAW_CONFIG_PATH` profile (run as `tui --local`) |
| `grok`, `agy` | **not routed** — own-auth subscription clients (see below) |


The synthesized files are throwaway, written under the working tree's self-ignoring `.bitrouter/launch/`; the user's own `~/.config` is never touched. Their model lists come from the daemon's `/v1/models` (best-effort — an unreachable daemon just yields an empty list and the harness keeps its own defaults).

**Gateway MCP servers.** `launch` also injects BitRouter's two MCP-shaped gateways into the harness: `bitrouter_tools` (the daemon's aggregate endpoint at `mcp.aggregate.route`, fanning out to every configured `mcp_servers` upstream — omitted when `mcp.aggregate.enabled: false`) and `bitrouter_skills` (this binary as `mcp serve --backend skills`, over the installed-skills root). Injection reaches the harnesses that have a mechanism for it — `claude` (`--mcp-config`), `codex` (`-c mcp_servers.*`), and `opencode` and `hermes` (their synthesized config files). `pi`, `openclaw`, `grok`, and `agy` expose no injectable MCP surface and launch without the gateways.

`--model <id>` pins the harness's model through whatever mechanism it has. Following `cargo run`'s convention, everything after `--` is forwarded verbatim, e.g. `bitrouter launch claude -- -p "summarize" --dangerously-skip-permissions`.

**`grok` and `agy` are own-auth harnesses.** They launch with their own subscription auth and are **never redirected** — the startup line says `own-auth · not routed · not metered`, and `--check` reports it as a `routing` warning. They also remain **providers**: subscription clients whose sessions the daemon borrows to serve *other* requests (`supergrok` / `google-ai`), which is a separate stack and unaffected.

The agent authenticates to BitRouter with `BITROUTER_API_KEY` when set; otherwise a local placeholder is used (fine under the `skip_auth` default written by `bitrouter init`). A missing `claude` / `codex` binary is offered for install via its official native installer (`--no-install`, or a non-TTY stdin, declines); the other harnesses have no bundled installer and error with a pointer to their upstream project.

When the target is the local daemon (a derived base URL on a loopback/wildcard bind) and none is running, `launch` **auto-starts it** — printing a hint, launching a detached `serve`, and waiting for readiness before handing off to the agent. Pass `--no-start` to skip this (a reachability warning is printed instead). An explicit `--base-url` or a non-local bind is never auto-started — BitRouter can't start someone else's daemon — and only gets a warning if it looks unreachable.

After the wrapped agent exits, `launch` prints a one-line session spend summary to stderr (spend during the run + today's total, from the local metering database). Silent when nothing was recorded in the window — e.g. when the run targeted Cloud.

`bitrouter spawn --agent <claude|codex>` is a **deprecated alias** for `launch` (prints a migration note); it will be removed after one or two alpha releases.

### Hidden `spawn` compatibility

The old `spawn` umbrella remains parseable for one migration window but is
absent from normal help. Its modes call the canonical implementations:
`spawn <agent> -p TEXT` maps to `run`, `spawn <agent> --serve` maps to
`acp serve`, and `spawn <agent> --check` maps to `agents check`. New scripts
and integrations must use the canonical commands above; protocol-serving
compatibility forms keep stdout reserved for ACP frames.

### `bitrouter policy`

```text
bitrouter policy init NAME --preset PRESET --economy MODEL [--economy-effort LEVEL] \
  [--strong MODEL] [--strong-effort LEVEL]
bitrouter policy check|status|show [--config PATH]
bitrouter policy compile --output FILE [--eval-snapshot SHA256] [--snapshot-time UNIX_MS]
bitrouter policy diff ACTIVE CANDIDATE
bitrouter policy publish CANDIDATE [--config PATH] [--socket PATH]
bitrouter policy verify --evidence [--config PATH]
bitrouter policy evolve [--config PATH] [--apply | --output FILE]
bitrouter policy reload [--config PATH] [--socket PATH]
bitrouter policy rollback DIGEST [--config PATH] [--socket PATH]
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

### `bitrouter optimize`

```text
bitrouter optimize run [--policy auto] [--candidate-tier TIER] \
  [--exploration-ppm 100000] [--minimum-tasks 3] [--maximum-tasks 20] \
  [--minimum-pass-rate-ppm 900000] \
  [--evaluator-config-digest sha256:...] \
  [--config bitrouter.yaml] [--socket PATH]
bitrouter optimize status [--policy auto] [--config bitrouter.yaml]
```

Optimization is driven by history from normal use, not by a bundled workflow
runner. Initialize a policy, run a coding agent or Terminal Bench normally,
submit externally evaluated results, and advance the controller one step:

```bash
bitrouter policy init auto --preset auto --economy provider:model
# run the coding agent or Terminal Bench normally through bitrouter/auto
bitrouter eval result submit result.json --config bitrouter.yaml
bitrouter optimize run --policy auto --config bitrouter.yaml
bitrouter optimize status --policy auto --config bitrouter.yaml
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

### `bitrouter trajectory`

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
bitrouter trajectory inspect EPISODE_ID
bitrouter trajectory replay EPISODE_ID
bitrouter trajectory prune --before RFC3339 [--dry-run]

bitrouter trajectory --config PATH inspect EPISODE_ID
bitrouter trajectory --config PATH replay EPISODE_ID
bitrouter trajectory --config PATH prune --before RFC3339 [--dry-run]
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

### `bitrouter eval`

```text
bitrouter eval subject put FILE [--config PATH]
bitrouter eval subject get EVAL_ID [--config PATH]
bitrouter eval subject list [--config PATH]
bitrouter eval result submit FILE [--config PATH]
bitrouter eval snapshot freeze [--at RFC3339] [--config PATH]
bitrouter eval snapshot get SHA256 [--config PATH]
bitrouter eval status [--config PATH]
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
bitrouter workflow-state classifier-bakeoff --fixtures <DIR> [--submission <JSON>] --output <JSON>
bitrouter workflow-state metering-usage --database-url <URL> --output <JSONL> [--since <RFC3339>] [--until <RFC3339>] [--impute-price <SPEC> ...]
bitrouter workflow-state reconcile-metering --database-url <URL> [--api-base <URL>] [--api-key-env <NAME>] [--credentials-file <PATH>] --request-id <ID> ... [--price <SPEC> ...] [--max-attempts <N>] [--poll-interval-ms <MS>]
bitrouter workflow-state reliability-report --database-url <URL> --config <PATH> --output <JSON>
bitrouter workflow-state policy-oracle --traces <JSONL> --cloud-usage <JSONL> --policy-lock <YAML> --policy <NAME> --effective-cost-factor <0..1> --target-savings <0..1> ... --output <JSON>
bitrouter workflow-state bundle --run-label <LABEL> --traces <JSONL> --cloud-usage <JSONL> [--outcomes <JSONL>] [--policy-decisions <JSONL>] --output-dir <DIR>
bitrouter workflow-state apply-reward-feedback --database-url <URL> --traces <JSONL> --cloud-usage <JSONL> --outcomes <JSONL> --policy-decisions <JSONL>
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

`bitrouter cloud …` drives the BitRouter Cloud API using the credential persisted by [`bitrouter cloud login`](#bitrouter-cloud-login--logout--whoami). Sign in first, then call a typed management subcommand or the generic API command. Typed subcommands cover the common terminal workflows: namespace inspection, API keys, usage and request history, billing balance and checkout, policies, budgets, presets, and BYOK. Use `bitrouter cloud api <relative-endpoint>` for the rest of the Cloud API surface, including public provider and usage discovery, settlement receipts, routing presets, OAuth clients, billing ledgers, checkout status, and namespace/account lifecycle endpoints.

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

Absolute URLs, scheme-relative paths, fragments, and cross-origin redirects are rejected. Redirect following is disabled, so a stored bearer is never forwarded to another origin. Documented endpoints include `/v1/models`, `/v1/providers`, `/v1/stats/usage`, `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, Google-style `:generateContent` / `:streamGenerateContent` routes under `/v1beta/models/*`, namespace-scoped management routes under `/v1/namespaces/*`, and user-level routes under `/v1/account`, `/v1/billing/*`, and `/v1/byok/*`.

This first release intentionally omits `gh api`'s GraphQL, pagination/slurp, `--jq`, Go templates, cache, hostname, preview, and placeholder expansion features. See the [Cloud API guide](/docs/guides/cloud-api) for copyable requests.

### `bitrouter cloud whoami`

```
bitrouter cloud whoami
```

Prints the cloud identity and the bound namespace alongside the `/v1/*` base URL the CLI will target. Reads the local credentials file only — no network call.

### `bitrouter cloud namespace`

Inspect the workspaces you own and the one this CLI session is baked to. The typed CLI only inspects workspaces; creation and deletion require the Console or `bitrouter cloud api` with the appropriate control-plane scope.

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

Use `bitrouter cloud api /v1/billing/transactions` for the billing ledger, and `/v1/billing/checkout/sessions/<session-id>/status` for checkout status.

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

`bitrouter skills …` inspects Agent Skills — directories containing a `SKILL.md` with YAML frontmatter (`name`, `description`). The agent skills directory is `~/.claude/skills/` with `--global`, or `./.claude/skills/` (project-local) by default.

BitRouter **reads** the installed-skills directory; it does not install into it.
Getting a skill onto disk is the ecosystem's job — `npx skills add`, or the
Claude Code / Codex plugin marketplaces. BitRouter is a skills *server* and
*gateway*, not an installer: see `docs/SKILLS_MCP_SPEC.md` §2.

The `add`, `remove`, `find`, and `update` verbs were removed for that reason.
To serve installed skills over MCP, see `bitrouter mcp serve` (every stdio
profile carries them) or the narrower `--backend skills`.

### `bitrouter skills list`

```
bitrouter skills list [-g|--global]
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

### `bitrouter skills init <name>`

```
bitrouter skills init <NAME> [-o|--output <PATH>]
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
bitrouter acp recordings [--config PATH] list --agent codex-acp
bitrouter acp recordings [--config PATH] show --agent codex-acp NATIVE_SESSION_ID
bitrouter acp recordings [--config PATH] delete --agent codex-acp NATIVE_SESSION_ID
```

The source is the resolved configured agent ID, and the session ID remains
harness-native. `show` returns canonical events, separate replay audit, gaps,
and observed metering/route links. Output follows the global JSON/`--human`
convention. Content persists until explicit deletion; deletion fences further
recording for that native identity and does not touch native harness history
or metering. See [the recording contract](ACP_CANONICAL_CAPTURE_SPEC.md) for
visibility, write-failure, and cost-accounting semantics.
