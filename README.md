# BitRouter

[![Build status](https://github.com/bitrouter/bitrouter/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/bitrouter/bitrouter/actions)
[![Crates.io](https://img.shields.io/crates/v/bitrouter)](https://crates.io/crates/bitrouter)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Twitter](https://img.shields.io/badge/Twitter-black?logo=x&logoColor=white)](https://x.com/BitRouterAI)
[![Discord](https://img.shields.io/badge/Discord-5865F2?logo=discord&logoColor=white)](https://discord.gg/G3zVrZDa5C)
[![Hugging Face](https://img.shields.io/badge/Hugging_Face-FFD21E?logo=huggingface&logoColor=black)](https://huggingface.co/BitRouterAI)
[![Docs](https://img.shields.io/badge/Docs-bitrouter.ai-green)](https://bitrouter.ai)
[![Benchmarks](https://img.shields.io/badge/Benchmarks-reports-orange)](benchmarks/)

**A context-aware router that optimizes your agentic loops — every run.**
An open-source agentic LLM gateway & router that makes models, tools, and agents all routable primitives, then re-optimizes that routing every run. **Today it optimizes for cost**; the same act → observe → evaluate → learn loop generalizes to latency- and accuracy-driven objectives. Zero harness changes.

> **You're tokenmaxxing in production.**
> Every step of every loop bills at frontier prices — file reads, tool calls, sub-agent hops, retries. Most of them don't need it. BitRouter routes each call, tool, and agent to the cheapest path that still reaches the goal, and tightens that routing as the loop runs.

## Three primitives, one gateway

An agentic loop consumes three things. Other routers govern only the first. BitRouter makes all three routable, observable, and governed:

- **Models** — route LLM calls across providers, accounts, and wire protocols: OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and Google Gemini. *(the classic router, cross-protocol — any request format to any upstream, and back)*
- **Tools** — an **MCP gateway** and an **AgentSkills gateway**: tools and skills become governed, routable resources instead of hardcoded endpoints. *(The skills gateway folds into the MCP gateway once the [MCP skills extension](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2640) reaches production.)*
- **Agents** — an **ACP gateway**: sub-agents become first-class routable primitives, so a task can go to the sub-agent that best fits the loop's objective — just as a call routes to the best-fit model. *(Local sub-agents over stdio today; remote gateways arrive with [ACP v2](https://agentclientprotocol.com/rfds/v2/overview).)*

Optimizing a loop isn't just model selection — it's choosing the model, the tool, and the sub-agent that best serve the loop's objective at every step that gets it to its goal.

## The self-improving loop

BitRouter wraps your agentic loop in a second loop. Each loop gets its own **policy spec** — a config file that declares how its calls, tools, and agents should route. Against that spec BitRouter runs a continuous **act → observe → evaluate → learn** cycle, and every step is a component it already ships:

- **Act — the router.** Each model, tool, and agent call is rewritten to a chosen route: policy-table routing, cross-protocol translation, multi-account failover.
- **Observe — telemetry.** Every hop is attributed with cost, tokens, latency, and outcome, and exported to Prometheus or any OTLP backend.
- **Evaluate — the eval engine.** Each run and routing decision is scored against your chosen objective — did the route it picked still reach the goal?
- **Learn — the policy engine.** The eval signal folds back into the policy spec: an agent self-tunes it, or you edit it by hand. The next turn of the loop acts on the improved spec.

You choose what the loop optimizes for — cost, latency, or accuracy — and it improves the longer it runs in production.

## Benchmarks

Today **cost** is the validated objective: on Terminal-Bench 2.1, `gpt-5.5` with BitRouter cut cost **32.8%** at near-parity accuracy (−1.1 pp), by offloading routine steps to a cheaper model. Latency and accuracy objectives — and more base models — are landing next.

| Base model | Cost vs baseline | Latency vs baseline | Accuracy vs baseline |
| --- | --- | --- | --- |
| `gpt-5.5` | **−32.8%**¹ | coming soon | coming soon |
| `gpt-5.6` | coming soon | coming soon | coming soon |
| `claude-opus-4.8` | coming soon | coming soon | coming soon |
| `claude-sonnet-5` | coming soon | coming soon | coming soon |
| `claude-fable-5` | coming soon | coming soon | coming soon |

¹ Cost-optimization run on Terminal-Bench 2.1: −32.8% zero-cache imputed cost (audited range 28.6–32.8% by cache share) at near-parity accuracy, −1.1 pp (76.1% vs 77.3%, within single-attempt noise).

This is a mechanism study under a modified protocol, not a Terminal-Bench leaderboard submission — read the [experiment limitations](benchmarks/001-2026-07-10-tbench-v2.1-codex-gpt55-kimi-k27.md#limitations) before citing the numbers. Full reports live in [`benchmarks/`](benchmarks/); complete traces, tool calls, usage, policy decisions, configs, and checksums are in the [`BitRouterAI/benchmarks`](https://huggingface.co/datasets/BitRouterAI/benchmarks) dataset.

## Comparison

Every gateway below routes model calls. BitRouter is the only one that also makes **tools and agents** routable, and optimizes the whole **loop** rather than a single call.

|  | **BitRouter** | **OpenRouter** | **LiteLLM** | **TensorZero** | **Portkey** | **Bifrost** |
| --- | --- | --- | --- | --- | --- | --- |
| **Routable primitives** | Models + tools + **agents** (MCP + ACP) | Models | Models + tools (MCP) | Models | Models + tools (MCP) | Models + tools (MCP) |
| **Optimizes** | The **loop**, multi-objective (cost today) | Static routing | Static routing | The model | Static routing | Static routing |

_All but OpenRouter are open-source and self-hostable; BitRouter and TensorZero are Rust._

## What BitRouter is not

- **Not an inference provider** — it serves no weights and hosts no GPUs; it sits in front of the providers you already use.
- **Not an agent framework or harness** — it runs *under* Claude Code, Codex, and the rest, not instead of them.
- **Not a hard dependency** — it's a local proxy behind one env var; unset the var and your stack is untouched.

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

### API

BitRouter exposes an OpenAI- and Anthropic-compatible HTTP API on `http://localhost:4356`, so any SDK or client works unchanged. The full endpoint reference and OpenAPI spec live in [`bitrouter/bitrouter-docs`](https://github.com/bitrouter/bitrouter-docs) (rendered at [bitrouter.ai](https://bitrouter.ai)).

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

## Features

Beyond the gateways above, the production controls for running agents unattended:

- **Multi-account failover + load-balancing** — reroute mid-run; a rate-limit at file 140 never re-pays for files 1–139
- **Virtual keys (`brvk_`)** scoped per agent or user — no agent holds an upstream key
- **Per-agent spend caps + loop guards** to contain runaway cost
- **Injection + output guardrails** at the router, before requests leave your network
- **Zero-config auto-detection** + custom OpenAI-/Anthropic-compatible providers

## Development

- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace architecture and SDK internals
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution workflow, issue reporting, and provider updates
- [`CLAUDE.md`](CLAUDE.md) — guidance for AI coding agents working in this repository
- [`skills/`](skills/) — the `/bitrouter` Agent Skill (source of truth)

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=bitrouter/bitrouter&type=Date)](https://star-history.com/#bitrouter/bitrouter&Date)

## License

Licensed under the [Apache License 2.0](LICENSE).
