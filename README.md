# BitRouter

[![Build status](https://github.com/bitrouter/bitrouter/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/bitrouter/bitrouter/actions)
[![Crates.io](https://img.shields.io/crates/v/bitrouter)](https://crates.io/crates/bitrouter)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Twitter](https://img.shields.io/badge/Twitter-black?logo=x&logoColor=white)](https://x.com/BitRouterAI)
[![Discord](https://img.shields.io/badge/Discord-5865F2?logo=discord&logoColor=white)](https://discord.gg/G3zVrZDa5C)
[![Telegram](https://img.shields.io/badge/Telegram-26A5E4?logo=telegram&logoColor=white)](https://t.me/bitrouterai)
[![Docs](https://img.shields.io/badge/Docs-bitrouter.ai-green)](https://bitrouter.ai)

Agent-native LLM router that optimizes your agent with every run. Zero harness changes — every model call reliable, traceable, secure, and cost-effective.

## What it does

BitRouter is a local proxy that sits between your agent and every upstream LLM provider. Point your agent at `http://localhost:4356` instead of a provider URL — your agent code stays unchanged while BitRouter handles routing, failover, cross-protocol translation, guardrails, and cost tracking.

```diff
- OPENAI_BASE_URL=https://api.openai.com/v1   # hardwired to one provider, no fallback
+ OPENAI_BASE_URL=http://localhost:4356        # all providers, automatic failover
```

That one env-var change is the only harness modification required. BitRouter auto-detects every API key in your environment and makes those providers immediately routable — no config file needed to get started.

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

## Features

BitRouter is purpose-built for autonomous agents — every feature is designed for unattended, multi-step execution rather than human-in-the-loop API access.

### Reliability

When an agent runs unattended, a provider outage or rate-limit doesn't get a human retry — it just fails. BitRouter routes each call through a configurable fallback chain: if the primary provider fails, the next takes over automatically. Configure multiple accounts per provider for round-robin load-balancing, or let cross-protocol routing send OpenAI-format requests to an Anthropic or Google upstream when it's the better option.

### Observability

Every model call is recorded with provider, model, latency, and cost — queryable from the CLI without reaching for a dashboard. Export to Prometheus or any OTLP-compatible backend for fleet-level visibility. Use `bitrouter route <model>` to trace exactly how a model name resolves before it reaches the upstream.

### Security

Guardrails run at the proxy layer — before requests leave your network and before responses reach your agent. Inspect and redact sensitive content, block policy-violating output, or abort streams mid-flight when a rule triggers. Virtual keys (`brvk_`) let you issue scoped credentials per agent or user so no agent ever holds an upstream API key directly. MCP tool calls route through the same layer, keeping tool access under one control point.

### Efficiency

BitRouter is written in Rust and adds ~10ms of overhead — well under the latency of any upstream model. No Python workers, no GIL, no warm-up time. Per-request cost tracking makes model spend visible immediately, without waiting for a provider invoice. ACP integration means you can manage coding agent sessions (Claude Code, Codex, Gemini CLI) from the same CLI, without extra tooling.

## Supported Providers

| Provider        | Status | Notes                                                          |
| --------------- | ------ | -------------------------------------------------------------- |
| OpenAI          | ✅     | Chat Completions + Responses API                               |
| Anthropic       | ✅     | Messages API + Claude Pro/Max subscription (PKCE)              |
| Google          | ✅     | Generative AI API                                              |
| Amazon Bedrock  | ✅     | Via AWS SDK (opt-in)                                           |
| OpenRouter      | ✅     | Chat Completions + Responses API                               |
| OpenCode Zen    | ✅     | Curated models across Chat Completions, Messages, and Generate Content protocols      |
| OpenCode Go     | ✅     | Low-cost subscription for open coding models                   |
| BitRouter Cloud | ✅     | OAuth sign-in (`bitrouter auth login`); cloud-managed routing  |
| GitHub Copilot  | ✅     | GitHub OAuth device flow (`bitrouter login github-copilot`)    |
| ChatGPT Codex   | ✅     | ChatGPT subscription PKCE (`bitrouter login openai-codex`)     |

Want to see another provider? [Open an issue](https://github.com/bitrouter/bitrouter/issues) or submit a PR. If you're a provider interested in first-party integration, reach out on [Discord](https://discord.gg/G3zVrZDa5C).

## Supported Harnesses

Any agent runtime that speaks OpenAI or Anthropic APIs works with BitRouter out of the box — set `OPENAI_BASE_URL=http://localhost:4356` and you're done. The following harnesses are tested and supported:

| Harness        | Status |
| -------------- | ------ |
| Claude Code    | ✅     |
| OpenAI Codex   | ✅     |
| OpenCode       | ✅     |
| Hermes Agent   | ✅     |
| Openclaw       | ✅     |
| Pi-Agent       | ✅     |

**Building an agent runtime?** We partner with teams on native integrations — email [contact@bitrouter.ai](mailto:contact@bitrouter.ai) or [book a meeting with the founder](https://cal.com/kelsenliu/founder-meeting).

The full provider and harness catalog lives at [github.com/bitrouter/provider-registry](https://github.com/bitrouter/provider-registry).

## CLI

```bash
bitrouter start / stop / restart               # daemon lifecycle
bitrouter route <model>                        # trace how a model name resolves
bitrouter agents list / check / install        # ACP agent management
bitrouter key sign --user <id>                 # mint a scoped brvk_ API key
bitrouter auth login / logout / whoami         # BitRouter Cloud sign-in
bitrouter cloud keys / usage / billing         # manage cloud account
```

See [`CLI.md`](CLI.md) for flags, config resolution, and examples.

## Agent Skill

BitRouter ships an [Agent Skill](https://agentskills.io) — `/bitrouter` — so AI
coding agents can install, configure, migrate to, and troubleshoot BitRouter on
their own. It lives in this repo at [`skills/bitrouter/`](skills/), kept in sync
with the code.

```bash
bitrouter skills add bitrouter        # via BitRouter's own installer
npx skills add bitrouter/bitrouter    # via the generic skills CLI
```

## Documentation

- [`CLI.md`](CLI.md) — full CLI reference with flags and examples
- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace architecture and SDK internals
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution workflow, issue reporting, and provider updates
- [`CLAUDE.md`](CLAUDE.md) — guidance for AI coding agents working in this repository
- [`skills/`](skills/) — the `/bitrouter` Agent Skill (source of truth)

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
