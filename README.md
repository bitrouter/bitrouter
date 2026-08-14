# BitRouter

[![Build status](https://github.com/bitrouter/bitrouter/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/bitrouter/bitrouter/actions)
[![Crates.io](https://img.shields.io/crates/v/bitrouter)](https://crates.io/crates/bitrouter)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Twitter](https://img.shields.io/badge/Twitter-black?logo=x&logoColor=white)](https://x.com/BitRouterAI)
[![Discord](https://img.shields.io/badge/Discord-5865F2?logo=discord&logoColor=white)](https://discord.gg/G3zVrZDa5C)
[![Hugging Face](https://img.shields.io/badge/Hugging_Face-FFD21E?logo=huggingface&logoColor=black)](https://huggingface.co/BitRouterAI)
[![Docs](https://img.shields.io/badge/Docs-bitrouter.ai-green)](https://bitrouter.ai)
[![LinkedIn](https://img.shields.io/badge/LinkedIn-0A66C2?logo=linkedin&logoColor=white)](https://www.linkedin.com/company/bitrouterai/?viewAsMember=true)
[![Book a call](https://img.shields.io/badge/Book_a_call-founders-000000?logo=cal.com&logoColor=white)](https://cal.com/kelsenliu)

**The self-improving LLM router that optimizes your agentic workflows with every run, works with any harnesses, any models, any loops.**

> **You're tokenmaxxing in production.**
> Every step of every loop bills at frontier prices — file reads, tool calls, sub-agent hops, retries. Most don't need it. BitRouter routes each call, tool, and agent to the cheapest path that still reaches the goal, and tightens that routing as the loop runs.

Cost is live today — latency and accuracy are next.

## Three primitives, one gateway

An agentic loop consumes three things. Other routers govern only the first. BitRouter makes all three routable, observable, and governed:

- **Models** — route LLM calls across providers, accounts, and wire protocols: OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and Google Gemini. *(the classic router, cross-protocol — any request format to any upstream, and back)*
- **Capabilities** — an **MCP gateway** and an **AgentSkills gateway**: tools and skills become governed, routable resources instead of hardcoded endpoints. *(The skills gateway folds into the MCP gateway once the [MCP skills extension](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2640) reaches production.)*
- **Agents** — an **ACP gateway**: sub-agents become first-class routable primitives, so a task can go to the sub-agent that best fits the loop's objective — just as a call routes to the best-fit model. *(Local sub-agents over stdio today; remote gateways arrive with [ACP v2](https://agentclientprotocol.com/rfds/v2/overview).)*

Optimizing a loop isn't just model selection — it's choosing the model, the tool, and the sub-agent that best serve the loop's objective at every step that gets it to its goal.

## The self-improving loop

BitRouter wraps your agentic loop in a second loop. `bitrouter.yaml` declares
providers, presets, and whether the process may publish; `policy-lock.yaml` is
the only live route authority. The routing key is **context-aware and lives as
code**: it is the step in the loop, not just the model name.

```yaml
policy:
  path: ./policy-lock.yaml
  mode: adaptive                 # authorizes explicit publication only
presets:
  auto:
    model: openai-codex:gpt-5.6-sol
    policy: auto
```

The v3 lock behind `@auto` contains the tier targets, canonical `agent_trace`
routes, capability guardrails, and a decision certificate for every explicit
route. A target may be a scalar model or an exact `(model, effort)` pair;
`@auto:cost` selects the cost variant when one is defined, while explicit
physical model IDs remain passthrough.

Against that spec BitRouter provides the control plane for an **act → observe → evaluate → compile** cycle:

- **Act — the router** reads the lock and rewrites each `@preset[:variant]` call to its tier's model: policy routing, cross-protocol translation, multi-account failover.
- **Observe — telemetry** attributes every hop with cost, tokens, latency, and outcome, exported to Prometheus or any OTLP backend.
- **Evaluate — the generic eval exchange** lets task-native tests, humans, enterprise systems, or an external agentic judge submit the same versioned outcome contract. BitRouter admits, disputes, and snapshots evidence; it does not pretend one bundled judge is universal.
- **Compile — the policy compiler** turns a frozen admitted-evidence snapshot into a deterministic, certificate-backed `policy-lock.yaml` candidate (an npm-style manifest/lock split, git-owned). Review and publication are explicit; the evidence database never changes a live route.

You choose what the external evaluator measures — cost, latency, quality, or a private objective — while the active lock remains the only authority for live policy routing.

## Benchmarks

Today **cost** is the validated objective: on Terminal-Bench 2.1, `gpt-5.5` with BitRouter cut cost **32.8%** at near-parity accuracy (−1.1 pp), by offloading routine steps to a cheaper model. Latency and accuracy objectives — and more base models — are landing next.

| Base model | Cost vs baseline | Latency vs baseline | Accuracy vs baseline |
| --- | --- | --- | --- |
| `gpt-5.5` | **−32.8%**¹ | coming soon | coming soon |
| `gpt-5.6` | coming soon | coming soon | coming soon |
| `claude-opus-5` | coming soon | coming soon | coming soon |
| `claude-sonnet-5` | coming soon | coming soon | coming soon |
| `claude-fable-5` | coming soon | coming soon | coming soon |

¹ Cost-optimization run on Terminal-Bench 2.1: −32.8% zero-cache imputed cost (audited range 28.6–32.8% by cache share) at near-parity accuracy, −1.1 pp (76.1% vs 77.3%, within single-attempt noise).

This is a mechanism study under a modified protocol, not a Terminal-Bench leaderboard submission — read the [experiment limitations](benchmarks/001-2026-07-10-tbench-v2.1-codex-gpt55-kimi-k27.md#limitations) before citing the numbers. Full reports live in [`benchmarks/`](benchmarks/); complete traces, tool calls, usage, policy decisions, configs, and checksums are in the [`BitRouterAI/benchmarks`](https://huggingface.co/datasets/BitRouterAI/benchmarks) dataset.

## Comparison

Every gateway below routes model calls. BitRouter is the only one that also makes **tools and agents** routable, and optimizes the whole **loop** rather than a single call.

|  | **BitRouter** | **OpenRouter** | **LiteLLM** | **TensorZero** | **Portkey** | **Bifrost** |
| --- | --- | --- | --- | --- | --- | --- |
| **Routable primitives** | Models + tools + **agents** (MCP + ACP) | Models | Models + tools (MCP) | Models | Models + tools (MCP) | Models + tools (MCP) |
| **Routing key** | **The loop step** (last tool called) | Model name | Model + request tags | Model name | Model + metadata | Model name |
| **Optimizes** | The **loop**, multi-objective (cost today) | Static routing | Static routing | The model | Static routing | Static routing |

_All but OpenRouter are open-source and self-hostable; BitRouter and TensorZero are Rust._

## What BitRouter is not

- **Not a static gateway** — it observes routed agent loops and compiles admitted external outcomes into a git-owned `policy-lock.yaml` you can read, diff, and revert. The live route never changes implicitly between publications.
- **Not an orchestration framework** — it doesn't define your agent's control flow, steps, or state; it routes the calls, tools, and sub-agents your loop already makes.
- **Not an agent harness** — it runs *under* Claude Code, Codex, and the rest, not instead of them.

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
+ OPENAI_BASE_URL=http://localhost:4356/v1    # all providers, automatic failover
```

### CLI

BitRouter runs as a local daemon — start it with your own keys or a Cloud sign-in.

**Bring your own keys (BYOK)** — auto-detected from the environment, no config file needed:

```bash
export OPENAI_API_KEY=sk-...    # ANTHROPIC_API_KEY / GEMINI_API_KEY also work
bitrouter start                 # proxy running at http://localhost:4356
```

**Or sign in to BitRouter Cloud** — use browser OAuth interactively or store an existing API key in CI:

```bash
bitrouter cloud login           # RFC 8628 device flow against api.bitrouter.ai
bitrouter cloud login --api-key "$BITROUTER_API_KEY"  # non-interactive CI login
bitrouter start                 # `bitrouter` provider auto-enables once signed in
```

The same credential also drives a [`gh api`](https://cli.github.com/manual/gh_api)-style raw client—no daemon required:

```bash
bitrouter cloud api /v1/models
bitrouter cloud api /v1/chat/completions --input request.json
```

Point your agent runtime at `http://localhost:4356` and any available provider is live. For advanced routing rules, guardrails, or multi-account failover, scaffold a config with `bitrouter init` (writes `./bitrouter.yaml`).

```bash
bitrouter start / stop / restart        # daemon lifecycle
bitrouter status --watch                # live request stream + spend
bitrouter route <model>                 # trace how a model name resolves
bitrouter key sign --user <id>          # mint a scoped brvk_ API key
bitrouter cloud keys list               # manage API keys
bitrouter cloud usage                   # inspect spend and tokens
bitrouter cloud billing balance         # check credits
bitrouter cloud api /v1/models          # call Cloud APIs directly
```

See [`docs/CLI.md`](docs/CLI.md) for the full command reference, flags, and config resolution.

### Agent Skill

BitRouter ships an [Agent Skill](https://agentskills.io) — `/bitrouter` — so AI
coding agents can install, configure, migrate to, and troubleshoot BitRouter on
their own. It lives in this repo at [`skills/bitrouter/`](skills/bitrouter/), kept in sync
with the code.

```bash
npx skills add bitrouter/bitrouter    # via the generic skills CLI
# ...or add this repo as a plugin marketplace in Claude Code / Codex
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

## Workflow templates

Ready-made **policy specs** for common agentic workflows start in [`templates/auto-router/`](templates/auto-router/): a conservative `@auto` / `@auto:cost` ladder using GPT-5.6 as the strong tier, Kimi K3 as balanced, and DeepSeek V4 Pro as economy. Treat it as a starting point and evaluate it against your own loop before publishing a live policy.

## Models & providers

BitRouter routes to a *model*, not a provider. Each family below is served by many providers — its own lab, hyperscalers (AWS Bedrock, Alibaba Cloud), gateways (OpenRouter, OpenCode), and serverless clouds — and BitRouter picks the cheapest route per call. **Bring your own key** to any of them, or use one **BitRouter Cloud** account with no keys at all.

| Lab      | Latest models                    |
| -------- | -------------------------------- |
| DeepSeek | DeepSeek V4 Flash 0731 / V4 Pro  |
| Alibaba  | Qwen3.8 Max / Qwen3.7 Max        |
| Moonshot | Kimi K3 / K2.7 Code              |
| Z.ai     | GLM-5.2 / 5.1                    |
| MiniMax  | MiniMax M3 / M2.7                |
| Xiaomi   | MiMo V2.5 Pro / V2.5             |
| StepFun  | Step 3.7 Flash / 3.5 Flash       |

Plus every frontier model from OpenAI, Anthropic, Google, and xAI — over your own keys, a subscription sign-in (Claude Pro/Max, GitHub Copilot, ChatGPT Codex), or BitRouter Cloud. Full catalog in the [registry/](registry/).

## Harness integrations

Any agent runtime that speaks OpenAI or Anthropic APIs works with BitRouter out of the box — set `OPENAI_BASE_URL=http://localhost:4356/v1` and you're done. For the four harnesses below, `bitrouter launch` does the wiring for you: it starts the harness's own native TUI with its traffic already pointed at the daemon, and never edits the harness's config files.

| Harness | Launch with | How BitRouter routes it |
| ------- | ----------- | ----------------------- |
| Claude Code | `bitrouter launch -a claude` | Child env overrides (`ANTHROPIC_BASE_URL`) — see the [LLM gateway guide](https://code.claude.com/docs/en/llm-gateway) for the manual form |
| OpenAI Codex | `bitrouter launch -a codex` | One-shot `-c` overrides — see [custom model providers](https://developers.openai.com/codex/config-advanced#custom-model-providers) for the manual form |
| OpenCode | `bitrouter launch -a opencode` | Synthesized `OPENCODE_CONFIG`; models via [models.dev](https://github.com/anomalyco/models.dev) |
| Pi-Agent | `bitrouter launch -a pi` | Synthesized `PI_CODING_AGENT_DIR` — see the [model configuration guide](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md) for the manual form |

`launch` supports these four because every promise it makes — routing, gateway injection, and the hosted terminal below — has to be re-verified per harness against upstream releases nobody controls. Four is a surface that stays honest.

**Other runtimes still work**, just not through `launch`:

| Runtime | How to use it |
| ------- | ------------- |
| Hermes Agent | Run it directly, or use the native [hermes-bitrouter-plugin](https://github.com/bitrouter/hermes-bitrouter-plugin) |
| OpenClaw | Run it directly, or use the native [bitrouter-openclaw](https://github.com/bitrouter/bitrouter-openclaw) plugin |
| Grok CLI | Run it directly on your SuperGrok session — the daemon borrows that session separately as the `supergrok` **provider** |
| Antigravity | Run it directly on your Google session — borrowed separately as the `google-ai` **provider** |

Headless ACP sub-agents use `bitrouter spawn` instead. The full provider and harness catalog lives in [github.com/bitrouter/bitrouter/registry](https://github.com/bitrouter/bitrouter/tree/main/registry).

### See what it's costing you

`bitrouter status --watch` is a live view of the router: a newest-first stream of settled requests — the provider that **actually** served, tokens, cost, latency — over today's spend and request rate. It reads the metering store directly, so it works even with the daemon stopped. Piped, it prints one snapshot and exits, so it scripts.

```bash
bitrouter status --watch          # live
bitrouter status --watch | less   # one snapshot
```

`bitrouter launch --tui` puts that same readout on a status row pinned under the harness, so cost is visible without leaving the agent. It is **opt-in**, and worth knowing why: hosting the harness inside BitRouter's terminal moves scrollback from your terminal to BitRouter, so terminal search stops finding agent output. Plain `launch` stays the daily driver.

## Features

Beyond the gateways above, the production controls for running agents unattended:

- **Multi-account failover + load-balancing** — reroute mid-run; a rate-limit at file 140 never re-pays for files 1–139
- **Virtual keys (`brvk_`)** scoped per agent or user — no agent holds an upstream key
- **Per-agent spend caps + loop guards** to contain runaway cost
- **Injection + output guardrails** at the router, before requests leave your network
- **Zero-config auto-detection** + custom OpenAI-/Anthropic-compatible providers

## Talk to founders

**[Try BitRouter Cloud →](https://cloud.bitrouter.ai)** or reach out directly:

Want a first-party provider integration, or building an open-source agent/harness? Email [kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai) or [book a meeting](https://cal.com/kelsenliu) — open-source builders get **up to 50% off** for you and your community.

## Development

- [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) — workspace architecture and SDK internals
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution workflow, issue reporting, and provider updates
- [`CLAUDE.md`](CLAUDE.md) — guidance for AI coding agents working in this repository
- [`skills/`](skills/) — the `/bitrouter` Agent Skill (source of truth)

## Star History

<a href="https://www.star-history.com/?type=date&repos=bitrouter%2Fbitrouter">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/chart?repos=bitrouter/bitrouter&type=date&theme=dark&legend=top-left&sealed_token=x3Lz0HqHPkyoGN8dh_Jdtkc-5lJ4iA_8eOmldMrXMyhVq7WCOxS03oBNGQXOxM962xv1AUhdyLKAtz6d1XK9ZSWUGHHd8HAWjEU44sXlwWT_I7iXPaTfizw7aDpxA-PrsxC3Jd5IN-SWladKBNoK2weKlIKVs9JQax5sbImPT9srpEeKzbYt_VsafBwd" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/chart?repos=bitrouter/bitrouter&type=date&legend=top-left&sealed_token=x3Lz0HqHPkyoGN8dh_Jdtkc-5lJ4iA_8eOmldMrXMyhVq7WCOxS03oBNGQXOxM962xv1AUhdyLKAtz6d1XK9ZSWUGHHd8HAWjEU44sXlwWT_I7iXPaTfizw7aDpxA-PrsxC3Jd5IN-SWladKBNoK2weKlIKVs9JQax5sbImPT9srpEeKzbYt_VsafBwd" />
   <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=bitrouter/bitrouter&type=date&legend=top-left&sealed_token=x3Lz0HqHPkyoGN8dh_Jdtkc-5lJ4iA_8eOmldMrXMyhVq7WCOxS03oBNGQXOxM962xv1AUhdyLKAtz6d1XK9ZSWUGHHd8HAWjEU44sXlwWT_I7iXPaTfizw7aDpxA-PrsxC3Jd5IN-SWladKBNoK2weKlIKVs9JQax5sbImPT9srpEeKzbYt_VsafBwd" />
 </picture>
</a>

## License

Licensed under the [Apache License 2.0](LICENSE).
