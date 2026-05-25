# BitRouter

> The local proxy built for AI agent runtimes — one endpoint to reach any LLM provider, with automatic cross-protocol translation and failover.

[![Build status](https://github.com/bitrouter/bitrouter/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/bitrouter/bitrouter/actions)
[![Crates.io](https://img.shields.io/crates/v/bitrouter)](https://crates.io/crates/bitrouter)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Twitter](https://img.shields.io/badge/Twitter-black?logo=x&logoColor=white)](https://x.com/BitRouterAI)
[![Discord](https://img.shields.io/badge/Discord-5865F2?logo=discord&logoColor=white)](https://discord.gg/G3zVrZDa5C)
[![Docs](https://img.shields.io/badge/Docs-bitrouter.ai-green)](https://bitrouter.ai)

## Overview

AI agent runtimes need to route to different LLM providers without reconfiguration. BitRouter is a local Rust proxy that gives your agent a single endpoint at `http://localhost:4356` — configure your providers once, then route freely. Any client protocol (OpenAI, Anthropic, Google) can transparently target any upstream, with ~10ms overhead and no cloud dependency required.

## Install

```bash
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/bitrouter/bitrouter/releases/latest/download/bitrouter-installer.sh | sh

# Homebrew
brew install bitrouter/tap/bitrouter

# npm
npm install -g bitrouter
```

<details>
<summary>From source (Cargo)</summary>

```bash
cargo install bitrouter
```

</details>

## Quick Start

### Local (BYOK)

Set your provider API keys and start:

```bash
export OPENAI_API_KEY=sk-...    # ANTHROPIC_API_KEY / GOOGLE_API_KEY also work
bitrouter start
# Proxy running at http://localhost:4356
```

BitRouter auto-detects any key set in the environment — no config file needed. Point your agent runtime at `http://localhost:4356` and any provider whose key is present is immediately available.

For advanced routing rules, guardrails, or multi-account failover, scaffold a config file:

```bash
bitrouter init          # writes ./bitrouter.yaml (override with `-c <path>`)
bitrouter start
```

### Cloud

Sign in to your BitRouter Cloud account from the terminal — one OAuth account covers every model the gateway offers, no upstream provider keys required:

```bash
bitrouter auth login    # RFC 8628 device flow against api.bitrouter.ai
bitrouter start         # `bitrouter` provider auto-enables once signed in
```

Manage keys, usage, billing, policies, and BYOK from the same CLI — see `bitrouter cloud --help` or [`CLI.md`](CLI.md#cloud-account-management).

## Features

- **Multi-provider routing** — unified access to OpenAI, Anthropic, Google, OpenRouter, OpenCode Zen / Go, BitRouter Cloud, GitHub Copilot, ChatGPT Codex, and Amazon Bedrock; configure multiple accounts per provider with failover or round-robin load-balancing ([`sdk`](crates/bitrouter-sdk/) · [`providers`](crates/bitrouter-providers/))
- **Cross-protocol routing** — any client protocol (OpenAI Chat, OpenAI Responses, Anthropic Messages, Google Gemini) against any upstream; BitRouter translates transparently ([`sdk`](crates/bitrouter-sdk/))
- **Zero-config** — auto-detects providers from environment variables; no config file needed to get started ([`providers`](crates/bitrouter-providers/))
- **MCP gateway** — proxy for MCP servers; agents discover and call tools across hosts ([`sdk`](crates/bitrouter-sdk/))
- **ACP integration** — manage coding agent sessions (Claude Code, OpenAI Codex, Gemini CLI) via the CLI ([`sdk`](crates/bitrouter-sdk/))
- **Agent guardrails** — inspect, redact, or block risky content at the proxy layer ([`guardrails`](plugins/bitrouter-guardrails/))
- **Observability** — per-request spend tracking, Prometheus metrics, and OTLP export ([`observe`](plugins/bitrouter-observe/))
- **Virtual keys** — mint scoped `brvk_` API keys with `bitrouter key sign`, persisted to SQLite, PostgreSQL, or MySQL
- **Custom providers** — add any OpenAI-compatible or Anthropic-compatible upstream via config

## Supported Providers

| Provider        | Status | Notes                                                          |
| --------------- | ------ | -------------------------------------------------------------- |
| OpenAI          | ✅     | Chat Completions + Responses API                               |
| Anthropic       | ✅     | Messages API + Claude Pro/Max subscription (PKCE)              |
| Google          | ✅     | Generative AI API                                              |
| Amazon Bedrock  | ✅     | Via AWS SDK (opt-in)                                           |
| OpenRouter      | ✅     | Chat Completions + Responses API                               |
| OpenCode Zen    | ✅     | Curated models across OpenAI, Anthropic, Google protocols      |
| OpenCode Go     | ✅     | Low-cost subscription for open coding models                   |
| BitRouter Cloud | ✅     | OAuth sign-in (`bitrouter auth login`); cloud-managed routing  |
| GitHub Copilot  | ✅     | GitHub OAuth device flow (`bitrouter login github-copilot`)    |
| ChatGPT Codex   | ✅     | ChatGPT subscription PKCE (`bitrouter login openai-codex`)     |

Want to see another provider? [Open an issue](https://github.com/bitrouter/bitrouter/issues) or submit a PR. If you're a provider interested in first-party integration, reach out on [Discord](https://discord.gg/G3zVrZDa5C).

Any agent runtime that supports a custom OpenAI or Anthropic base URL works with BitRouter out of the box — point it at `http://localhost:4356`. **Building an agent runtime?** We partner with teams on native integrations — email [contact@bitrouter.ai](mailto:contact@bitrouter.ai) or [book a meeting with the founder](https://cal.com/kelsenliu/founder-meeting).

## Comparison

|                           | **BitRouter**                               | **OpenRouter**            | **LiteLLM**                    |
| ------------------------- | ------------------------------------------- | ------------------------- | ------------------------------ |
| **Architecture**          | Local-first proxy + optional cloud          | Cloud-only SaaS           | Local proxy (Python)           |
| **Language**              | Rust                                        | Closed-source             | Python                         |
| **Self-hosted**           | Yes                                         | No                        | Yes                            |
| **Agent-native**          | Yes — built for autonomous agent runtimes   | No — human-facing gateway | Partial — SDK-oriented         |
| **Agent protocols**       | MCP + ACP                                   | No                        | MCP                            |
| **Agent guardrails**      | Built-in (inspect, redact, block)           | Yes                       | Yes                            |
| **Cross-protocol routing**| Yes (e.g. OpenAI format → Anthropic upstream)| Provider-specific        | Yes (unified interface)        |
| **Observability**         | CLI + per-request cost tracking + Prometheus| Web dashboard             | Logging + callbacks + WebUI    |
| **Extensibility**         | Trait-based SDK — import and compose crates | API only                  | Python middleware               |
| **Performance**           | ~10ms                                       | ~30ms (cloud)             | ~500ms                         |
| **License**               | Apache 2.0                                  | Proprietary               | Apache 2.0                     |

**TL;DR** — OpenRouter is a cloud API marketplace for humans picking models. LiteLLM is a Python proxy for unifying provider SDKs. BitRouter is a Rust-native proxy purpose-built for autonomous agents — with cross-protocol routing, MCP and ACP support, and guardrails out of the box.

## CLI

```bash
bitrouter start / stop / restart / reload      # daemon lifecycle
bitrouter status                               # pid, listen address, active models
bitrouter route <model>                        # trace how a model name resolves
bitrouter models [--provider <id>]            # list routable models
bitrouter providers list                       # list configured providers
bitrouter tools list / status / discover       # MCP server introspection
bitrouter agents list / check / install        # ACP agent management
bitrouter key sign --user <id>                 # mint a scoped brvk_ API key
bitrouter policy create <id>                   # scaffold a routing policy
bitrouter init                                 # scaffold bitrouter.yaml
bitrouter login <provider>                     # upstream-provider OAuth (anthropic, openai-codex, github-copilot, …)
bitrouter auth login / logout / whoami         # BitRouter Cloud sign-in (RFC 8628 device flow)
bitrouter cloud keys / usage / billing / …     # manage keys, usage, billing, policies, BYOK against BitRouter Cloud
```

See [`CLI.md`](CLI.md) for flags, config resolution, and examples.

## Documentation

- [`CLI.md`](CLI.md) — full CLI reference with flags and examples
- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace architecture and SDK internals
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution workflow, issue reporting, and provider updates
- [`CLAUDE.md`](CLAUDE.md) — guidance for AI coding agents working in this repository

## Roadmap

- [x] Core routing engine and provider abstractions
- [x] OpenAI, Anthropic, Google, and Amazon Bedrock adapters
- [x] Zero-config auto-detection from environment variables
- [x] Custom provider support (OpenAI-compatible / Anthropic-compatible)
- [x] Cross-protocol routing (e.g. OpenAI format → Anthropic provider)
- [x] MCP gateway and ACP agent integration
- [x] Multiple accounts per provider — failover + load-balancing
- [x] Virtual key management (`bitrouter key`) backed by SQLite / PostgreSQL / MySQL
- [ ] Telemetry and usage analytics
- [ ] Provider & model routing policy customization

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=bitrouter/bitrouter&type=Date)](https://star-history.com/#bitrouter/bitrouter&Date)

## License

Licensed under the [Apache License 2.0](LICENSE).
