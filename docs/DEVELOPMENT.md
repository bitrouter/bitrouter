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
| `crates/bitrouter-observe`       | crate   | OpenTelemetry traces + metrics with multi-tenant attribution, exported over OTLP (feature-gated HTTP or gRPC transport)    |
| `crates/bitrouter-tui`           | crate   | Terminal front-end for one ACP agent session (`bitrouter chat`) — the live `view` and its footer, transcript, tool cards, permission prompt, provider picker, cost figure, the `plain` form for a pipe, plus terminal custody (`lifecycle`) and the line editor (`editor`). Synchronous: no async runtime, no I/O of its own |
| `apps/bitrouter`                 | app     | Assembly library + the `bitrouter` CLI binary — turns a `Config` into a running `App` and owns the management commands |

The "plugin" concept lives in the SDK — the `Plugin` trait and the hook traits — not in the directory layout: a hook crate like guardrails or observe is an ordinary library that implements those traits.

### External interfaces

Clients reach BitRouter through four external **interfaces** — the ways *in*. These are distinct from the SDK's four internal *wire-protocol adapters* (Chat Completions / Responses / Messages / Generate Content, described below): an interface is an entry point, an adapter is a dialect the `language_model` pipeline parses and speaks.

| Interface                 | Where it lives                                                                                            | Entry point              |
| ------------------------- | -------------------------------------------------------------------------------------------------------- | ------------------------ |
| **API** (HTTP LLM router) | `bitrouter-sdk` `server` feature (`crates/bitrouter-sdk/src/server.rs`) over the `language_model` pipeline | `bitrouter serve`        |
| **MCP** (origin server)   | `crates/bitrouter-mcp`                                                                                    | `bitrouter mcp serve`    |
| **ACP**                   | `bitrouter-sdk` `acp` feature (`crates/bitrouter-sdk/src/acp/`, `down` / `engine` / `up`); subcommand glue in `apps/bitrouter/src/acp_cli.rs` | `bitrouter acp serve`    |
| **ACP (interactive)**     | `crates/bitrouter-tui` renders what the session emits; the loop and keys are `apps/bitrouter/src/chat/session.rs`; launch and routing stay in `apps/bitrouter/src/acp_cli.rs::chat` | `bitrouter chat`         |

**`bitrouter-tui` must not depend on the `bitrouter` app crate.** That absence
is the boundary, and it is enforced by the build rather than by review: the
renderer draws only what arrives over the ACP wire, so daemon-wide data — the
metering store, the control socket, request history — is unreachable from it
rather than merely unused. Anything it cannot learn from the protocol (the
session log's path, for one) is passed in by the caller. The previous terminal
UI lived inside the application, could reach any function in it, and accreted
verbs with no command-line equivalent until it was deleted; a module boundary
was a promise, this one is a compiler error. It also depends on neither
`bitrouter-sdk` — only the ACP schema types, `ratatui`, and `crossterm`.

The line is drawn on **knowledge, not medium**. Rendering something is never by
itself a reason to keep it in the app, and a method BitRouter is currently
alone in *serving* is still ACP: `providers/list` renders in the crate because
`ProviderInfo` is an `agent-client-protocol-schema` type. What stays in the app
is what the protocol does not carry — the `_meta` key naming a cost figure's
scope (`apps/bitrouter/src/chat/cost.rs`), this process's stdin and signals,
and anything needing `Config` or the control socket. The check is mechanical:
`grep -rn "bitrouter/" crates/bitrouter-tui/src` must return nothing.

Where a control's honesty depends on something ACP does not carry, the crate
takes it as a **parameter** rather than inferring it — `Picker::open` takes
whether the agent serves `providers/*`, `Cost::new` takes whose spend the
figure is — so there is no constructor that skips the question.

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
prints the same data as text (`apps/bitrouter/src/tui/`, now a data layer and a
formatter with no terminal code at all).

The second line, which keeps the crate synchronous, is **meaning vs
transport**. What a key *means* is a terminal fact and lives in the crate
(`editor`: the line editor, `is_cancel`, `is_redraw`). *Owning stdin and
delivering events* is a fact about the host process — what else it selects
over, and which runtime it has — so the pump stays in the app
(`apps/bitrouter/src/chat/input.rs`). The same cut keeps signal handling out
(`crate::tui::lifecycle::Shutdown`) while terminal enter/restore stays in.
`bitrouter-tui` therefore depends on no async runtime at all, and
`cargo tree -p bitrouter-tui | rg -c '^tokio'` printing `0` is how that is
checked.
| **CLI**                   | `apps/bitrouter` — the composition-root binary                                                            | `bitrouter <subcommand>` |

The CLI is the **host** interface: it owns `main()` and mounts the other three as subcommands. That asymmetry is by design — it's why MCP is a standalone crate while both ACP and the API ride inside the SDK, and only the CLI lives in the binary itself.

### Dependency Logic

The layering is strictly one-directional — every library crate points down at **`bitrouter-sdk`**, **`apps`** composes them all, and the SDK never depends back on anything above it:

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
3. **`bitrouter-guardrails`** / **`bitrouter-observe`** — depend on `bitrouter-sdk` only. Hook libraries: they implement the SDK's hook traits and keep their default builds lean. Guardrails never pulls the axum HTTP stack; observe pulls axum/tower-http (for the inbound `TraceLayer`) only under its opt-in `otel-*` features. The `feature-isolation` CI job enforces this.
4. **`apps/bitrouter`** — depends on everything. The assembly layer (`assemble.rs`) turns a parsed `Config` into a running `App` by wiring the builtin hooks (auth, policy, metering, guardrails, observability) onto the `language_model` pipeline; `main.rs` is a thin CLI shell over that library.

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

`bitrouter <subcommand>` — `serve` / `start` / `stop` / `restart` / `reload` / `status` / `route` / `init` / `config` / `key` / `models` / `tools` / `observe` / `policy` / `eval` / `optimize` / `trajectory` / `providers` / `agents` / `launch` / `spawn` / `cloud` / `skills` / `mcp` / `workflow-state` / `update` / `acp`. `start` spawns `serve` detached and the client subcommands talk to it over the control socket. `launch` runs a harness as an interactive native TUI; `spawn` (and its `acp serve|prompt` aliases) runs one as a headless ACP sub-agent. See `apps/bitrouter/src/main.rs`.

### Observability surfaces (`apps/bitrouter/src/tui/`)

One surface, with `render.rs` / `snapshot.rs` as its formatting and data layers:

- `bitrouter status --requests` — the portable settled-request snapshot. It reads the metering store through the snapshot layer and formats the same table for terminals and pipes. There is no live ratatui watcher left in the app; repeat the command with an external `watch -n1` loop when a live refresh is needed.

The hosted mode `bitrouter launch --tui` and its VT emulator (`tui/host.rs`, `tui/term.rs`, `tui/pty.rs`, `tui/conformance.rs`, `tui/fixtures/`) are **deleted**, along with the fidelity matrix that gated them and the `alacritty_terminal` / `portable-pty` / `termwiz` / `wezterm-input-types` dependencies. See [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) for the reasoning and for what replaces it — an inline ACP client backed by the `bitrouter-tui` differential writer rather than a terminal emulator.

`spawn::prepare` still builds the child once and `exec_inherited` runs it; the `Prepared` seam is kept independent of hosting.

## Where To Extend The System

### Add or update a provider

Add a provider definition under `registry/providers/*.yaml` (the registry source; `dist/` is regenerated by `helpers/dist-helper`). `bearer` / `header` auth needs no Rust. For a regional or per-account base URL, use `${VAR}` in `api_base` — it is resolved from the environment at merge time (e.g. Bedrock `https://bedrock-mantle.${AWS_REGION}.api.aws/v1`). For stateful auth (OAuth, token-exchange), add an `AuthApplier` impl in `crates/bitrouter-providers/` keyed by the registry `auth.handler` and register it in `apps/bitrouter/src/assemble.rs::build_auth_appliers` (see `copilot`). See [`CONTRIBUTING.md`](../CONTRIBUTING.md) for the step-by-step.

### Add a new wire protocol

Protocol adapters live in `crates/bitrouter-sdk/src/language_model/protocol/`. A new protocol needs an inbound adapter (parse request / encode response + SSE), an outbound adapter (render request / decode response + SSE), a variant on `ApiProtocol`, dispatch wiring, and coverage in the protocol-conversion test matrix.

### Add a provider whose wire isn't HTTP+JSON+SSE

Rare — no current registry provider needs this. The big clouds (Bedrock, Azure) speak one of BitRouter's built-in protocols over SSE and are ordinary Bearer registry providers. Only if an upstream uses a wire an existing outbound adapter can't decode (e.g. a vendor SDK's binary event-stream) do you add an `ApiProtocol::Custom` outbound adapter + `Transport` in a standalone crate, registered on the dispatch executor at startup. See the `Custom` escape-hatch docs in `crates/bitrouter-sdk/src/language_model/protocol/mod.rs`.

### Add a hook (auth, policy, metering, guardrail, observability)

Implement one of the SDK hook traits (`PreRequestHook`, `RouteHook`, `ExecutionHook`, `StreamHook`, `SettlementRecorder`, `ObserveHook`) and wire it onto the pipeline in `apps/bitrouter/src/assemble.rs`. A hook that brings real dependency weight belongs in its own `crates/` library (the guardrails / observe pattern); a lightweight one can live in the binary.

### Embed the SDK in your own service

`apps/bitrouter/src/assemble.rs` is the worked example: it builds an `App` via `App::builder()`, registers the `language_model` pipeline with a routing table, an executor, and the hook chain, then serves it. A consumer that wants BitRouter's routing + protocol conversion without the stock CLI composes the same builder with its own hooks and routing table.

## Validation

Before opening a change, run the workspace checks from [`CONTRIBUTING.md`](../CONTRIBUTING.md):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

CI additionally runs `doc` (rustdoc under `-D warnings`), `doctest`, `feature-isolation` (default builds of the hook crates stay axum-free), and `msrv` (pinned to Rust 1.93). AI agents should also read [`CLAUDE.md`](../CLAUDE.md).
