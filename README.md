# BitRouter - Open Intelligence Router for LLM Agents

> The zero-ops LLM gateway built for modern agent runtime. Single binary. Zero infrastructure dependencies. Agent-native control.

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Twitter](https://img.shields.io/badge/Twitter-black?logo=x&logoColor=white)](https://x.com/BitRouterAI)
[![Discord](https://img.shields.io/badge/Discord-5865F2?logo=discord&logoColor=white)](https://discord.gg/G3zVrZDa5C)
[![Docs](https://img.shields.io/badge/Docs-bitrouter.ai-green)](https://bitrouter.ai)

## Overview

As LLM agents grow more autonomous, humans can no longer hand-pick the best model, tool, or sub-agent for every runtime decision. BitRouter is a proxy layer purpose-built for LLM agents (OpenClaw, OpenCode, etc.) to discover and route to LLMs, tools, and other agents autonomously — with agent-native control and observability via CLI + TUI, backed by a high-performance Rust proxy that optimizes for both performance and cost during runtime.

## Features

- **Agent-native routing** — agents discover and select LLMs, tools, and sub-agents at runtime
- **Multi-provider gateway** — unified access to OpenAI, Anthropic, Google, and more
- **Streaming & non-streaming** — first-class support for both modes
- **CLI + TUI observability** — monitor and control agent sessions in real time
- **Smart routing** — cost and performance optimization via configurable routing tables
- **High-performance proxy** — single Rust binary, async-first, minimal overhead
- **Tool calling** — unified tool use across providers

## Crate Structure

| Crate | Description |
|---|---|
| `bitrouter-core` | Core traits, models, and error types |
| `bitrouter-openai` | OpenAI adapter (Chat Completions & Responses API) |
| `bitrouter-anthropic` | Anthropic adapter (Messages API) |
| `bitrouter-google` | Google adapter (Gemini API) |
| `bitrouter-warp-router` | HTTP routing layer via Warp |
| `bitrouter` | Top-level re-export crate |

## Quick Start

```bash
# Install
cargo install bitrouter

# Start the proxy
bitrouter start
```

<!-- Add more details on configuration and agent integration as the CLI stabilizes -->

## Supported Providers

| Provider | Status | Notes |
|---|---|---|
| OpenAI | ✅ | Chat Completions + Responses API |
| Anthropic | ✅ | Messages API |
| Google | ✅ | Gemini API |

Want to see another provider supported? [Open an issue](https://github.com/AIMOverse/bitrouter/issues) or submit a PR — contributions are welcome. If you're a provider interested in first-party integration, reach out on [Discord](https://discord.gg/G3zVrZDa5C).

## Architecture

```
                         ┌─────────────────┐
                         │   LLM Agent     │
                         └────────┬────────┘
                                  │ request
                                  ▼
                         ┌─────────────────┐
                         │  RoutingTable   │  ← maps model name → (provider, model_id)
                         └────────┬────────┘
                                  │ RoutingTarget
                                  ▼
               ┌──────────────────────────────────────┐
               │      LanguageModelRouter /           │
               │      ImageModelRouter                │  ← resolves target → provider impl
               └────┬─────────┬──────────┬────────────┘
                    │         │          │
                    ▼         ▼          ▼
              ┌─────────┐ ┌──────────┐ ┌────────┐
              │ OpenAI  │ │Anthropic │ │ Google │    ← LanguageModel / ImageModel trait
              └─────────┘ └──────────┘ └────────┘
```

- **`LanguageModel` / `ImageModel`** — provider trait that each adapter implements (`generate`, `stream`)
- **`RoutingTable`** — maps an incoming model name to a `RoutingTarget` (provider + model ID)
- **`LanguageModelRouter` / `ImageModelRouter`** — resolves a `RoutingTarget` to a concrete provider instance
- **Provider adapters** — translate BitRouter types to/from each provider's native API

## Roadmap

- [x] Core routing engine and provider abstractions
- [x] OpenAI, Anthropic, and Google adapters
- [ ] MCP & A2A protocol support
- [ ] TUI observability dashboard
- [ ] Telemetry and usage analytics
- [ ] Provider & model routing policy customization

## Contributing

We welcome contributions of all kinds — bug fixes, new providers, documentation, and feature ideas.

### Getting Started

1. Fork the repository
2. Create a feature branch: `git checkout -b feat/my-feature`
3. Make your changes
4. Run tests: `cargo test --workspace`
5. Run formatting and lints: `cargo fmt --all && cargo clippy --workspace`
6. Commit your changes with a descriptive message
7. Push and open a pull request

### Guidelines

- Keep PRs focused — one feature or fix per PR
- Follow existing code style and conventions
- Add tests for new functionality
- Update documentation if your change affects public APIs
- Be respectful and constructive in discussions

### Branch Naming

| Prefix | Purpose |
|---|---|
| `feat/` | New features |
| `fix/` | Bug fixes |
| `docs/` | Documentation |
| `refactor/` | Code refactoring |
| `chore/` | Maintenance tasks |

For larger changes, please open an issue first to discuss the approach. If you have questions, join us on [Discord](https://discord.gg/G3zVrZDa5C).

## License

Licensed under the [Apache License 2.0](LICENSE).
