# BitRouter

[![Build status](https://github.com/bitrouter/bitrouter/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/bitrouter/bitrouter/actions)
[![Crates.io](https://img.shields.io/crates/v/bitrouter)](https://crates.io/crates/bitrouter)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Twitter](https://img.shields.io/badge/Twitter-black?logo=x&logoColor=white)](https://x.com/BitRouterAI)
[![Discord](https://img.shields.io/badge/Discord-5865F2?logo=discord&logoColor=white)](https://discord.gg/G3zVrZDa5C)
[![Telegram](https://img.shields.io/badge/Telegram-26A5E4?logo=telegram&logoColor=white)](https://t.me/bitrouterai)
[![Docs](https://img.shields.io/badge/Docs-bitrouter.ai-green)](https://bitrouter.ai)

Optimize your agent for cost and performance — with every run.
An open-source LLM router that sends routine calls to open models and pays frontier prices only for the calls that earn them. Zero harness changes.

> **Not every agent run needs Claude Opus 4.8 — or even Claude 5.**
> \~80% of agent workloads run just fine on cheaper open-source models without sacrificing performance. Use BitRouter alongside Claude Code (or any coding agent) to reserve your subscription budget for the calls that actually need it. **Enjoy 25% off all open-source model calls on BitRouter Cloud today.**

## Before & After

Without BitRouter, your coding agent routes every call — file reads, summaries, tool calls, scaffolding — through the same frontier model. With BitRouter, routine work goes to open models automatically; frontier models get invoked only when they're justified.

| | Without BitRouter | With BitRouter |
| --- | --- | --- |
| **Routing** | All calls → one frontier model | Routine calls → open models; complex calls → frontier |
| **Cost** | Frontier pricing on every request | Frontier prices only where they're earned |
| **Setup change** | — | One env var |
| **Code change** | — | None |

<!-- Screenshots coming — will be added here -->

### Benchmark

| Metric | Without BitRouter | With BitRouter |
| --- | --- | --- |
| **Cost per task** | baseline | — |
| **Task success rate** | baseline | — |
| **Avg. latency** | baseline | — |

<!-- Benchmark data coming — replace with real numbers -->

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

BitRouter is a local proxy between your agent and every LLM provider. One env-var swap — no harness changes required:

```diff
- OPENAI_BASE_URL=https://api.openai.com/v1   # hardwired to one provider, no fallback
+ OPENAI_BASE_URL=http://localhost:4356        # all providers, automatic failover
```

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

### CLI

```bash
bitrouter start / stop / restart               # daemon lifecycle
bitrouter route <model>                        # trace how a model name resolves
bitrouter agents list / check / install        # ACP agent management
bitrouter key sign --user <id>                 # mint a scoped brvk_ API key
bitrouter auth login / logout / whoami         # BitRouter Cloud sign-in
bitrouter cloud keys / usage / billing         # manage cloud account
```

See [`CLI.md`](CLI.md) for flags, config resolution, and examples.

### Agent Skill

BitRouter ships an [Agent Skill](https://agentskills.io) — `/bitrouter` — so AI
coding agents can install, configure, migrate to, and troubleshoot BitRouter on
their own. It lives in this repo at [`skills/bitrouter/`](skills/), kept in sync
with the code.

```bash
bitrouter skills add bitrouter        # via BitRouter's own installer
npx skills add bitrouter/bitrouter    # via the generic skills CLI
```

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

Agents can't retry a provider outage the way a human can. BitRouter reroutes across providers mid-run, transparently — so a rate-limit at file 140 never makes you re-pay for 139 files of work. Configure fallback chains, round-robin across multiple accounts, or let cross-protocol routing send OpenAI-format requests to an Anthropic or Google upstream automatically.

### Observability

Billed per run. Now visible per run. Every agent, every model, every hop — with cost and latency attributed to the call. Query spend from the CLI without reaching for a dashboard, export to Prometheus or any OTLP backend, or trace exactly how a model name resolves before it hits the upstream with `bitrouter route <model>`.

### Security

One policy at the router — before requests leave your network and before responses reach your agent. Injection and output filtering, private by default. Virtual keys (`brvk_`) scope credentials per agent or user so no agent ever holds an upstream key directly; per-agent spend caps and loop guards keep runaway costs contained.

### Efficiency

Pay open-source prices for the calls that don't need frontier. Route by policy: fall back to a cheaper provider when the primary exceeds a cost threshold, or pin call types to the model with the best price-to-quality ratio for that task. Scoped virtual keys let you cap what each agent or user can spend before it touches your upstream account.

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

**Want to add a provider?** Open an issue or submit a PR. **Interested in a first-party integration?** Email [kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai) or [book a meeting](https://cal.com/kelsenliu).

## Supported Harnesses

Any agent runtime that speaks OpenAI or Anthropic APIs works with BitRouter out of the box — set `OPENAI_BASE_URL=http://localhost:4356` and you're done. The following harnesses are tested and supported:

| Harness        | Status | Notes                                                                                       |
| -------------- | ------ | ------------------------------------------------------------------------------------------- |
| Claude Code    | ✅     | [LLM gateway guide](https://code.claude.com/docs/en/llm-gateway)                           |
| OpenAI Codex   | ✅     | [Custom model providers](https://developers.openai.com/codex/config-advanced#custom-model-providers) |
| OpenCode       | ✅     | Via [models.dev](https://github.com/anomalyco/models.dev)                                  |
| Hermes Agent   | ✅     | Native plugin — [hermes-bitrouter-plugin](https://github.com/bitrouter/hermes-bitrouter-plugin) |
| Openclaw       | ✅     | Native plugin — [bitrouter-openclaw](https://github.com/bitrouter/bitrouter-openclaw)      |
| Pi-Agent       | ✅     | [Model configuration guide](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md) |

**Building an open-source agent?** Reach out at [kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai) or [book a meeting](https://cal.com/kelsenliu) — we offer **up to 50% off** for you and your community.

The full provider and harness catalog lives at [github.com/bitrouter/provider-registry](https://github.com/bitrouter/provider-registry).

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
