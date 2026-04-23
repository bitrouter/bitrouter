# BitRouter - Open Intelligence Router for LLM Agents

> The agentic proxy for modern agent runtimes. Smart, safe, agent-controlled routing across LLMs, tools, and agents.

[![Build status](https://github.com/bitrouter/bitrouter/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/bitrouter/bitrouter/actions)
[![Crates.io](https://img.shields.io/crates/v/bitrouter)](https://crates.io/crates/bitrouter)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Twitter](https://img.shields.io/badge/Twitter-black?logo=x&logoColor=white)](https://x.com/BitRouterAI)
[![Discord](https://img.shields.io/badge/Discord-5865F2?logo=discord&logoColor=white)](https://discord.gg/G3zVrZDa5C)
[![Docs](https://img.shields.io/badge/Docs-bitrouter.ai-green)](https://bitrouter.ai)

## Overview

As LLM agents grow more autonomous, humans can no longer hand-pick the best model, tool, or sub-agent for every runtime decision. BitRouter is a proxy layer purpose-built for LLM agents (OpenClaw, OpenCode, etc.) to discover and route to LLMs, tools, and other agents autonomously — with agent-native control, guardrails, and observability via CLI + TUI, backed by a high-performance Rust proxy that optimizes for performance, cost, and safety during runtime.

## Features

- **Multi-provider routing** — unified access to OpenAI, Anthropic, Google, and custom providers with cost/performance-aware routing ([`core`](bitrouter-core/) · [`providers`](bitrouter-providers/) · [`config`](bitrouter-config/))
- **Tools as a service** — discover, aggregate, and route tool calls across MCP servers and REST APIs with the same config-driven routing used for models ([`core`](bitrouter-core/) · [`providers`](bitrouter-providers/) · [`config`](bitrouter-config/))
- **Streaming & non-streaming** — first-class support for both modes across all providers
- **Agent firewall** — inspect, warn, redact, or block risky content at the proxy layer ([`guardrails`](bitrouter-guardrails/))
- **MCP gateway** — proxy for MCP servers, agents discover and call tools across hosts ([`providers`](bitrouter-providers/) · [`api`](bitrouter-api/))
- **Skills registry** — track and expose agent skills following the [agentskills.io](https://agentskills.io) standard ([`providers`](bitrouter-providers/))
- **Agentic payment** — 402/MPP payment handling for LLMs, tools, and APIs ([`api`](bitrouter-api/) · [`accounts`](bitrouter-accounts/))
- **Observability** — per-request spend tracking, metrics, and cost calculation ([`observe`](bitrouter-observe/))
- **CLI + TUI** — monitor and control agent sessions in real time, with live ACP (Agent Client Protocol) integration for managing coding agents ([`cli`](bitrouter/) · [`tui`](bitrouter-tui/))

## Documentation

- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace architecture and server composition details
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution workflow, issue reporting, and provider updates
- [`CLAUDE.md`](CLAUDE.md) — guidance for AI coding agents working in this repository

## Quick Start

### Install

```bash
cargo install bitrouter
```

### Default (setup wizard)

```bash
bitrouter
```

On first launch, BitRouter runs an interactive setup wizard with two modes:

- **Cloud** — connect to BitRouter Cloud with x402/Solana wallet payments
- **BYOK** — bring your own API keys for OpenAI, Anthropic, Google, or custom providers

After setup, the TUI and API server start at `http://localhost:8787`.

You can re-run the wizard at any time with `bitrouter reset`.

### BYOK (bring your own keys)

If you already have provider API keys in your environment, BitRouter auto-detects them — no config file needed:

```bash
export OPENAI_API_KEY=sk-...
bitrouter
# Routes to "openai:gpt-4o" at http://localhost:8787
```

For a foreground server without the TUI, use `bitrouter serve`.

### Agent Skills

Install [Agent Skills](https://github.com/bitrouter/agent-skills) to give your AI agent the knowledge to register on the BitRouter network, configure services, and start serving requests:

```bash
# Any agent (Claude Code, Copilot, Cursor, Codex, etc.)
npx skills add BitRouterAI/agent-skills
```

## Supported Providers

| Provider   | Status | Notes                            |
| ---------- | ------ | -------------------------------- |
| OpenAI     | ✅     | Chat Completions + Responses API |
| Anthropic  | ✅     | Messages API                     |
| Google     | ✅     | Generative AI API                |
| OpenRouter | ✅     | Chat Completions + Responses API |

Want to see another provider supported? [Open an issue](https://github.com/bitrouter/bitrouter/issues) or submit a PR — contributions are welcome. If you're a provider interested in first-party integration, reach out on [Discord](https://discord.gg/G3zVrZDa5C).

## Supported Agent Runtimes

BitRouter works as a drop-in proxy for agent runtimes that support custom API base URLs. Point your runtime at `http://localhost:8787` and route to any configured provider.

| Runtime                                                  | Integration                                                      |
| -------------------------------------------------------- | ---------------------------------------------------------------- |
| [OpenClaw](https://github.com/openclaw/openclaw)         | [Native plugin](https://github.com/bitrouter/bitrouter-openclaw) |
| [Claude Code](https://github.com/anthropics/claude-code) | CLI + Skills                                                     |
| [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw)    | CLI + Skills                                                     |
| [Codex CLI](https://github.com/openai/codex)             | CLI + Skills                                                     |
| [OpenCode](https://github.com/opencode-ai/opencode)      | CLI + Skills                                                     |
| [Kilo Code](https://github.com/Kilo-Org/kilocode)        | CLI + Skills                                                     |

Any agent runtime that can target a custom OpenAI or Anthropic base URL works with BitRouter out of the box. **Building an agent runtime or framework?** We partner with teams to build native BitRouter integrations — reach out on [Discord](https://discord.gg/G3zVrZDa5C) or [open an issue](https://github.com/bitrouter/bitrouter/issues).

## Comparison

| | **BitRouter** | **OpenRouter** | **LiteLLM** |
| --- | --- | --- | --- |
| **Architecture** | Local-first proxy + optional cloud | Cloud-only SaaS | Local proxy (Python) |
| **Language** | Rust | Closed-source | Python |
| **Self-hosted** | Yes | No | Yes |
| **Agent-native** | Yes — built for autonomous agent runtimes | No — human-facing API gateway | Partial — SDK-oriented |
| **Agent protocols** | MCP + Skills + ACP | No | MCP |
| **Agent firewall** | Built-in guardrails (inspect, redact, block) | Yes | Yes |
| **Cross-protocol routing** | Yes (e.g. OpenAI format → Anthropic provider) | Provider-specific | Yes (unified interface) |
| **Agentic payments** | Stablecoin (402/MPP) + Fiat| Credit-based billing | No |
| **Observability** | CLI + TUI + per-request cost tracking | Web dashboard | Logging + callbacks + WebUI |
| **Extensibility** | Trait-based SDK — import and compose crates | API only | Python middleware |
| **Performance** | ~10ms | ~30ms (cloud) | ~500ms |
| **License** | Apache 2.0 | Proprietary | Apache 2.0 |

**TL;DR** — OpenRouter is a cloud API marketplace for humans picking models. LiteLLM is a Python proxy for unifying provider SDKs. BitRouter is a Rust-native proxy purpose-built for autonomous agents — with unified model and tool routing, agent protocols (MCP, Skills), guardrails, and agentic payments out of the box.

## Roadmap

- [x] Core routing engine and provider abstractions
- [x] OpenAI, Anthropic, and Google adapters
- [x] Interactive setup wizard (`bitrouter init`) with auto-detection
- [x] Custom provider support (OpenAI-compatible / Anthropic-compatible)
- [x] Cross-protocol routing (e.g. OpenAI format → Anthropic provider)
- [x] MCP & Skills protocol support
- [x] Tools as a service — config-driven tool routing across MCP and REST providers
- [x] ACP (Agent Client Protocol) integration — manage coding agents (Claude Code, OpenCode, OpenClaw) via the TUI
- [ ] TUI observability dashboard
- [ ] Telemetry and usage analytics
- [ ] Provider & model routing policy customization

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=bitrouter/bitrouter&type=Date)](https://star-history.com/#bitrouter/bitrouter&Date)

## License

Licensed under the [Apache License 2.0](LICENSE).
