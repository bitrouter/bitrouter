# Development Guide

This document is the workspace-level guide for BitRouter internals. Start with [`README.md`](README.md) for the product introduction, then use this guide when you need to understand how the workspace is assembled or how to build on top of the SDK.

## Workspace Architecture

BitRouter is a Cargo workspace with three tiers — `crates/` (the SDK and provider crates), `plugins/` (hook libraries), and `apps/` (the shipped binary):

| Crate                            | Tier    | Responsibility                                                                                                          |
| -------------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------- |
| `crates/bitrouter-sdk`           | crate   | The SDK: three protocol pipelines, hook traits, the four wire-protocol adapters, config loading, and the axum HTTP server |
| `crates/bitrouter-providers`     | crate   | Provider catalog glue: the compiled-in `bitrouter` cloud gateway, the registry fetch/merge, and the `AuthApplier` impls    |
| `plugins/bitrouter-guardrails`   | plugin  | `GuardrailPreHook` (upstream inspection) + `GuardrailStreamHook` (downstream redaction / abort)                           |
| `plugins/bitrouter-observe`      | plugin  | `ObserveHook` implementations — a Prometheus accumulator and an optional OTLP/HTTP JSON span exporter                     |
| `apps/bitrouter`                 | app     | Assembly library + the `bitrouter` CLI/TUI binary — turns a `Config` into a running `App` and owns the management commands |

### Dependency Logic

The layering is strictly one-directional — **`plugins → sdk`**, **`apps → sdk + plugins + providers`**, and the SDK never depends back on anything above it:

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
3. **`bitrouter-guardrails`** / **`bitrouter-observe`** — depend on `bitrouter-sdk` only. Hook libraries: they implement the SDK's hook traits and nothing else. They must **not** pull the axum HTTP stack — the `feature-isolation` CI job enforces this.
4. **`apps/bitrouter`** — depends on everything. The assembly layer (`assemble.rs`) turns a parsed `Config` into a running `App` by wiring the builtin hooks (auth, policy, metering, guardrails, observability) onto the `language_model` pipeline; `main.rs` is a thin CLI/TUI shell over that library.

### SDK feature flags

The SDK keeps its default dependency tree minimal — capabilities that pull weight are feature-gated:

| Feature        | Pulls in                              | Purpose                                                       |
| -------------- | ------------------------------------- | ------------------------------------------------------------- |
| `server`       | axum, tower, tower-http               | The HTTP server, SSE handlers, admin endpoints                |
| `config_file`  | serde-saphyr, `tokio::fs`             | YAML `bitrouter.yaml` loading                                 |
| `mcp`          | rmcp                                  | The bundled `RmcpExecutor` for the `mcp` pipeline             |
| `acp`          | `tokio` process / io-util             | `ConfigAcpRoutingTable` for the pure-routing `acp` pipeline    |

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
5. `ObserveHook`s receive read-only lifecycle events throughout (Prometheus, OTLP).

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
| `GET  /metrics`                     | Prometheus exposition            |
| `GET  /health`                      | health check                    |

Daemon control (`stop` / `restart` / `reload` / `status` / `route`) runs over a Unix domain socket, not HTTP — see `apps/bitrouter/src/daemon.rs`.

## CLI Surface

`bitrouter <subcommand>` — `serve` / `start` / `stop` / `restart` / `reload` / `status` / `route` / `init` / `config` / `key` / `models` / `verify` / `tools` / `observe` / `policy` / `providers` / `agents` / `acp` / `spawn` / `cloud` / `skills` / `mcp` / `update`. `start` spawns `serve` detached and the client subcommands talk to it over the control socket. See `apps/bitrouter/src/main.rs`.

## Where To Extend The System

### Add or update a provider

Add a provider definition under `registry/providers/*.yaml` (the registry source; `dist/` is regenerated by `helpers/dist-helper`). `bearer` / `header` auth needs no Rust. For a regional or per-account base URL, use `${VAR}` in `api_base` — it is resolved from the environment at merge time (e.g. Bedrock `https://bedrock-mantle.${AWS_REGION}.api.aws/v1`). For stateful auth (OAuth, token-exchange), add an `AuthApplier` impl in `crates/bitrouter-providers/` keyed by the registry `auth.handler` and register it in `apps/bitrouter/src/assemble.rs::build_auth_appliers` (see `copilot`). See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the step-by-step.

### Add a new wire protocol

Protocol adapters live in `crates/bitrouter-sdk/src/language_model/protocol/`. A new protocol needs an inbound adapter (parse request / encode response + SSE), an outbound adapter (render request / decode response + SSE), a variant on `ApiProtocol`, dispatch wiring, and coverage in the protocol-conversion test matrix.

### Add a provider whose wire isn't HTTP+JSON+SSE

Rare — no current registry provider needs this. The big clouds (Bedrock, Azure) speak one of BitRouter's built-in protocols over SSE and are ordinary Bearer registry providers. Only if an upstream uses a wire an existing outbound adapter can't decode (e.g. a vendor SDK's binary event-stream) do you add an `ApiProtocol::Custom` outbound adapter + `Transport` in a standalone crate, registered on the dispatch executor at startup. See the `Custom` escape-hatch docs in `crates/bitrouter-sdk/src/language_model/protocol/mod.rs`.

### Add a hook (auth, policy, metering, guardrail, observability)

Implement one of the SDK hook traits (`PreRequestHook`, `RouteHook`, `ExecutionHook`, `StreamHook`, `SettlementRecorder`, `ObserveHook`) and wire it onto the pipeline in `apps/bitrouter/src/assemble.rs`. A hook that brings real dependency weight belongs in its own `plugins/` crate (the guardrails / observe pattern); a lightweight one can live in the binary.

### Embed the SDK in your own service

`apps/bitrouter/src/assemble.rs` is the worked example: it builds an `App` via `App::builder()`, registers the `language_model` pipeline with a routing table, an executor, and the hook chain, then serves it. A consumer that wants BitRouter's routing + protocol conversion without the stock CLI composes the same builder with its own hooks and routing table.

## Validation

Before opening a change, run the workspace checks from [`CONTRIBUTING.md`](CONTRIBUTING.md):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

CI additionally runs `doc` (rustdoc under `-D warnings`), `doctest`, `feature-isolation` (plugins must not pull axum), and `msrv` (pinned to Rust 1.93). AI agents should also read [`CLAUDE.md`](CLAUDE.md).
