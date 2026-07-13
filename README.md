# BitRouter

[![Build status](https://github.com/bitrouter/bitrouter/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/bitrouter/bitrouter/actions)
[![Crates.io](https://img.shields.io/crates/v/bitrouter)](https://crates.io/crates/bitrouter)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Twitter](https://img.shields.io/badge/Twitter-black?logo=x&logoColor=white)](https://x.com/BitRouterAI)
[![Discord](https://img.shields.io/badge/Discord-5865F2?logo=discord&logoColor=white)](https://discord.gg/G3zVrZDa5C)
[![Telegram](https://img.shields.io/badge/Telegram-26A5E4?logo=telegram&logoColor=white)](https://t.me/bitrouterai)
[![Docs](https://img.shields.io/badge/Docs-bitrouter.ai-green)](https://bitrouter.ai)

**Cost-optimize your production agentic loops.**
An open-source agentic LLM gateway & router that cost-optimizes your production agentic loops by making models, tools, and agents all routable primitives. Zero harness changes.

> **You're tokenmaxxing in production.**
> Every step of every loop bills at frontier prices — file reads, tool calls, sub-agent hops, retries. Most of them don't need it. BitRouter routes each call, tool, and agent to the cheapest path that still reaches the goal, and tightens that routing as the loop runs.

## Before & After

Without BitRouter, your agent routes every call — file reads, summaries, tool calls, scaffolding — through the same frontier model. With BitRouter, routine work goes to open models automatically; frontier models get invoked only when they're justified.

| | Without BitRouter | With BitRouter |
| --- | --- | --- |
| **Routing** | All calls → one frontier model | Routine calls → open models; complex calls → frontier |
| **Cost** | Frontier pricing on every request | Frontier prices only where they're earned |
| **Setup change** | — | One env var |
| **Code change** | — | None |

<!-- Screenshots coming — will be added here -->

## Three primitives, one gateway

An agentic loop consumes three things. Other routers govern only the first. BitRouter makes all three routable, observable, and cost-governed:

- **Models** — route LLM calls across providers, protocols, and accounts. *(the classic router, cross-protocol)*
- **Tools** — an **MCP gateway** and an **AgentSkills gateway**: tools and skills become governed, routable resources instead of hardcoded endpoints.
- **Agents** — an **ACP gateway**: sub-agents are first-class, so you hand a task to a cheaper agent the same way you route a call to a cheaper model.

Cost optimization isn't just model selection — it's the cheapest model, the cheapest tool, and the cheapest sub-agent that still gets the loop to its goal.

## The self-improving loop

BitRouter wraps your agentic loop in a second loop. Each loop gets its own **policy spec** — a config file that declares how its calls, tools, and agents should route. BitRouter runs a continuous **observe → evaluate → act** cycle against it:

- **Observe** — every model, tool, and agent call, with cost and outcome attributed to the hop.
- **Evaluate** — score each run against the loop's goal.
- **Act** — update the policy spec. Let an agent self-tune the spec from the eval signal, or edit it yourself.

The result is a loop that gets cheaper the longer it runs in production — without re-paying frontier prices for work that never needed them.

## Features

Purpose-built for autonomous agents — concrete capabilities for unattended, multi-step execution:

- **Cross-protocol routing** — an OpenAI-format request to an Anthropic or Google upstream, and back
- **Multi-account failover + load-balancing** — reroute mid-run; a rate-limit at file 140 never re-pays for files 1–139
- **MCP gateway** — auth, access control, and identity forwarding in front of any MCP server
- **AgentSkills gateway** — install and serve skills as governed resources
- **ACP gateway** — route work to sub-agents as a first-class primitive
- **Per-request cost + latency** attributed to every agent, model, and hop
- **Telemetry export** to Prometheus or any OTLP backend
- **Per-loop policy spec** — declare routing; tune by hand or let an agent self-tune from the eval signal
- **Virtual keys (`brvk_`)** scoped per agent or user — no agent holds an upstream key
- **Per-agent spend caps + loop guards** to contain runaway cost
- **Injection + output guardrails** at the router, before requests leave your network
- **Zero-config auto-detection** + custom OpenAI-/Anthropic-compatible providers

## Comparison

|  | **BitRouter** | **OpenRouter** | **LiteLLM** | **TensorZero** | **Portkey** | **Bifrost** |
| --- | --- | --- | --- | --- | --- | --- |
| **Best for** | Cost-optimizing agent loops | Model marketplace | Unifying provider SDKs | Model optimization | Fast unified gateway | Fast unified gateway |
| **Routable primitives** | Models + tools + **agents** (MCP + ACP) | Models | Models + tools (MCP) | Models | Models + tools (MCP) | Models + tools (MCP) |
| **Optimizes** | The **loop**, by cost | Static routing | Static routing | The model | Static routing | Static routing |
| **Model catalog** | Curated + bring any provider | **1,600+ marketplace** | Any provider | Curated | **1,600+** | 23+ providers |

_All but OpenRouter are open-source and self-hostable; BitRouter and TensorZero are Rust. TensorZero is no longer maintained._

**TL;DR** — OpenRouter is a cloud API marketplace for humans picking models. LiteLLM (Python), Portkey (TypeScript), and Bifrost (Go) are unified gateways — fast, OpenAI-compatible, guardrails included — but they route models. TensorZero (Rust) adds a production feedback loop, but optimizes the model itself, not the loop. BitRouter is the only one that treats models, tools, and agents as a single routable surface — a Rust-native gateway that cost-optimizes the whole production loop, with cross-protocol routing, MCP and ACP gateways, and guardrails out of the box.

## Benchmarks

Reproducible benchmark evidence lives in [`benchmarks/`](benchmarks/). In the latest run (Terminal-Bench 2.1, Codex + Kimi), adaptive routing replaced strong-model calls with a cheaper model: round r2 cut imputed cost **32.8%** versus a strong-only control at near-parity score, and the best round scored **82.95%** at −8.2% cost. Each run ships its full report, a machine-readable `results.json`, the frozen config, and the derived evidence needed to recompute every number.

This is a mechanism study under a modified protocol (one attempt per task) — **not a Terminal-Bench leaderboard submission**. See [protocol and limitations](benchmarks/README.md#protocol-and-limitations) before citing the numbers.

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

### CLI

BitRouter runs as a local daemon — start it with your own keys or a Cloud sign-in.

**Bring your own keys (BYOK)** — auto-detected from the environment, no config file needed:

```bash
export OPENAI_API_KEY=sk-...    # ANTHROPIC_API_KEY / GEMINI_API_KEY also work
bitrouter start                 # proxy running at http://localhost:4356
```

**Or sign in to BitRouter Cloud** — one OAuth account covers every model, no upstream provider keys:

```bash
bitrouter cloud login           # RFC 8628 device flow against api.bitrouter.ai
bitrouter start                 # `bitrouter` provider auto-enables once signed in
```

Point your agent runtime at `http://localhost:4356` and any available provider is live. For advanced routing rules, guardrails, or multi-account failover, scaffold a config with `bitrouter init` (writes `./bitrouter.yaml`).

```bash
bitrouter start / stop / restart        # daemon lifecycle
bitrouter route <model>                 # trace how a model name resolves
bitrouter key sign --user <id>          # mint a scoped brvk_ API key
bitrouter cloud keys / usage / billing  # manage your cloud account
```

See [`CLI.md`](CLI.md) for the full command reference, flags, and config resolution.

### Agent Skill

BitRouter ships an [Agent Skill](https://agentskills.io) — `/bitrouter` — so AI
coding agents can install, configure, migrate to, and troubleshoot BitRouter on
their own. It lives in this repo at [`skills/bitrouter/`](skills/), kept in sync
with the code.

```bash
bitrouter skills add bitrouter        # via BitRouter's own installer
npx skills add bitrouter/bitrouter    # via the generic skills CLI
```

### MCP

Use BitRouter from any MCP client — it exposes `complete`, `list_models`, and `status` as MCP tools (the *origin* server, distinct from the MCP gateway that proxies your own MCP servers):

```bash
bitrouter mcp serve                    # stdio → local daemon at 127.0.0.1:4356
bitrouter mcp install --client claude  # print the Claude/Cursor mcpServers config block
```

Add `--transport http` to target the multi-tenant cloud backend.

### GUI

Native desktop app for driving multi-agent loops — coming soon.

## Models & providers

BitRouter routes to a *model*, not a provider. Each open-weight family below is served by many providers — its own lab, hyperscalers (AWS Bedrock, Alibaba Cloud), gateways (OpenRouter, OpenCode), and serverless clouds — and BitRouter picks the cheapest route per call. **Bring your own key** to any of them, or use one **BitRouter Cloud** account with no keys at all.

| Open model            | Lab      |
| --------------------- | -------- |
| DeepSeek V3.2 / V4    | DeepSeek |
| Qwen3 / Qwen3-Coder   | Alibaba  |
| Kimi K2               | Moonshot |
| GLM-5 / 5.1           | Z.ai     |
| MiniMax M2–M3         | MiniMax  |
| MiMo V2               | Xiaomi   |
| Step 3.5              | StepFun  |

Plus every frontier model from OpenAI, Anthropic, Google, and xAI — over your own keys, a subscription sign-in (Claude Pro/Max, GitHub Copilot, ChatGPT Codex), or BitRouter Cloud. Full catalog in the [registry/](registry/).

**Want to add a provider?** Open an issue or submit a PR. **Interested in a first-party integration?** Email [kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai) or [book a meeting](https://cal.com/kelsenliu).

## Harness integrations

Any agent runtime that speaks OpenAI or Anthropic APIs works with BitRouter out of the box — set `OPENAI_BASE_URL=http://localhost:4356` and you're done. The following harnesses are tested and supported:

| Harness        | Status | Notes                                                                                       |
| -------------- | ------ | ------------------------------------------------------------------------------------------- |
| Claude Code    | ✅     | [LLM gateway guide](https://code.claude.com/docs/en/llm-gateway)                           |
| OpenAI Codex   | ✅     | `bitrouter spawn --agent codex` or [custom model providers](https://developers.openai.com/codex/config-advanced#custom-model-providers) |
| OpenCode       | ✅     | Via [models.dev](https://github.com/anomalyco/models.dev)                                  |
| Hermes Agent   | ✅     | Native plugin — [hermes-bitrouter-plugin](https://github.com/bitrouter/hermes-bitrouter-plugin) |
| Openclaw       | ✅     | Native plugin — [bitrouter-openclaw](https://github.com/bitrouter/bitrouter-openclaw)      |
| Pi-Agent       | ✅     | [Model configuration guide](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md) |

**Building an open-source agent?** Reach out at [kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai) or [book a meeting](https://cal.com/kelsenliu) — we offer **up to 50% off** for you and your community.

The full provider and harness catalog lives in [github.com/bitrouter/bitrouter/registry](https://github.com/bitrouter/bitrouter/tree/main/registry).

## Documentation

- [`CLI.md`](CLI.md) — full CLI reference with flags and examples
- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace architecture and SDK internals
- [`docs/`](docs/) — guides and recipes (e.g. [Claude Code on your subscription](docs/integrations/claude-subscription.md))
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution workflow, issue reporting, and provider updates
- [`CLAUDE.md`](CLAUDE.md) — guidance for AI coding agents working in this repository
- [`skills/`](skills/) — the `/bitrouter` Agent Skill (source of truth)

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=bitrouter/bitrouter&type=Date)](https://star-history.com/#bitrouter/bitrouter&Date)

## License

Licensed under the [Apache License 2.0](LICENSE).
