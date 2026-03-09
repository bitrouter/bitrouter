# Claude.md

## Project Overview

bitrouter is a modular, trait-based LLM routing system written in Rust. It can be used as:

- **A lightweight local LLM aggregator and proxy** — connect to upstream providers (OpenAI, Anthropic, Google) and expose provider-specific API types, running on your local machine.
- **A high-performance, out-of-the-box web server on the cloud** — deploy the binary for production LLM request proxying with config-driven routing, daemon management, and observability.
- **An SDK to build your own service** — import trait-based core components and API routes as library crates. Write your own implementation at any layer, or re-use service components to plug-and-play.

---

## Crate Dependency Graph

```txt
                        bitrouter-core
                     (traits, models, errors)
                    /       |        |       \
                   /        |        |        \
       bitrouter-openai  bitrouter-anthropic  bitrouter-google   bitrouter-config
       (provider impl)   (provider impl)      (provider impl)   (config, routing)
                   \        |        |        /
                    \       |        |       /
                     bitrouter-api
                 (HTTP filters, routes)
                          |
                    bitrouter-runtime
              (server, router, daemon, paths)
                    /            \
              bitrouter           bitrouter-tui
           (CLI binary)          (terminal UI)
```

### Dependency Logic

The layering follows a strict bottom-up principle — each crate depends only on the layers below it, never sideways or upward:

1. **bitrouter-core** — The foundation. Zero knowledge of HTTP, config files, or any concrete provider. Owns transport-neutral traits (`LanguageModel`, `RoutingTable`, `LanguageModelRouter`), shared model types (prompts, messages, tool schemas, usage stats), and error types. Every other crate depends on this.

2. **Provider crates** (bitrouter-openai, bitrouter-anthropic, bitrouter-google) — Each depends only on `bitrouter-core`. They implement the `LanguageModel` trait for a specific upstream API, handling request/response conversion, streaming, and provider-specific error parsing. Providers are fully independent of each other and of any HTTP framework.

3. **bitrouter-config** — Depends on `bitrouter-core` for routing trait definitions. Owns YAML config parsing, environment variable substitution, built-in provider registry, provider inheritance (`derives`), and the `ConfigRoutingTable` implementation. No knowledge of HTTP or concrete provider SDK types.

4. **bitrouter-api** — Depends on `bitrouter-core` for traits, and optionally on individual provider crates (feature-gated) for API type serialization. Provides reusable Warp HTTP filters for each provider's API surface (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/v1beta/models/`). Filters accept any `RoutingTable + LanguageModelRouter` — they are decoupled from concrete config or provider instantiation.

5. **bitrouter-runtime** — The integration layer. Depends on `bitrouter-core`, `bitrouter-config`, `bitrouter-api`, and all provider crates. Assembles everything: resolves paths, loads config, builds the concrete `Router` (which maps `RoutingTarget` → provider model instances), wires Warp filters into a server, and manages daemon lifecycle.

6. **bitrouter** (binary) — The CLI product. Depends on `bitrouter-runtime` and `bitrouter-config`. Provides the user-facing commands (`serve`, `start`, `stop`, `status`, `restart`) and optional TUI.

7. **bitrouter-tui** — Standalone terminal UI crate (ratatui + crossterm). No dependency on bitrouter internals; receives display data at runtime.

### Why This Layering Matters

- **Provider crates never import each other** — adding or removing a provider has zero impact on other providers.
- **bitrouter-api is reusable without the runtime** — you can import the Warp filters into your own server and supply your own `RoutingTable` + `LanguageModelRouter` implementations.
- **bitrouter-core is HTTP-agnostic** — the `LanguageModel` trait works with any transport, not just Warp. You could build a gRPC or Lambda adapter on top.
- **bitrouter-config is independently useful** — load and resolve config without starting a server.
- **Feature gates on provider API types in bitrouter-api** — only compile the API surface you need.

---

## Key Design Decisions

### 1. Trait-Based Core with Dynamic Dispatch

`bitrouter-core` defines the `LanguageModel` trait using `#[dynosaur]` for object-safe dynamic dispatch. This means:

- Concrete provider types are erased behind `Box<DynLanguageModel>` at runtime.
- The routing and API layers never know which provider they're talking to.
- New providers are added by implementing `LanguageModel` — no changes to routing or API code.

The two routing traits (`RoutingTable` for name → target resolution, `LanguageModelRouter` for target → model instantiation) are similarly trait-based, allowing full replacement of the routing strategy.

### 2. Canonical Intermediate Representation

All providers convert to/from a shared type system in `bitrouter-core`:

- `LanguageModelCallOptions` (request)
- `LanguageModelPrompt` / `LanguageModelMessage` (conversation)
- `LanguageModelGenerateResult` (response)

This means the API layer translates once (HTTP → core types), the provider layer translates once (core types → provider API), and the two are completely independent. Adding a new API surface or a new provider is an isolated change.

### 3. Config-Driven Provider Registry with Inheritance

Built-in provider definitions (OpenAI, Anthropic, Google) are embedded at compile time from YAML files in `bitrouter-config/providers/`. User config merges on top:

- `derives: openai` lets a custom provider inherit all defaults from the built-in OpenAI definition.
- `env_prefix` auto-loads `{PREFIX}_API_KEY` and `{PREFIX}_BASE_URL` from environment variables.
- `${VAR}` substitution works in any YAML string value.

This eliminates boilerplate — a minimal config just needs an API key.

### 4. Routing Strategies

The `ConfigRoutingTable` supports two routing modes:

- **Direct routing**: `"provider:model_id"` (e.g., `"openai:gpt-4o"`) bypasses model lookup and routes directly.
- **Named model routing**: Looks up the model name in the `models` config section, then applies a strategy:
  - `priority` — try endpoints in order, failover on error.
  - `load_balance` — round-robin via atomic counter.

### 5. Reusable HTTP Filters (SDK Mode)

`bitrouter-api` exposes Warp filters as composable building blocks. Each filter:

- Accepts generic `Arc<dyn RoutingTable>` + `Arc<dyn LanguageModelRouter>`.
- Handles deserialization, model routing, generation, and response serialization.
- Is independently mountable — use only the OpenAI-compatible endpoint, or mix and match.

To build your own service, import `bitrouter-api` and supply your own trait implementations. You don't need `bitrouter-runtime` or `bitrouter-config` at all.

### 6. Async-First, Streaming-Native

All core traits return futures and are `Send`-compatible. Streaming responses flow through `tokio::mpsc` channels wrapped in `ReceiverStream`, enabling backpressure-aware SSE delivery to clients. The server uses Warp's streaming body support for zero-copy forwarding.

### 7. Home Directory Convention

The runtime resolves a "home directory" (priority: `--home-dir` > CWD with `bitrouter.yaml` > `$BITROUTER_HOME` > `~/.bitrouter`). All runtime artifacts (config, .env, PID files, logs) live under this directory, with individual path overrides available via CLI flags. This supports both local development (project-local config) and system deployment (`~/.bitrouter`).

### 8. Daemon Lifecycle

`bitrouter start` spawns a detached background process, writing PID to `run/bitrouter.pid`. `stop`/`restart`/`status` operate via this PID file. The spawned daemon always receives `--home-dir <absolute-path>` to ensure path resolution is deterministic regardless of the parent process's working directory.

---

## Usage Modes Mapped to Crates

| Usage Mode                           | Crates You Use                                                                                                            |
| ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------- |
| **Local proxy (full product)**       | `bitrouter` binary — just run it                                                                                          |
| **Cloud server (production deploy)** | `bitrouter` binary with `serve` or `start` — config-driven, daemon-managed                                                |
| **SDK — custom routing only**        | `bitrouter-core` + `bitrouter-api` + provider crates you need. Implement `RoutingTable` + `LanguageModelRouter` yourself. |
| **SDK — custom API surface**         | `bitrouter-core` + provider crates. Build your own HTTP layer using the `LanguageModel` trait directly.                   |
| **SDK — full stack, swap one layer** | `bitrouter-runtime` + swap your own `RoutingTable`, `LanguageModelRouter`, or config loader.                              |
