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

**The minimal interpretable model router that learns & adapts to your agent workflows.**

Point any OpenAI-compatible agent at your local BitRouter instance, choose `bitrouter/auto`, and keep working.

```diff
- OPENAI_BASE_URL=https://api.openai.com/v1
+ OPENAI_BASE_URL=http://localhost:4356/v1
```

Then set the request model to:

```text
bitrouter/auto
```

BitRouter runs locally and exposes an OpenAI-compatible API, so existing agents and tools can switch over with minimal changes.

You can also give your coding agent the `/bitrouter` skill to install, configure, migrate to, and troubleshoot BitRouter for you:

```bash
npx skills add bitrouter/bitrouter
```

> **[Try BitRouter Cloud →](https://cloud.bitrouter.ai):** Don’t want to run the router locally? Use `https://api.bitrouter.ai/v1` as your base URL instead.

## Core features

### Minimal core. Lightweight & extensible.

One Rust router sits between your agents and every model provider. Extend it
with providers, routing policies, tools, skills, and sub-agents without
rebuilding your workflow around a new agent runtime.

### Interpretable decisions. Under your control.

Routing policy lives in files you own. Routes are inspectable, changes are
diffable, and a live policy changes only when you explicitly publish it.

### A customizable router. Adapt it to your workflow.

Start with `bitrouter/auto`, then shape the models, rules, guardrails, and
evaluation objective around your own loop. BitRouter learns from the outcomes
you admit and proposes a new policy you can review, publish, or revert.

## Integrates with your agent stack

Keep the clients and workflows you already use:

- **Model APIs** — OpenAI Chat Completions and Responses, Anthropic Messages, and Google Gemini `generateContent`, with cross-protocol routing.
- **Coding agents** — built-in Codex and Claude support through `bro code`, native launchers, and ACP adapters.
- **Tools & skills** — aggregate configured MCP servers behind one endpoint, with tools and [SEP-2640](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2640) skills available through the same gateway.

## Benchmarks

On **Terminal-Bench 2.1**, BitRouter cut API cost by **40.9%** while achieving nearly the same reward as using `gpt-5.6-sol` alone.

| Configuration | Reward | Cost / trial | Cost savings |
| --- | ---: | ---: | ---: |
| `gpt-5.6-sol` only | **81.56%** | $0.830 | — |
| **BitRouter** | **81.25%** | **$0.490** | **40.9%** |
| Random model mix | 76.50% | $0.553 | 33.4% |

BitRouter dynamically routed between **GPT-5.6, Kimi K3, and DeepSeek V4 Flash**, retaining nearly all of GPT-5.6’s task performance while substantially reducing model cost.

The random baseline used the same model mix, showing that the savings come from **routing decisions—not simply using cheaper models**.

Results use the strict 80-task common-valid set and frozen API prices. This is a research benchmark, not an official Terminal-Bench submission.

See the [full study and limitations](benchmarks/002-2026-09-07-tbench-v2.1-router-random-study.md) and the [`BitRouterAI/benchmarks`](https://huggingface.co/datasets/BitRouterAI/benchmarks) dataset for reproducible results.

## Install

```bash
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/bitrouter/bitrouter/releases/latest/download/bitrouter-installer.sh | sh

# Homebrew
brew install bitrouter/tap/bitrouter

# Windows
powershell -ExecutionPolicy Bypass -Command "irm https://github.com/bitrouter/bitrouter/releases/download/v1.0.0-alpha.31/bitrouter-installer.ps1 | iex"

# npm
npm install -g bitrouter

# From source (Cargo)
cargo install bitrouter
```

## Quick start

`bro code` (aka. BitRouter Orchestrator) is BitRouter's CLI workspace for coding-agent conversations. On the
first run, `bro` guides you through setup and opens the default agent. Use
`bro code` directly when you want to choose an agent or resume a session.

```bash
bro                                  # first-run setup, then the default agent
bro code                             # open a conversation and choose an agent
bro code codex                       # start an interactive Codex ACP session
bro code claude                      # start an interactive Claude ACP session
bro run claude "summarize this repo" # run one headless agent turn
```

BitRouter discovers a compatible local coding-agent CLI automatically. Its
built-in Codex and Claude adapters require Node.js 22+ and `npx`; when the local
CLI is unavailable, the adapter uses its bundled worker.

Use the surrounding `bro` commands to inspect and control the router while you
work:

```bash
bro requests                      # settled requests and spend
bro route <model>                 # explain how a model name resolves
bro start                         # start the local router
bro stop                          # stop the local router
bro restart                       # restart the local router
bro init                          # scaffold advanced routing configuration
```

`bro claude` and `bro codex` launch each harness's native interface with
per-process BitRouter overrides. `bro code <agent>` keeps the conversation
inside BitRouter's interface; `bro run <agent>` is the headless equivalent.

See [`docs/CLI.md`](docs/CLI.md) for the complete command reference, session
controls, flags, and config resolution.


## Workflow recipes

Ready-made **policy specs** for common agentic workflows start in [`templates/auto-router/`](templates/auto-router/): a predictive `bitrouter/auto` / `bitrouter/auto:cost` ladder using GPT-5.6 as the strong tier, Kimi K3 as balanced, and DeepSeek V4 Pro as economy. Treat it as a starting point and evaluate it against your own loop before publishing a live policy.

## Models & providers

BitRouter routes to a *model*, not a provider. Each model below is served by many
providers — its own lab, hyperscalers (AWS Bedrock, Alibaba Cloud), gateways
(OpenRouter, OpenCode), and serverless clouds — and BitRouter picks the cheapest
route per call. The bars are those routes: the more providers serve a model, the
more room the router has to move.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/bitrouter/bitrouter/charts/dist/charts/registry-by-lab-dark.svg" />
    <source media="(prefers-color-scheme: light)" srcset="https://raw.githubusercontent.com/bitrouter/bitrouter/charts/dist/charts/registry-by-lab.svg" />
    <img alt="BitRouter model catalog grouped by lab; bar length is the number of providers routing each model" src="https://raw.githubusercontent.com/bitrouter/bitrouter/charts/dist/charts/registry-by-lab.svg" />
  </picture>
</p>

**Bring your own key** to any of them, or use one **BitRouter Cloud** account with
no keys at all. Frontier models from OpenAI, Anthropic, Google, and xAI also route
over a subscription sign-in (Claude Pro/Max, GitHub Copilot, ChatGPT Codex)
instead of a key.

The chart is generated from [`dist/registry/`](dist/registry/) on every catalog
change — it is never hand-maintained, so it cannot drift from what the router
actually resolves. Full catalog in [`registry/`](registry/).

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
