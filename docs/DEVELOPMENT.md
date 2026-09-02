# Development Guide

This document is the workspace-level guide for BitRouter internals. Start with [`README.md`](../README.md) for the product introduction, then use this guide when you need to understand how the workspace is assembled or how to build on top of the SDK.

## Workspace Architecture

BitRouter is a Cargo workspace with two tiers — `crates/` (the SDK and the library crates built on it) and `apps/` (the shipped binary):

| Crate                            | Tier    | Responsibility                                                                                                          |
| -------------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------- |
| `crates/bitrouter-sdk`           | crate   | The SDK: three protocol pipelines, hook traits, the four wire-protocol adapters, the ACP thin proxy (`acp` feature), config loading, the axum HTTP server, and the observability contract (`observe`) |
| `crates/bitrouter-providers`     | crate   | Provider catalog glue: the compiled-in `bitrouter` cloud gateway, the registry fetch/merge, and the `AuthApplier` impls    |
| `crates/bitrouter-mcp`           | crate   | Origin MCP server — exposes BitRouter's own `complete` / `list_models` / `status` tools over stdio + streamable HTTP, with its billing wire type kept local        |
| `crates/bitrouter-guardrails`    | crate   | `GuardrailPreHook` (upstream inspection) + `GuardrailStreamHook` (downstream redaction / abort)                           |
| `crates/bitrouter-telemetry`     | crate   | Optional telemetry egress: the OTLP exporter (traces + metrics, multi-tenant attribution), the inbound ingress span, and the `tracing` ↔ OTel bridge — all default-off |
| `crates/bitrouter-tui`           | crate   | Terminal renderer for one ACP agent session (`bitrouter chat`) — transcript, tool cards, permission prompt, provider picker, cost line |
| `apps/bitrouter`                 | app     | Assembly library + the `bitrouter` CLI binary — turns a `Config` into a running `App` and owns the management commands |

The "plugin" concept lives in the SDK — the `Plugin` trait and the hook traits — not in the directory layout: a hook crate like guardrails is an ordinary library that implements those traits.

### External interfaces

Clients reach BitRouter through four external **interfaces** — the ways *in*. These are distinct from the SDK's four internal *wire-protocol adapters* (Chat Completions / Responses / Messages / Generate Content, described below): an interface is an entry point, an adapter is a dialect the `language_model` pipeline parses and speaks.

| Interface                 | Where it lives                                                                                            | Entry point              |
| ------------------------- | -------------------------------------------------------------------------------------------------------- | ------------------------ |
| **API** (HTTP LLM router) | `bitrouter-sdk` `server` feature (`crates/bitrouter-sdk/src/server.rs`) over the `language_model` pipeline | `bitrouter serve`        |
| **MCP** (origin server)   | `crates/bitrouter-mcp`                                                                                    | `bitrouter mcp serve`    |
| **ACP**                   | `bitrouter-sdk` `acp` feature (`crates/bitrouter-sdk/src/acp/`): `controller` is the manager-facing server, `client` the one ACP client (transport-generic, driven on the caller's runtime, and the only speaker of `_bitrouter/route/*`), `up` the agent-process transport, `translate` the typed view of `session/update` that `acp prompt`'s NDJSON publishes. There is one stack: `chat`, piped `chat` and `acp prompt` are all consumers of `client`, differing in what they do with the update stream rather than in how they speak ACP. Subcommand glue in `apps/bitrouter/src/acp_cli.rs` | `bitrouter acp serve`    |
| **ACP (interactive)**     | `crates/bitrouter-tui` renders what the session emits; the loop and keys are `apps/bitrouter/src/chat/session.rs`; launch and routing stay in `apps/bitrouter/src/acp_cli.rs::chat` | `bitrouter chat`         |

**`bitrouter-tui` must not depend on the `bitrouter` app crate.** That absence
is the boundary, and Cargo enforces it: the app depends on the crate by path,
so the reverse edge is a cyclic-package error, not a review question. What it
prevents is **reachability** — daemon-wide data (the metering store, the
control socket, request history) is unreachable from the renderer rather than
merely unused. The previous terminal UI lived inside the application, could
reach any function in it, and accreted verbs with no command-line equivalent
until it was deleted; a module boundary was a promise, this one is a compiler
error.

**The ACP-generic charter is retired.** The crate was once forbidden from
naming any BitRouter concept. The check for it began life unanchored — as
`grep -rn "bitrouter/"`, which matched the doc comment that stated it — and was
later tightened to
`rg -n "bitrouter.dev/cost|COST_PROVENANCE_META_KEY" crates/bitrouter-tui/src`.
That tightened form worked, and this change deliberately breaks it: the
cost-provenance wire spelling now lives in `crates/bitrouter-tui/src/cost.rs`
(and is pinned equal to the controller's by a test in `acp_cli.rs`), because
splitting one `_meta` key across a crate boundary bought nothing but two
places to look. This is BitRouter's TUI and may name BitRouter's concepts.
Conforming to ACP is still how the renderer works, and a non-BitRouter agent
still renders — it lands in the honest-default branch of every control.

What has *not* changed is why any of it was chosen. The honesty rules are now
pinned by named tests rather than by an absent dependency: a cost nobody can
vouch for is never drawn as ours (the harness's own figure is labelled the
agent's, an unknown marker is not drawn, and no figure renders `unreported`,
never `$0.00`); a controller that advertises no route control gets no picker
rather than a dead one; a cancelled permission never resolves to consent. Those tests are the guard — deleting one is the change to refuse in
review.

Where a control's honesty depends on something ACP does not carry, the crate
takes it as a **parameter** rather than inferring it — `Picker::open` takes
whether the controller advertised route control, `Cost::new` takes who wrote
the figure — so there is no constructor that skips the question.

The rule these add up to, and it is enforced rather than agreed:

> **`apps/bitrouter` may touch the terminal. Only `bitrouter-tui` may draw on
> it.**

`apps/bitrouter` does not depend on `ratatui`. It cannot construct a widget, so
every drawn thing goes through the renderer crate and the compiler says so —
adding `ratatui` back to that manifest is the change to refuse in review.

What the app keeps is `crossterm`, and that is the rule rather than an exception
to it: crossterm is terminal **I/O** (owning stdin, bracketed paste, reading key
events), which is this process's, while ratatui is **drawing**, which is the
crate's.

The last surface short of this was `status --watch`, a self-refreshing ratatui
table over daemon-wide request rows. It could not move to `bitrouter-tui` —
those rows come from the metering store and cover every caller, most of which
never speak ACP, so importing them would have put a daemon-wide model inside a
session-scoped crate. It was removed instead, and `bitrouter status --requests`
reports the same data through the ordinary CLI report layer
(`apps/bitrouter/src/output/reports/requests.rs`) — JSON by default, `--human`
for the table, and no terminal code anywhere in it.

The second line, which keeps the crate synchronous, is **meaning vs
transport**. What a key *means* is a terminal fact and lives in the crate
(`editor`: the line editor, `is_cancel`, `is_redraw`). *Owning stdin and
delivering events* is a fact about the host process — what else it selects
over, and which runtime it has — so the pump stays in the app
(`apps/bitrouter/src/chat/input.rs`). The same cut keeps signal handling out
(`crate::chat::signals::Shutdown`) while terminal enter/restore stays in.
`bitrouter-tui` therefore depends on no async runtime at all, and
`cargo tree -p bitrouter-tui | rg -c '^tokio'` printing `0` is how that is
checked.
| **CLI**                   | `apps/bitrouter` — the composition-root binary                                                            | `bitrouter <subcommand>` |

The CLI is the **host** interface: it owns `main()` and mounts the other three as subcommands. That asymmetry is by design — it's why MCP is a standalone crate while both ACP and the API ride inside the SDK, and only the CLI lives in the binary itself.

### Dependency Logic

The layering is strictly one-directional — every library crate points down at **`bitrouter-sdk`**, **`apps`** composes them all, and the SDK never depends back on anything above it.

Note what that does *not* say: it constrains the direction of dependencies, not how much lives in the SDK. A capability belongs in the SDK when it is an **interop surface** — a *contract* the SDK's own domain model is rendered into, which must be identical across deployments to mean anything — and it goes behind a default-off feature so consumers who skip it pay nothing. Deployment business logic (auth, policy, charging, metering, content policy) stays out, whether it would point down cleanly or not.

Observability is the case in point, and it is the one the workspace previously got wrong in both directions. The contract is the **span schema** — the span names (`chat`, `route`, `settle`, the per-hop `chat`), the `bitrouter.*` attribute vocabulary, and the invariants that fail silently when a deployment re-derives them wrong (a hop is not a `gen_ai` generation; stamping it as one makes gen_ai-aware backends double-count cost). *That* is BitRouter semantics, it lives in `bitrouter-sdk` as `observe`, and it is behind no feature gate at all, because a deployment implementing the contract must not have to enable a renderer it is not using.

Rendering the contract onto a wire is not the contract. OTLP transport, bearer refresh, batch processing, endpoint configuration and cardinality limiting are one egress path's implementation — by volume, roughly 40% of the old module's production lines were transport and vendor glue and another 17% deployment configuration, against ~43% span semantics — and they ship in `bitrouter-telemetry`. They were briefly folded into the SDK behind a default-off feature; that placement cost the foundation crate two permanent 0.x public dependencies and needed three separate paragraphs of "do not read this as a precedent" to hold its shape. Both are gone. `ObserveHook` is the seam the renderer plugs into, and it is a seam with more than one production implementation: `apps/bitrouter` registers its own observers beside the OTLP one. See [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md).

Because the schema is the contract, it is written down rather than inferred from a renderer's call sites. `crates/bitrouter-sdk/src/observe/schema.rs` declares every span, attribute, event, metric and silent-failure invariant, names no `opentelemetry` type, depends on nothing but `serde`, and renders to the committed artifact `crates/bitrouter-sdk/span-schema.json` — regenerate with `UPDATE_SPAN_SCHEMA=1 cargo test -p bitrouter-sdk committed_artifact` (default features: the module is ungated, and a staleness guard that only fired under `--all-features` would let the artifact rot everywhere else), and the ordinary test run fails when it is stale. It sits beside `public-api-deps.txt`, the crate's other generated manifest — but unlike that one it *ships* with the crate, because it is the interop surface. Every item in the module is `pub` for the same reason: a second renderer needs the declaration at compile time, not just as JSON. Conformance tests in `bitrouter-telemetry`'s `otel/exporter.rs`, `otel/acp.rs` and `otel/http_layer.rs` drive real lifecycles and assert that nothing reaches the wire the declaration does not describe, so the artifact is checked rather than aspirational. See [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md).

1. **`bitrouter-sdk`** — the foundation. Knows nothing about which providers exist or how the binary is wired. It owns:
   - **Three independent pipelines**, one per wire family. They are deliberately *not* generic over a shared hook trait — each has its own hook set:
     - `language_model` — the main pipeline: LLM completions with the full hook chain (pre-request → route → execute → settle), an interleaved stream stage, and read-only observation.
     - `mcp` — Model Context Protocol routing (pure routing, no settlement).
     - `acp` — Agent Client Protocol routing (pure routing, no settlement).
   - **Four wire-protocol adapters** — Chat Completions, Responses, Messages, Generate Content — each with an inbound side (parse a client request / encode a client response + SSE) and an outbound side (render a provider request / decode a provider response + SSE). Any inbound protocol can be served by any outbound protocol.
   - **Hook traits** — `PreRequestHook`, `RouteHook`, `ExecutionHook`, `StreamHook`, `SettlementRecorder`, `ObserveHook` — the extension points every plugin and the binary's builtin hooks implement.
   - **Config + routing** — YAML parsing, `${VAR}` substitution, the `ConfigRoutingTable`.
   - The **axum HTTP server** and the `App` builder.
2. **`bitrouter-providers`** — depends on `bitrouter-sdk`. Provider integration glue. The only compiled-in provider entry is the hosted `bitrouter` cloud gateway (`providers/bitrouter.toml`, embedded via `include_str!`); every other provider comes from the runtime-fetched registry and is merged by `registry::apply`. Owns the `AuthApplier` impls (copilot, anthropic, claude-code, openai-codex) and `zero_config()` — the in-memory `Config` used when the binary runs with no config file.
3. **`bitrouter-guardrails`** / **`bitrouter-telemetry`** — depend on `bitrouter-sdk` only. Hook libraries: they implement the SDK's hook traits and keep their default builds lean. Guardrails never pulls the axum HTTP stack; telemetry's whole OpenTelemetry stack sits behind `otel-*` and its ingress span behind `server`, so `cargo add bitrouter-telemetry` on its own pulls neither. The `feature-isolation` CI job enforces all of it, plus the invariant that gives the split its point: **no `opentelemetry*` crate is in `bitrouter-sdk`'s tree at any feature combination**, and the two OTLP transports stay isolated from each other.
4. **`apps/bitrouter`** — depends on everything. The assembly layer (`assemble.rs`) turns a parsed `Config` into a running `App` by wiring the builtin hooks (auth, policy, metering, guardrails, observability) onto the `language_model` pipeline; `main.rs` is a thin CLI shell over that library.

### SDK feature flags

The SDK keeps its default dependency tree minimal — capabilities that pull weight are feature-gated:

| Feature        | Pulls in                              | Purpose                                                       |
| -------------- | ------------------------------------- | ------------------------------------------------------------- |
| `server`       | axum, tower                           | The HTTP server, SSE handlers, admin endpoints                |
| `config_file`  | serde-saphyr, `tokio::fs`             | YAML `bitrouter.yaml` loading                                 |
| `mcp`          | rmcp                                  | The bundled `RmcpExecutor` for the `mcp` pipeline             |
| `acp`          | `tokio` process / io-util             | `ConfigAcpRoutingTable` for the pure-routing `acp` pipeline, plus the live thin proxy (`up` / `engine` / `down`) |

> **`acp` links an HTTP server, and that is a known wart rather than a design choice.** It pulls `agent-client-protocol-conductor` (the controller kernel), which depends on `agent-client-protocol-trace-viewer`, which depends on **`axum` non-optionally**, with no feature to switch it off. An `acp-controller` split was built and withdrawn: it failed this workspace's own "name the beneficiary" test, since `apps/bitrouter` is the only consumer of `acp` and it wants the controller. The one real victim was `helpers/dist-helper`, which enabled `acp` without using it — trimming that is the fix that shipped, and `feature-isolation` keeps `dist-helper` free of both `axum` and `opentelemetry`. Splitting the feature now would also mean restructuring `acp::controller`, since `acp::client` imports its route-control types. The real fix is upstream making the trace viewer optional; revisit when a consumer that wants ACP routing without a controller actually exists.

`observe` — the span schema and the `SpanAttributes` hatch — is **not** in this table: it is ungated and carries no dependency beyond `serde`.

### `bitrouter-telemetry` feature flags

| Feature        | Pulls in                              | Purpose                                                       |
| -------------- | ------------------------------------- | ------------------------------------------------------------- |
| `otel`         | (selects `otel-http`)                 | OTLP export of the span / metric model — the entry point       |
| `otel-http`    | opentelemetry\*, tracing-opentelemetry, tracing-subscriber, opentelemetry-http, dashmap | The above over OTLP/HTTP + protobuf (reqwest + rustls) |
| `otel-grpc`    | the same stack plus tonic             | The above over OTLP/gRPC (tonic + native trust roots)          |
| `server`       | axum, http-body, pin-project-lite, `bitrouter-sdk/server` | The inbound ingress SERVER span, as a middleware over the SDK's router |

> **The public-dependency commitment lives with the renderer, and that is the point of the split.** `otel::subscriber::tracing_subscriber_layer` returns `impl tracing_subscriber::Layer<S>`, so **tracing-subscriber 0.3** and **tracing-core 0.1** are semver-committed public dependencies — of `bitrouter-telemetry`, which is default-off and which nothing else in the workspace links. They were public dependencies of `bitrouter-sdk` for one release cycle, where a `tracing-subscriber` 0.4 would have forced a breaking release on every consumer including the five that never enabled `otel`. `crates/bitrouter-sdk/public-api-deps.txt` records the removal. `server` is split out because the two halves have different consumers: a deployment that builds its own ingress span and only wants the bridge should not compile axum for it — the out-of-tree consumer is exactly that, since it installs its own `TraceLayer` so a public multi-tenant edge does not let callers control its trace ids or sampling.

Without `mcp` / `acp`, the SDK still exposes those pipelines, hook traits, and transport enums — a consumer can plug in a custom `Executor` without pulling rmcp or the stdio bridge.

> **Rule of thumb:** a feature exists only if disabling it removes a non-trivial set of dependencies. Pure module-visibility toggles are not features — the module is always compiled.

## Request Flow

A streaming LLM request moves through the workspace like this:

1. The `bitrouter` binary resolves the config source (see *Configuration*), loads or synthesises a `Config`, and `assemble.rs` builds an `App` — the `language_model` pipeline with the builtin hooks wired on.
2. The SDK's axum server receives the inbound HTTP request on one of the protocol routes and the matching **inbound adapter** parses it into a canonical `PipelineRequest` (model name, messages, tools, params).
3. The `language_model` pipeline runs its stages:
   - **Pre-request** — every `PreRequestHook` in order: auth, policy, guardrail inspection.
   - **Route** — the `RoutingTable` resolves the model name to a fallback chain of `RoutingTarget`s (provider + upstream model id + protocol); `RouteHook`s may rewrite the chain.
   - **Execute** — the executor dials the first target; on failure the `FallbackPolicy` decides whether to try the next. The **outbound adapter** for the target's protocol renders the provider request and decodes the provider response (and its SSE stream).
   - **Settlement** — every `SettlementRecorder` runs (metering, etc.), success or failure.
4. For streaming, the canonical `StreamPart` stream flows through the `StreamHook` stage and is re-encoded by the inbound adapter into the **client's** protocol — so a client written for the Responses protocol can transparently use a Messages upstream, and vice versa.
5. `ObserveHook`s receive read-only lifecycle events throughout; `bitrouter-telemetry` turns them into OTLP traces and metrics, and the binary's own observers consume the same events.

The `mcp` and `acp` pipelines are simpler: pure routing with no settlement.

## Configuration Model

### Config source resolution

When a subcommand doesn't pass `-c <path>`, the binary walks a fixed order (see `apps/bitrouter/src/paths.rs`):

1. **`-c <path>`** — explicit; a missing file is a hard error.
2. **`./bitrouter.yaml`** in the current directory.
3. **`$BITROUTER_HOME/bitrouter.yaml`** — if the env var is set, that file must exist.
4. **`~/.bitrouter/bitrouter.yaml`** — used if present.
5. **Zero-config in-memory defaults** — when nothing above exists. No file is written; `bitrouter init` is the explicit way to scaffold one.

The daemon `chdir`s into the bitrouter home (the config file's directory, or `~/.bitrouter` for zero-config) on startup, so relative paths in the config — `database.url`, `server.control_socket` — and the socket / pid / log all resolve against one stable location.

### Zero-config and the provider catalog

In zero-config mode `bitrouter_providers::zero_config()` builds a `Config` with `skip_auth: true`, `listen: 127.0.0.1:4356`, and the compiled-in hosted gateway auto-enabled when its API key is set in the environment. Every other public provider comes from the fetched-or-cached registry merge: an env-keyed registry provider becomes active when its credential is available, and a local-OAuth provider becomes active after `bitrouter providers login <provider>`.

## HTTP Server Surface

The axum server lives behind the SDK's `server` feature (`crates/bitrouter-sdk/src/server.rs`):

| Route                               | Handler                          |
| ----------------------------------- | -------------------------------- |
| `POST /v1/chat/completions`         | Chat Completions inbound         |
| `POST /v1/responses`                | Responses inbound                |
| `POST /v1/messages`                 | Messages inbound                 |
| `POST /v1beta/models/{model_action}`| Generate Content inbound         |
| `GET  /v1/models`                   | model catalog listing            |
| `POST /mcp/{server}`                | MCP gateway (JSON-RPC proxy)     |
| `GET  /metrics`                     | OTLP-migration banner (see below)|
| `GET  /health`                      | health check                    |

`GET /metrics` is retained for endpoint compatibility only. Prometheus accumulation was removed: metrics are now *pushed* over OTLP by `bitrouter-telemetry`, and the endpoint serves a short banner pointing at `plugins.bitrouter-telemetry.otel` (`EmptyMetricsRenderer` in `apps/bitrouter/src/assemble.rs`). The SDK's `MetricsRenderer` trait and its `text/plain; version=0.0.4` content-type default still exist — a deployment that wants a real pull-based endpoint implements the trait itself.

Daemon control (`stop` / `restart` / `reload` / `status` / `route`) runs over a Unix domain socket, not HTTP — see `apps/bitrouter/src/daemon.rs`.

## CLI Surface

`bitrouter <subcommand>` — `serve` / `start` / `stop` / `restart` / `reload` / `status` / `route` / `init` / `config` / `key` / `models` / `tools` / `observe` / `policy` / `eval` / `optimize` / `trajectory` / `providers` / `agents` / `launch` / `spawn` / `cloud` / `skills` / `mcp` / `workflow-state` / `update` / `acp`. `start` spawns `serve` detached and the client subcommands talk to it over the control socket. `launch` runs a harness as an interactive native TUI; `spawn` (and its `acp serve|prompt` aliases) runs one as a headless ACP sub-agent. See `apps/bitrouter/src/main.rs`.

### Observability surface (`bitrouter status --requests`)

One surface, and no module of its own. `RequestsReport`
(`apps/bitrouter/src/output/reports/requests.rs`) is an ordinary `CliReport`:
the builder in `main.rs` polls the metering store and the control socket, and
`Human::table` / `Human::status_block` render it. There is no second table
implementation and no terminal-only path.

The `apps/bitrouter/src/tui/` module it replaced is **deleted**. It had become a
618-line private implementation of one public function that bypassed `Output`
(so `--json` was silently ignored) and reimplemented `output/human.rs`'s table.
Its signal arm moved to `chat/signals.rs`, next to its only caller.

`apps/bitrouter` no longer depends on `ratatui`; `crates/bitrouter-tui` owns
drawn terminal UI for ACP chat.

The hosted mode `bitrouter launch --tui` and its VT emulator (`tui/host.rs`, `tui/term.rs`, `tui/pty.rs`, `tui/conformance.rs`, `tui/fixtures/`) are **deleted**, along with the fidelity matrix that gated them and the `alacritty_terminal` / `portable-pty` / `termwiz` / `wezterm-input-types` dependencies. See [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) for the reasoning and for what replaces it — an inline-viewport ACP client rather than a terminal emulator.

`spawn::prepare` still builds the child once and `exec_inherited` runs it; the `Prepared` seam is kept independent of hosting.

## Where To Extend The System

### Add or update a provider

Add a provider definition under `registry/providers/*.yaml` (the registry source; `dist/` is regenerated by `helpers/dist-helper`). `bearer` / `header` auth needs no Rust. For a regional or per-account base URL, use `${VAR}` in `api_base` — it is resolved from the environment at merge time (e.g. Bedrock `https://bedrock-mantle.${AWS_REGION}.api.aws/v1`). For stateful auth (OAuth, token-exchange), add an `AuthApplier` impl in `crates/bitrouter-providers/` keyed by the registry `auth.handler` and register it in `apps/bitrouter/src/assemble.rs::build_auth_appliers` (see `copilot`). See [`CONTRIBUTING.md`](../CONTRIBUTING.md) for the step-by-step.

### Add a new wire protocol

Protocol adapters live in `crates/bitrouter-sdk/src/language_model/protocol/`. A new protocol needs an inbound adapter (parse request / encode response + SSE), an outbound adapter (render request / decode response + SSE), a variant on `ApiProtocol`, dispatch wiring, and coverage in the protocol-conversion test matrix.

### Add a provider whose wire isn't HTTP+JSON+SSE

Rare — no current registry provider needs this. The big clouds (Bedrock, Azure) speak one of BitRouter's built-in protocols over SSE and are ordinary Bearer registry providers. Only if an upstream uses a wire an existing outbound adapter can't decode (e.g. a vendor SDK's binary event-stream) do you add an `ApiProtocol::Custom` outbound adapter + `Transport` in a standalone crate, registered on the dispatch executor at startup. See the `Custom` escape-hatch docs in `crates/bitrouter-sdk/src/language_model/protocol/mod.rs`.

### Add a hook (auth, policy, metering, guardrail, observability)

Implement one of the SDK hook traits (`PreRequestHook`, `RouteHook`, `ExecutionHook`, `StreamHook`, `SettlementRecorder`, `ObserveHook`) and wire it onto the pipeline in `apps/bitrouter/src/assemble.rs`. A hook that brings real dependency weight belongs in its own `crates/` library behind a default-off feature — the guardrails and telemetry pattern. What goes in the SDK is the *contract* such a hook binds to, not the hook: `observe::schema` is in the SDK, its OTLP renderer is not. A lightweight hook can live in the binary.

### Embed the SDK in your own service

`apps/bitrouter/src/assemble.rs` is the worked example: it builds an `App` via `App::builder()`, registers the `language_model` pipeline with a routing table, an executor, and the hook chain, then serves it. A consumer that wants BitRouter's routing + protocol conversion without the stock CLI composes the same builder with its own hooks and routing table.

## Validation

Before opening a change, run the workspace checks from [`CONTRIBUTING.md`](../CONTRIBUTING.md):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

CI additionally runs `doc` (rustdoc under `-D warnings`), `doctest`, `feature-isolation` (the SDK's default tree stays free of axum, the OTel stack never reaches the SDK at any feature combination, and the two OTLP transports stay isolated), `sdk-public-api` (see below), and `msrv` (pinned to Rust 1.93). AI agents should also read [`CLAUDE.md`](../CLAUDE.md).

### The `sdk-public-api` job

`bitrouter-sdk` owns the observability contract without owning a renderer of it, which only holds while no `opentelemetry*` or `tracing_opentelemetry` type appears in any public SDK signature. `feature-isolation` now proves the stronger form — the stack is not in the SDK's tree at all — so this job is the *forward* guard: it is what catches the next change that pulls a renderer back in. The job renders the SDK's entire public surface with [`cargo public-api`](https://github.com/cargo-public-api/cargo-public-api) — one fully-qualified line per public item — greps that rendering for forbidden types, then reduces it to the set of foreign crates the public API reaches and diffs that against `crates/bitrouter-sdk/public-api-deps.txt`. That second half is the broader guard: a grep can only find a type you already know to look for, whereas the manifest catches the SDK quietly gaining a *new* public dependency — which is how `tracing_core` got there, through a generic bound. Full rationale in [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md).

**If this job fails on your PR**, read which step failed. A forbidden-type or re-export failure is a real design problem — keep the OTel type out of the public signature (make it `pub(crate)`, or return `impl Trait`), do not regenerate around it. A public-dependency failure means your change added or removed a crate from the SDK's public API; a `+` line means an upstream breaking release in that crate now becomes a BitRouter breaking release, so confirm that is intended before regenerating:

```sh
rustup toolchain install nightly-2026-05-05
cargo install cargo-public-api --locked --version 0.52.0
cargo +nightly-2026-05-05 public-api \
  -p bitrouter-sdk --all-features --simplified \
  | grep -oE '\b[a-z_][a-z0-9_]*(::[a-zA-Z_][a-zA-Z0-9_]*)+' \
  | cut -d: -f1 | sort -u \
  | grep -vxE 'bitrouter_sdk|core|std|alloc'
```

Append the output under the comment header in `public-api-deps.txt` — the header is stripped before comparison, so keep it out of the generated part. Both versions are pinned in `crates/bitrouter-sdk/public-api.pins`, which the CI job reads; take them from there rather than from this snippet if the two ever disagree. They are pinned because the manifest is derived from `cargo public-api`'s rendering, and both rustdoc's JSON format and that rendering change across releases. Run it on Linux or macOS; the SDK has `#[cfg(unix)]` items, so a Windows-generated listing will not match.
