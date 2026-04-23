# Development Guide

This document is the workspace-level guide for BitRouter internals. Start with [`README.md`](README.md) for the product introduction, then use this guide when you need to understand how the workspace is assembled or how to build on top of its reusable server components.

## Workspace Architecture

BitRouter is organized as a set of focused crates:

| Crate                  | Responsibility                                                                                                                                    |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `bitrouter`            | CLI entry point with setup wizard, auto-init on first run, and runtime launch                                                                     |
| `bitrouter-api`        | Reusable Warp filters for provider-compatible HTTP endpoints and the MCP gateway                                                                  |
| `bitrouter-config`     | YAML loading, `.env` support, environment substitution, built-in providers (models and tools), config-backed routing, and config generation        |
| `bitrouter-core`       | Shared traits for models and tools, router contracts, errors, and transport-neutral types                                                          |
| `bitrouter-providers`  | Feature-gated provider adapters (OpenAI, Anthropic, Google), MCP client, Agent Skills registry, and REST tool provider                            |
| `bitrouter-accounts`   | Account and session management backed by sea-orm                                                                                                  |
| `bitrouter-observe`    | Spend tracking, metrics collection, and request observation for model and tool invocations                                                        |
| `bitrouter-blob`       | Concrete `BlobStore` implementations (filesystem backend)                                                                                         |
| `bitrouter-guardrails` | Local firewall for AI agent traffic — pattern-based content inspection with warn, redact, and block actions                                       |
| `bitrouter-tui`        | Terminal UI with ACP (Agent Client Protocol) integration for managing coding agent sessions in real time                                              |

### Dependency Logic

The layering follows a strict bottom-up principle — each crate depends only on the layers below it, never sideways or upward:

1. **bitrouter-core** — The foundation. Zero knowledge of HTTP, config files, or any concrete provider. Owns transport-neutral traits for both models (`LanguageModel`, `LanguageModelRouter`) and tools (`ToolProvider`, `ToolRouter`, `ToolRegistry`), shared routing traits (`RoutingTable`), model types (prompts, messages, tool schemas, usage stats), tool types (`ToolDefinition`, `ToolCallResult`), and error types. Every other crate depends on this.
2. **bitrouter-providers** — Depends on `bitrouter-core`. Contains feature-gated provider adapters for upstream APIs (OpenAI, Anthropic, Google) implementing the `LanguageModel` trait, plus protocol clients for MCP (`McpTransport`, `ConfigMcpRegistry`) and Agent Skills (`FilesystemSkillRegistry`), and a generic REST tool provider. Adapters are independent of each other and of any HTTP framework.
3. **bitrouter-config** — Depends on `bitrouter-core` for routing trait definitions. Owns YAML config parsing, environment variable substitution, built-in provider registry (for both models and tools), provider inheritance (`derives`), the `ConfigRoutingTable` for models, and the `ConfigToolRoutingTable` for tools. Built-in tool providers live alongside model providers under `providers/tools/`. No knowledge of HTTP or concrete provider SDK types.
4. **bitrouter-api** — Depends on `bitrouter-core` for traits, and optionally on `bitrouter-providers` (feature-gated) for API type serialization. Provides reusable Warp HTTP filters for each provider's API surface (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/v1beta/models/`) and the MCP gateway (`/mcp/{name}`). Filters accept any `RoutingTable + LanguageModelRouter` — they are decoupled from concrete config or provider instantiation.
5. **bitrouter-accounts** — Depends on `bitrouter-core` for server contract types. Provides account and session management backed by sea-orm: entity types (`Account`, `ApiKey`, `Session`, `Message`), database migrations via `Migrator`, `AccountService` / `SessionService` for data operations, and Warp filter builders parameterized by a caller-supplied auth filter. This crate does **not** implement authentication — callers provide a Warp filter that extracts an `Identity`, decoupling auth strategy from account logic.
6. **bitrouter-observe** — Depends on `bitrouter-core` for observation callback traits. Provides spend tracking, metrics collection, and request observation for both model and tool invocations.
7. **bitrouter-blob** — Depends on `bitrouter-core` for the `BlobStore` trait. Provides concrete blob storage backends (filesystem via the `fs` feature).
8. **bitrouter-guardrails** — Depends on `bitrouter-core`. Local firewall for AI agent traffic — pattern-based content inspection with warn, redact, and block actions for both model and tool requests.
9. **bitrouter-tui** — Standalone TUI crate. Depends on `agent-client-protocol` for ACP integration and `ratatui`/`crossterm` for rendering. Provides the terminal UI for managing coding agent sessions via the Agent Client Protocol (JSON-RPC over stdio). Auto-discovers ACP-compatible agents on PATH and communicates with them on a dedicated thread using `LocalSet` (ACP types are `!Send`).
10. **bitrouter** (binary) — The CLI product. Depends on all workspace crates. Assembles everything: resolves paths, loads config, and provides the user-facing commands (`serve`, `start`, `stop`, `status`, `restart`) and optional TUI.

### Crate-creation rule

> **Introduce a new `bitrouter-*` crate only if a feature plugged into the SDK would grow the SDK's dependency tree with new heavyweight integration crates.** Otherwise, the feature lives inside `bitrouter-api` (or `bitrouter-core`) behind a feature flag.

The rule keeps the workspace small by default and pushes new integrations into companion crates only when they bring real dependency weight. How current crates satisfy it:

- `bitrouter-accounts` and `bitrouter-observe` — both pull `sea-orm` + `sqlx` drivers; gating them as features on `bitrouter-api` would force every embedder to compile sea-orm.
- `bitrouter-providers` — pulls provider SDKs and protocol clients (`rmcp`, ACP archive readers) that should not be paid for by SDK consumers who don't use them.
- `bitrouter-guardrails` — kept as its own crate because the firewall is a distinct concern with room to grow its own dep surface (richer pattern engines, ML-based detectors, remote rule sources).
- `bitrouter-blob` — kept as its own crate in anticipation of additional backends (`s3`, `gcs`, …) that will pull large SDK trees; avoids moving code in and out of the workspace.
- `bitrouter-tui` — pulls `ratatui`, `crossterm`, and the ACP stack; clearly its own crate.
- `bitrouter-config` — owns YAML loading and the built-in provider registry; not a heavy integration but a natural seam between transport-neutral types and runtime composition.

A feature on `bitrouter-api` that would satisfy this rule today: anything pulling a new optional dep on a companion crate (e.g. `bitrouter-accounts`, `bitrouter-observe`, `bitrouter-guardrails`).

### Feature rule

> **A feature exists if and only if disabling it removes a non-trivial set of dependencies from the build.** Pure module-visibility toggles are not features — the module is always compiled.

The rule applies equally to the `bitrouter` binary and every library crate. Rationale:

- Features that don't shrink the dep tree only add `cfg` noise without delivering smaller binaries or faster builds.
- A feature must therefore correspond to either an `optional` dependency, a sub-tree selected through a transitive crate's features, or a meaningfully different set of system-level deps.
- Sub-flag choices that swap between mutually exclusive sub-trees (e.g. `sqlx-sqlite` vs `sqlx-postgres`, or chain-specific MPP signing stacks) satisfy the rule and remain valid features even when always-on at the top level.

In `bitrouter` (the binary), features describe **bundles of capability** the user opts into (for example `tui`) rather than backend toggles. Capabilities considered core to the product should not be feature-gated at the binary level: they are part of every build regardless of feature selection. Backend choices that pick between mutually exclusive sub-trees (e.g. database driver, payment chain) remain features, but are always-on as defaults.

## Request Flow

### Model requests

A typical model request moves through the workspace in this order:

1. The CLI resolves runtime paths and applies any per-path overrides.
2. `AppRuntime::load` reads `bitrouter.yaml`, optionally loads `.env`, substitutes `${VAR}` values, merges built-in provider definitions, and builds a `ConfigRoutingTable`.
3. The runtime router receives provider configs and knows how to instantiate concrete provider-backed language models.
4. The server plan wires reusable filters from `bitrouter-api` into a Warp server.
5. Each API filter asks a `RoutingTable` to resolve the incoming model name, then asks a `LanguageModelRouter` to construct the concrete model implementation.
6. Provider adapters in `bitrouter-providers` translate between BitRouter's internal request/response types and the upstream provider API.

### Tool requests

Tool routing follows a parallel path:

1. Config loading merges built-in tool provider definitions (`providers/tools/*.yaml`) with user-defined `tools:` entries and builds a `ConfigToolRoutingTable`.
2. Tool calls arrive via the MCP gateway (`POST /mcp`) or through LLM-initiated tool use in a model request.
3. The `ToolRouter` resolves the tool name to a `RoutingTarget` (provider + tool ID + protocol) and constructs the appropriate `ToolProvider` (MCP client, REST client, etc.).
4. The `ToolProvider` executes the call and returns a protocol-neutral `ToolCallResult`.
5. The `ToolRegistry` provides discovery — listing available tools across all providers for agent consumption.

## Configuration Model

### Runtime paths

`bitrouter/src/runtime/paths.rs` is the source of truth for BitRouter's home-directory behavior.

Resolution order:

1. `--home-dir`
2. current working directory when `bitrouter.yaml` is present
3. `BITROUTER_HOME` when it points to an existing directory
4. `~/.bitrouter`

Derived paths are:

- `<home>/bitrouter.yaml`
- `<home>/.env`
- `<home>/run`
- `<home>/logs`

Those derived paths can then be overridden individually with `--config-file`, `--env-file`, `--run-dir`, and `--logs-dir`.

### Providers and routing

`bitrouter-config` is responsible for turning config files into a usable routing layer for both models and tools.

- Built-in model providers live in [`bitrouter-config/providers/models`](bitrouter-config/providers/models) as YAML files embedded at compile time.
- Built-in tool providers live in [`bitrouter-config/providers/tools`](bitrouter-config/providers/tools) as YAML files embedded at compile time.
- `BitrouterConfig::load_from_file` merges user-defined providers on top of those built-ins.
- Provider `derives` chains are resolved before the runtime starts serving traffic.
- `env_prefix` automatically maps environment variables such as `OPENAI_API_KEY` and `OPENAI_BASE_URL` onto a provider config.
- `ConfigRoutingTable` (models) supports two routing modes:
  - direct routing with `provider:model_id`
  - model aliases defined under `models` using `priority` or `load_balance`
- `ConfigToolRoutingTable` (tools) supports three routing modes:
  - direct routing with `provider:tool_id`
  - tool aliases defined under `tools` using `priority` or `load_balance`
  - namespaced format with `provider/tool_id`

## HTTP Server Surface

The reusable HTTP filters live in `bitrouter-api`.

### Filters currently provided

| Route                               | Crate module                                               |
| ----------------------------------- | ---------------------------------------------------------- |
| `GET /health`                       | `bitrouter::runtime::ServerPlan`                           |
| `POST /v1/chat/completions`         | `bitrouter_api::router::openai::chat::filters`             |
| `POST /v1/responses`                | `bitrouter_api::router::openai::responses::filters`        |
| `POST /v1/messages`                 | `bitrouter_api::router::anthropic::messages::filters`      |
| `POST /v1beta/models/:model_action` | `bitrouter_api::router::google::generate_content::filters` |
| `POST /mcp`                         | `bitrouter_api::router::mcp::filters`                      |
| `GET /mcp/sse`                      | `bitrouter_api::router::mcp::filters`                      |
| `POST /mcp/{name}`                  | `bitrouter_api::router::mcp::filters` (per-server bridge)  |
| `GET /mcp/{name}/sse`               | `bitrouter_api::router::mcp::filters` (per-server bridge)  |

The server plan wires all routes into the default runtime server. All filters are also independently importable from `bitrouter-api` for custom service composition.

## Building Your Own Router Service

If you want BitRouter's routing and provider adapters without the stock CLI/runtime entry point, you can compose your own Warp server from the reusable parts.

### Option 1: reuse the existing config and provider router

This is the shortest path when you want BitRouter-compatible config loading and provider instantiation:

```rust
use std::sync::Arc;

use bitrouter_api::router::{anthropic, google, openai};
use bitrouter_config::{BitrouterConfig, ConfigRoutingTable};
use warp::Filter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = BitrouterConfig::load_from_file("bitrouter.yaml".as_ref(), Some(".env".as_ref()))?;

    let table = Arc::new(ConfigRoutingTable::new(
        config.providers.clone(),
        config.models.clone(),
    ));
    // Build a model router from provider configs.
    // Router::new takes a reqwest_middleware::ClientWithMiddleware.
    let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
    let router = Arc::new(crate::runtime::Router::new(client, config.providers.clone()));

    let health = warp::path("health")
        .and(warp::get())
        .map(|| warp::reply::json(&serde_json::json!({ "status": "ok" })));

    let routes = health
        .or(openai::chat::filters::chat_completions_filter(table.clone(), router.clone()))
        .or(openai::responses::filters::responses_filter(table.clone(), router.clone()))
        .or(anthropic::messages::filters::messages_filter(table.clone(), router.clone()))
        .or(google::generate_content::filters::generate_content_filter(
            table.clone(),
            router.clone(),
        ))
        .recover(openai::chat::filters::rejection_handler)
        .with(warp::trace::request());

    warp::serve(routes).run(config.server.listen).await;
    Ok(())
}
```

### Option 2: bring your own routing table or router

`bitrouter-api` depends only on the contracts from `bitrouter-core`:

- `RoutingTable` resolves an incoming name into a provider name plus upstream service ID.
- `LanguageModelRouter` constructs a concrete model implementation for that target.
- `ToolRouter` constructs a concrete tool provider for a tool routing target.

If you already have your own config system or provider registry, implement those traits and pass your types into the existing filters.

## Where To Extend The System

### Add or update built-in model providers

- Model provider definitions: `bitrouter-config/providers/models/*.yaml`
- Built-in registry wiring: `bitrouter-config/src/registry.rs`
- Provider config schema: `bitrouter-config/src/config.rs`

### Add or update built-in tool providers

- Tool provider definitions: `bitrouter-config/providers/tools/*.yaml`
- Built-in tool registry wiring: `bitrouter-config/src/registry.rs`
- Tool config schema: `bitrouter-config/src/config.rs` (`ToolConfig`)
- Tool routing: `bitrouter-config/src/routing.rs` (`ConfigToolRoutingTable`)

### Extend the default runtime server

- HTTP composition: `bitrouter/src/runtime/server.rs`
- Runtime router implementation: `bitrouter/src/runtime/router.rs`
- CLI entry point: `bitrouter/src/main.rs`

### Add a new model provider adapter

A model provider adapter lives inside `bitrouter-providers` and typically needs:

- provider-specific request/response types
- conversion logic between workspace types and the provider API
- a `LanguageModel` implementation from `bitrouter-core`
- a feature flag in `bitrouter-providers/Cargo.toml`
- runtime wiring in the router
- optional filter wiring in `bitrouter-api` if the provider has a public HTTP-compatible surface

### Add a new tool provider adapter

A tool provider adapter lives inside `bitrouter-providers` and typically needs:

- a `ToolProvider` implementation from `bitrouter-core`
- transport/client logic for the upstream protocol (MCP, REST, etc.)
- a feature flag in `bitrouter-providers/Cargo.toml`
- runtime wiring in the tool router
- optional built-in tool definitions in `bitrouter-config/providers/tools/`

## Validation

Before opening a change, run the workspace checks described in [`CONTRIBUTING.md`](CONTRIBUTING.md). AI agents should also read [`CLAUDE.md`](CLAUDE.md).
