# Development Guide

This document is the workspace-level guide for BitRouter internals. Start with [`README.md`](README.md) for the product introduction, then use this guide when you need to understand how the workspace is assembled or how to build on top of its reusable server components.

## Workspace Architecture

BitRouter is organized as a set of focused crates:

| Crate                 | Responsibility                                                                                        |
| --------------------- | ----------------------------------------------------------------------------------------------------- |
| `bitrouter`           | Thin CLI entry point that parses commands, resolves runtime paths, and launches the runtime           |
| `bitrouter-runtime`   | Application assembly, runtime path resolution, daemon lifecycle, and Warp server bootstrapping        |
| `bitrouter-api`       | Reusable Warp filters for provider-compatible HTTP endpoints                                          |
| `bitrouter-config`    | YAML loading, `.env` support, environment substitution, built-in providers, and config-backed routing |
| `bitrouter-core`      | Shared model traits, router contracts, errors, and transport-neutral types                            |
| `bitrouter-openai`    | OpenAI-compatible language model adapters                                                             |
| `bitrouter-anthropic` | Anthropic Messages adapter                                                                            |
| `bitrouter-google`    | Google Generative AI adapter                                                                          |
| `bitrouter-tui`       | Terminal UI used by the default interactive `bitrouter` flow                                          |

## Request Flow

A typical request moves through the workspace in this order:

1. The CLI resolves runtime paths with `bitrouter-runtime::resolve_home` and applies any per-path overrides.
2. `AppRuntime::load` reads `bitrouter.yaml`, optionally loads `.env`, substitutes `${VAR}` values, merges built-in provider definitions, and builds a `ConfigRoutingTable`.
3. `bitrouter-runtime::Router` receives provider configs and knows how to instantiate concrete provider-backed language models.
4. `bitrouter-runtime::ServerPlan` wires reusable filters from `bitrouter-api` into a Warp server.
5. Each API filter asks a `RoutingTable` to resolve the incoming model name, then asks a `LanguageModelRouter` to construct the concrete model implementation.
6. Provider crates translate between BitRouter's internal request/response types and the upstream provider API.

## Configuration Model

### Runtime paths

`bitrouter-runtime/src/paths.rs` is the source of truth for BitRouter's home-directory behavior.

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

`bitrouter-config` is responsible for turning config files into a usable routing layer.

- Built-in providers live in [`bitrouter-config/providers`](bitrouter-config/providers) as YAML files embedded at compile time.
- `BitrouterConfig::load_from_file` merges user-defined providers on top of those built-ins.
- Provider `derives` chains are resolved before the runtime starts serving traffic.
- `env_prefix` automatically maps environment variables such as `OPENAI_API_KEY` and `OPENAI_BASE_URL` onto a provider config.
- `ConfigRoutingTable` supports two routing modes:
  - direct routing with `provider:model_id`
  - model aliases defined under `models` using `priority` or `load_balance`

## HTTP Server Surface

The reusable HTTP filters live in `bitrouter-api`.

### Filters currently provided

| Route                               | Crate module                                               |
| ----------------------------------- | ---------------------------------------------------------- |
| `GET /health`                       | `bitrouter-runtime::ServerPlan`                            |
| `POST /v1/chat/completions`         | `bitrouter_api::router::openai::chat::filters`             |
| `POST /v1/responses`                | `bitrouter_api::router::openai::responses::filters`        |
| `POST /v1/messages`                 | `bitrouter_api::router::anthropic::messages::filters`      |
| `POST /v1beta/models/:model_action` | `bitrouter_api::router::google::generate_content::filters` |

`ServerPlan` currently wires `/health`, OpenAI chat completions, OpenAI responses, and Anthropic messages into the default runtime server. The Google-compatible filter is available from `bitrouter-api` and can be added directly when you build a custom service.

## Building Your Own Router Service

If you want BitRouter's routing and provider adapters without the stock CLI/runtime entry point, you can compose your own Warp server from the reusable parts.

### Option 1: reuse the existing config and provider router

This is the shortest path when you want BitRouter-compatible config loading and provider instantiation:

```rust
use std::sync::Arc;

use bitrouter_api::router::{anthropic, google, openai};
use bitrouter_config::{BitrouterConfig, ConfigRoutingTable};
use bitrouter_runtime::Router;
use warp::Filter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = BitrouterConfig::load_from_file("bitrouter.yaml".as_ref(), Some(".env".as_ref()))?;

    let table = Arc::new(ConfigRoutingTable::new(
        config.providers.clone(),
        config.models.clone(),
    ));
    let router = Arc::new(Router::new(reqwest::Client::new(), config.providers.clone()));

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

### Option 2: bring your own routing table or model router

`bitrouter-api` depends only on the contracts from `bitrouter-core`:

- `RoutingTable` resolves an incoming model name into a provider name plus upstream model ID.
- `LanguageModelRouter` constructs a concrete model implementation for that target.

If you already have your own config system or provider registry, implement those traits and pass your types into the existing filters.

## Where To Extend The System

### Add or update built-in providers

- Provider definitions: `bitrouter-config/providers/*.yaml`
- Built-in registry wiring: `bitrouter-config/src/registry.rs`
- Provider config schema: `bitrouter-config/src/config.rs`

### Extend the default runtime server

- HTTP composition: `bitrouter-runtime/src/server.rs`
- Runtime router implementation: `bitrouter-runtime/src/router.rs`
- CLI entry point: `bitrouter/src/main.rs`

### Add a new provider adapter crate

A provider crate typically needs:

- provider-specific request/response types
- conversion logic between workspace types and the provider API
- a `LanguageModel` implementation from `bitrouter-core`
- runtime wiring in `bitrouter-runtime::Router`
- optional filter wiring in `bitrouter-api` if the provider has a public HTTP-compatible surface

## Validation

Before opening a change, run the workspace checks described in [`CONTRIBUTING.md`](CONTRIBUTING.md). AI agents should also read [`CLAUDE.md`](CLAUDE.md).
