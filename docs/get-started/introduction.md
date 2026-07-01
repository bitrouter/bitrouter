---
title: Introduction
description: An open-source agentic LLM gateway that cost-optimizes your production agent loops by making models, tools, and agents all routable primitives — with zero harness changes.
sourceHash: d98ac30ce61a9f4c773cb4c3d2cb2e7f5123012dacae3b17c6f32904ecdeeefb
---

## What is BitRouter?

BitRouter is an **open-source agentic LLM gateway and router that cost-optimizes your production agent loops**. It's a single local binary that gives any agent one endpoint to route its model calls, tools, and sub-agents to the cheapest path that still reaches the goal — with **zero harness changes**. Point your runtime at it and every step of every loop stops billing at frontier prices by default.

It runs anywhere your agent runs, with no dependencies to install, and is operated as a permissionless network where any provider can register and any agent can connect. The [Core](/docs/get-started/self-hosted-and-cloud) is **open-source under Apache 2.0 and self-hostable for free** — bring your own keys or run a local model and you owe nothing. [Cloud](/docs/get-started/self-hosted-and-cloud) is an optional hosted layer that adds managed providers, agentic payments, and account-wide policies on top. Browse the full [models & pricing](/docs/get-started/models) catalog.

## Three primitives, one gateway

An agentic loop consumes three things. Most routers govern only the first — BitRouter makes all three routable, observable, and cost-governed:

- **Models** — route LLM calls across providers, protocols, and accounts (the classic router, cross-protocol). See [Model routing](/docs/concepts/models).
- **Tools** — an **MCP gateway** and an **AgentSkills gateway**: tools and skills become governed, routable resources instead of hardcoded endpoints. See [Tools](/docs/concepts/tools).
- **Agents** — an **ACP gateway**: sub-agents are first-class, so you hand a task to a cheaper agent the same way you route a call to a cheaper model. See [Agents](/docs/concepts/agents).

Cost optimization isn't just model selection — it's the cheapest model, the cheapest tool, and the cheapest sub-agent that still gets the loop to its goal.

## The self-improving loop

BitRouter wraps your agent loop in a second loop. Each loop gets its own [policy](/docs/concepts/policy) — a spec that declares how its calls, tools, and agents route — and BitRouter runs a continuous **observe → evaluate → act** cycle against it:

- **Observe** — every model, tool, and agent call, with cost and outcome attributed to the hop.
- **Evaluate** — score each run against the loop's goal.
- **Act** — update the policy. Let an agent self-tune it from the eval signal, or edit it yourself.

The result is a loop that gets cheaper the longer it runs in production — without re-paying frontier prices for work that never needed them.

## Why agents run on BitRouter

Four mechanisms, built into the router — not bolted on per agent.

### Reliability — one provider fails, your agent run doesn't

BitRouter reroutes across providers mid-run, transparently — your agent never sees the failed call. Automatic retries with exponential backoff, model and provider fallbacks, connection reuse, and request-level idempotency keep long agent loops alive through outages and `429`s. Failed requests aren't billed. See [Model Fallback](/docs/features/model-fallback) and [Provider Selection](/docs/features/provider-selection).

### Observability — trace every hop, not just every request

Full call-chain visibility: every agent, every model, every step, with cost attributed **per run** rather than per month. BitRouter is OpenTelemetry-native — traces and metrics export over OTLP to any backend you run, and `bitrouter observe status` reports the live exporter state. See [OpenTelemetry](/docs/features/opentelemetry).

### Security — guardrails for every agent, configured once

Regex guardrails that redact or block risky prompts and output, plus rate limits — enforced at the router, once, for every agent, with no application-level changes. Combined with per-agent [KYA](/docs/features/payment) identity, an autonomous agent holding your keys stops being an unsupervised attack surface. See [Guardrails](/docs/features/guardrails).

### Efficiency — not every call needs your strongest model

Most calls in a run are trivial — a lookup, a format, a yes/no. BitRouter matches each call to the right model by task complexity with price-aware routing, so you stop billing simple calls at frontier prices. The savings compound across every run.

## The foundation

- **Universal LLM API** — One binary, four protocols: OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and Google Generative AI. Talk to any LLM through your preferred protocol, and route cross-protocol (OpenAI ↔ Anthropic).
- **Free BYOK** — Bring your own provider keys at zero cost. BitRouter auto-detects keys from environment variables — no config file required. You can also point BitRouter at your own local model (Ollama, vLLM, LM Studio) for free — see [Local & private models](/docs/integrations/models).
- **MCP & ACP gateway** — Proxy [MCP](https://modelcontextprotocol.io) servers so agents can discover and call tools across hosts. [ACP](https://github.com/zed-industries/acp) support for agent identity, discovery, and task dispatch.
- **Agentic auth & payment** — KYA (Know-Your-Agent) identity and x402/MPP pay-per-use on the hosted service. Agents authenticate and pay autonomously — no credit cards, no prepaid credits, no invoices.
- **Open ecosystem** — Permissionless [provider registration](/docs/guides/register-as-a-provider). Any provider exposing an OpenAI- or Anthropic-compatible endpoint can join the network and be discovered by agents on it.

## How BitRouter compares

OpenRouter, LiteLLM, Portkey, and Bifrost all route LLM traffic, and TensorZero adds a model-optimization loop — but BitRouter is the only one that treats **models, tools, and agents as a single routable surface** and optimizes the whole production **loop** by cost, not just static model selection. It's open-source, self-hostable, and Rust-native, with automatic mid-run failover and sub-10ms routing overhead. See the full [Comparison](/docs/get-started/comparison).

## Why we're building this

Today's LLM agents lose hours of work to a single provider outage, rewrite integration code every time they swap models, ship risky outputs with no consistent way to redact or block them, and operate in the dark because each provider only shows its own slice. BitRouter survives outages with automatic fallback, lets agents swap models without code changes, redacts or blocks risky content at the proxy, and shows every call, cost, and error in one feed. The longer goal is an open, permissionless intelligence layer where agents discover, route to, and pay for their own resources — owned by the agents and operators using it, not a gateway company in the middle.

## Agent Runtimes

BitRouter is a drop-in proxy for any runtime that supports a custom OpenAI or Anthropic base URL — point it at `http://127.0.0.1:4356` and you're done.

Setup recipes for OpenClaw, Hermes Agent, Claude Code, and more live in the [Integrations](/docs/integrations).

For machine-readable docs and drop-in agent skills, see [AI Resources](/docs/ai-resources).
