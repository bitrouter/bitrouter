# Development Guide

This document is the workspace-level guide for BitRouter internals. Start with [`README.md`](../README.md) for the product introduction, then use this guide when you need to understand how the workspace is assembled or how to build on top of the SDK.

## Workspace Architecture

BitRouter is a Cargo workspace with two tiers — `crates/` (the SDK and the library crates built on it) and `apps/` (the shipped binary):

| Crate                            | Tier    | Responsibility                                                                                                          |
| -------------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------- |
| `crates/bitrouter-sdk`           | crate   | The SDK: three protocol pipelines, hook traits, the four wire-protocol adapters, the ACP thin proxy (`acp` feature), config loading, and the axum HTTP server |
| `crates/bitrouter-providers`     | crate   | Provider catalog glue: the compiled-in `bitrouter` cloud gateway, the registry fetch/merge, and the `AuthApplier` impls    |
| `crates/bitrouter-mcp`           | crate   | Origin MCP server — exposes BitRouter's own `complete` / `list_models` / `status` tools over stdio + streamable HTTP        |
| `crates/bitrouter-guardrails`    | crate   | `GuardrailPreHook` (upstream inspection) + `GuardrailStreamHook` (downstream redaction / abort)                           |
| `apps/bitrouter`                 | app     | Assembly library + the `bitrouter` CLI binary — turns a `Config` into a running `App` and owns the management commands |

The "plugin" concept lives in the SDK — the `Plugin` trait and the hook traits — not in the directory layout: a hook crate like guardrails is an ordinary library that implements those traits.

### External interfaces

Clients reach BitRouter through four external **interfaces** — the ways *in*. These are distinct from the SDK's four internal *wire-protocol adapters* (Chat Completions / Responses / Messages / Generate Content, described below): an interface is an entry point, an adapter is a dialect the `language_model` pipeline parses and speaks.

| Interface                 | Where it lives                                                                                            | Entry point              |
| ------------------------- | -------------------------------------------------------------------------------------------------------- | ------------------------ |
| **API** (HTTP LLM router) | `bitrouter-sdk` `server` feature (`crates/bitrouter-sdk/src/server.rs`) over the `language_model` pipeline | `bitrouter serve`        |
| **MCP** (origin server)   | `crates/bitrouter-mcp`                                                                                    | `bitrouter mcp serve`    |
| **ACP**                   | `bitrouter-sdk` `acp` feature (`crates/bitrouter-sdk/src/acp/`, `down` / `engine` / `up`); subcommand glue in `apps/bitrouter/src/acp_cli.rs` | `bitrouter acp serve`    |
| **CLI**                   | `apps/bitrouter` — the composition-root binary                                                            | `bitrouter <subcommand>` |

The CLI is the **host** interface: it owns `main()` and mounts the other three as subcommands. That asymmetry is by design — it's why MCP is a standalone crate while both ACP and the API ride inside the SDK, and only the CLI lives in the binary itself.

### Dependency Logic

The layering is strictly one-directional — every library crate points down at **`bitrouter-sdk`**, **`apps`** composes them all, and the SDK never depends back on anything above it.

Note what that does *not* say: it constrains the direction of dependencies, not how much lives in the SDK. A capability belongs in the SDK when it is an **interop surface** — the SDK's own domain model rendered into an open standard, which must be identical across deployments to mean anything — and it goes behind a default-off feature so consumers who skip it pay nothing. The `otel` feature is the case in point: which span is `chat`, what counts as a hop, when settlement closes, are BitRouter semantics, so OTLP export is SDK work. Deployment business logic (auth, policy, charging, metering, content policy) stays out, whether it would point down cleanly or not.

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
3. **`bitrouter-guardrails`** — depends on `bitrouter-sdk` only. A hook library: it implements the SDK's hook traits and keeps its default build lean, never pulling the axum HTTP stack. The `feature-isolation` CI job enforces this, and also enforces the SDK's own equivalent invariant: the OTel stack it carries under `otel-*` stays out of the default tree, and the two transports stay isolated from each other.
4. **`apps/bitrouter`** — depends on everything. The assembly layer (`assemble.rs`) turns a parsed `Config` into a running `App` by wiring the builtin hooks (auth, policy, metering, guardrails, observability) onto the `language_model` pipeline; `main.rs` is a thin CLI shell over that library.

### SDK feature flags

The SDK keeps its default dependency tree minimal — capabilities that pull weight are feature-gated:

| Feature        | Pulls in                              | Purpose                                                       |
| -------------- | ------------------------------------- | ------------------------------------------------------------- |
| `server`       | axum, tower, tower-http               | The HTTP server, SSE handlers, admin endpoints                |
| `config_file`  | serde-saphyr, `tokio::fs`             | YAML `bitrouter.yaml` loading                                 |
| `mcp`          | rmcp                                  | The bundled `RmcpExecutor` for the `mcp` pipeline             |
| `acp`          | `tokio` process / io-util             | `ConfigAcpRoutingTable` for the pure-routing `acp` pipeline    |
| `otel`         | (selects `otel-http`)                 | OTLP export of the SDK's span / metric model — the entry point |
| `otel-http`    | opentelemetry\*, tracing-opentelemetry, tracing-subscriber, opentelemetry-http, dashmap | The above over OTLP/HTTP + protobuf (reqwest + rustls) |
| `otel-grpc`    | the same stack plus tonic             | The above over OTLP/gRPC (tonic + native trust roots)          |

> **The `otel` features carry a public-dependency commitment.** `otel::subscriber::tracing_subscriber_layer` returns `impl tracing_subscriber::Layer<S>`, so enabling any `otel*` feature makes **tracing-subscriber 0.3** and **tracing-core 0.1** semver-committed public dependencies of the SDK: `tracing_subscriber::Layer`, `tracing_subscriber::registry::LookupSpan`, and `tracing_core::Subscriber` (reached via the `S: tracing::Subscriber` bound) all appear in the public API. A major bump in either crate is a breaking change for SDK consumers. No `opentelemetry*` type is public — see [`OTEL_SDK_MIGRATION_SPEC.md`](OTEL_SDK_MIGRATION_SPEC.md).

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
5. `ObserveHook`s receive read-only lifecycle events throughout; the SDK's `otel` feature turns them into OTLP traces and metrics.

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

`GET /metrics` is retained for endpoint compatibility only. Prometheus accumulation was removed: metrics are now *pushed* over OTLP by the SDK's `otel` feature, and the endpoint serves a short banner pointing at `plugins.bitrouter-observe.otel` (`EmptyMetricsRenderer` in `apps/bitrouter/src/assemble.rs`). The SDK's `MetricsRenderer` trait and its `text/plain; version=0.0.4` content-type default still exist — a deployment that wants a real pull-based endpoint implements the trait itself.

Daemon control (`stop` / `restart` / `reload` / `status` / `route`) runs over a Unix domain socket, not HTTP — see `apps/bitrouter/src/daemon.rs`.

## CLI Surface

`bitrouter <subcommand>` — `serve` / `start` / `stop` / `restart` / `reload` / `status` / `route` / `init` / `config` / `key` / `models` / `tools` / `observe` / `policy` / `eval` / `optimize` / `trajectory` / `providers` / `agents` / `launch` / `spawn` / `cloud` / `skills` / `mcp` / `workflow-state` / `update` / `acp`. `start` spawns `serve` detached and the client subcommands talk to it over the control socket. `launch` runs a harness as an interactive native TUI; `spawn` (and its `acp serve|prompt` aliases) runs one as a headless ACP sub-agent. See `apps/bitrouter/src/main.rs`.

### Observability surfaces (`apps/bitrouter/src/tui/`)

Two surfaces share one renderer and one data layer:

- `bitrouter status --watch` — the live view (`tui/watch.rs`). Unix-only and gated in exactly one place, `#[cfg(unix)] mod watch;`. Piping it prints a single snapshot and exits, which is the path that stays portable.
- `bitrouter launch --tui` — the same readout on a status row pinned under a harness hosted in a VT emulator (`tui/host.rs`, `tui/term.rs`, `tui/pty.rs`).

The emulator exists because harnesses differ on whether they take the alternate screen, and an alt-screen app clobbers a `DECSTBM`-reserved line — so guaranteeing the row means owning the screen. That cost is why `--tui` is opt-in: scrollback moves from the user's terminal to BitRouter.

`spawn::prepare` builds the child once; `exec_inherited` and `exec_hosted` differ only in how it is run, which is what makes "identical env and args" structural rather than a test promise. Terminal-identity env may differ, bounded by `HOSTED_ENV_SET` / `HOSTED_ENV_MAY_ADD` / `HOSTED_ENV_UNSET`.

Verification is layered — input conformance against `cat -v`, replay of recorded harness output, and a manual pass for what needs hands. See [`TUI_FIDELITY_MATRIX.md`](TUI_FIDELITY_MATRIX.md), which also states what the automated layers structurally cannot catch.

## Where To Extend The System

### Add or update a provider

Add a provider definition under `registry/providers/*.yaml` (the registry source; `dist/` is regenerated by `helpers/dist-helper`). `bearer` / `header` auth needs no Rust. For a regional or per-account base URL, use `${VAR}` in `api_base` — it is resolved from the environment at merge time (e.g. Bedrock `https://bedrock-mantle.${AWS_REGION}.api.aws/v1`). For stateful auth (OAuth, token-exchange), add an `AuthApplier` impl in `crates/bitrouter-providers/` keyed by the registry `auth.handler` and register it in `apps/bitrouter/src/assemble.rs::build_auth_appliers` (see `copilot`). See [`CONTRIBUTING.md`](../CONTRIBUTING.md) for the step-by-step.

### Add a new wire protocol

Protocol adapters live in `crates/bitrouter-sdk/src/language_model/protocol/`. A new protocol needs an inbound adapter (parse request / encode response + SSE), an outbound adapter (render request / decode response + SSE), a variant on `ApiProtocol`, dispatch wiring, and coverage in the protocol-conversion test matrix.

### Add a provider whose wire isn't HTTP+JSON+SSE

Rare — no current registry provider needs this. The big clouds (Bedrock, Azure) speak one of BitRouter's built-in protocols over SSE and are ordinary Bearer registry providers. Only if an upstream uses a wire an existing outbound adapter can't decode (e.g. a vendor SDK's binary event-stream) do you add an `ApiProtocol::Custom` outbound adapter + `Transport` in a standalone crate, registered on the dispatch executor at startup. See the `Custom` escape-hatch docs in `crates/bitrouter-sdk/src/language_model/protocol/mod.rs`.

### Add a hook (auth, policy, metering, guardrail, observability)

Implement one of the SDK hook traits (`PreRequestHook`, `RouteHook`, `ExecutionHook`, `StreamHook`, `SettlementRecorder`, `ObserveHook`) and wire it onto the pipeline in `apps/bitrouter/src/assemble.rs`. A hook that brings real dependency weight belongs behind a default-off feature: in its own `crates/` library when it encodes deployment-specific business logic (the guardrails pattern), or in the SDK when it renders the SDK's own domain model into an interop standard (the `otel` pattern). A lightweight one can live in the binary.

### Embed the SDK in your own service

`apps/bitrouter/src/assemble.rs` is the worked example: it builds an `App` via `App::builder()`, registers the `language_model` pipeline with a routing table, an executor, and the hook chain, then serves it. A consumer that wants BitRouter's routing + protocol conversion without the stock CLI composes the same builder with its own hooks and routing table.

## Validation

Before opening a change, run the workspace checks from [`CONTRIBUTING.md`](../CONTRIBUTING.md):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

CI additionally runs `doc` (rustdoc under `-D warnings`), `doctest`, `feature-isolation` (the SDK's default tree stays free of axum and the OTel stack, and the two OTLP transports stay isolated), `sdk-public-api` (see below), and `msrv` (pinned to Rust 1.93). AI agents should also read [`CLAUDE.md`](../CLAUDE.md).

### The `sdk-public-api` job

`bitrouter-sdk` exports OTLP without making the OpenTelemetry version part of its own semver contract, which only holds while no `opentelemetry*` or `tracing_opentelemetry` type appears in any public SDK signature. The job renders the SDK's entire public surface with [`cargo public-api`](https://github.com/cargo-public-api/cargo-public-api) — one fully-qualified line per public item — greps that rendering for forbidden types, and diffs it against the committed baseline at `crates/bitrouter-sdk/public-api.txt`. The baseline half is the broader guard: any change to the SDK's public API, OTel-related or not, shows up in review as a diff instead of slipping through. Full rationale in [`OTEL_SDK_MIGRATION_SPEC.md`](OTEL_SDK_MIGRATION_SPEC.md).

**If this job fails on your PR**, read which step failed. A forbidden-type or re-export failure is a real design problem — keep the OTel type out of the public signature (make it `pub(crate)`, or return `impl Trait`), do not regenerate around it. A baseline failure means your change moved the SDK's public API; if the printed diff is entirely intended, regenerate:

```sh
rustup toolchain install nightly-2026-05-05
cargo install cargo-public-api --locked --version 0.52.0
cargo +nightly-2026-05-05 public-api \
  -p bitrouter-sdk --all-features --simplified > crates/bitrouter-sdk/public-api.txt
```

Both versions are pinned in `crates/bitrouter-sdk/public-api.pins`, which the CI job reads — take them from there rather than from this snippet if the two ever disagree. They are pinned because the baseline is a byte-for-byte diff: rustdoc's JSON format and `cargo public-api`'s rendering each change across releases, so regenerating on a different nightly or a newer tool rewrites the whole file. Run it on Linux or macOS; the SDK has `#[cfg(unix)]` items, so a Windows-generated baseline will not match.
