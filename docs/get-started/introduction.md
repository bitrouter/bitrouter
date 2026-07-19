---
title: Introduction
description: An open-source, adaptive LLM gateway that optimizes your production agent loops by making models, tools, and agents all routable primitives — with zero harness changes.
sourceHash: d98ac30ce61a9f4c773cb4c3d2cb2e7f5123012dacae3b17c6f32904ecdeeefb
---

## What is BitRouter?

BitRouter is an **open-source, adaptive LLM gateway and router** for your production agent loops. It's a single local binary that gives any agent one endpoint to route its model calls, tools, and sub-agents to the best path that still reaches the goal — with **zero harness changes** — tightening that routing every run. **Today it optimizes for cost**: point your runtime at it and every step of every loop stops billing at frontier prices by default. Latency and accuracy objectives are next.

It runs anywhere your agent runs, with no dependencies to install, and is operated as a permissionless network where any provider can register and any agent can connect. The **Core** is **open-source under Apache 2.0 and self-hostable for free** — bring your own keys or run a local model and you owe nothing. **Cloud** is an optional hosted layer that adds managed providers, agentic payments, and account-wide policies on top — [install either mode](/docs/get-started/configuration) in under a minute. Browse the full [models & pricing](/docs/get-started/supported-models) catalog.

## Three primitives, one gateway

An agentic loop consumes three things. Most routers govern only the first — BitRouter makes all three routable, observable, and governed:

- **Models** — route LLM calls across providers, accounts, and wire protocols (OpenAI, Anthropic, Google). See [Models](/docs/concepts/models).
- **Capabilities** — an **MCP gateway** and an **AgentSkills gateway**: tools and skills become governed, routable resources, not hardcoded endpoints. See [Tools](/docs/concepts/tools).
- **Agents** — an **ACP gateway**: sub-agents are first-class routable primitives, so a task can go to the sub-agent that best fits the objective — just as a call routes to the best-fit model. See [Agents](/docs/concepts/agents).

Optimizing a loop isn't just model selection — it's the model, the tool, *and* the sub-agent that best serve the loop's objective. Each loop gets a [policy](/docs/concepts/policy) that BitRouter tunes from live signal through a continuous act → observe → evaluate → learn loop, so it improves the longer it runs. Cost is what it optimizes today; latency and accuracy are next.

## Why agents run on BitRouter

Four mechanisms, built into the router — not bolted on per agent:

- **Reliability** — mid-run reroute across providers, automatic retries, and model/provider fallback keep long loops alive through outages and `429`s. Failed requests aren't billed. See [Model Fallback](/docs/features/model-fallback).
- **Observability** — every agent, model, and step traced, with cost attributed **per run**, exported over OTLP to any backend. See [OpenTelemetry](/docs/features/opentelemetry).
- **Security** — regex guardrails and rate limits enforced once, at the router, for every agent, plus per-agent [KYA](/docs/features/payment) identity. See [Guardrails](/docs/features/guardrails).
- **Efficiency** — adaptive routing matches each call, tool, and agent to the right path for the objective; today that means trivial calls stop billing at frontier prices, with latency and accuracy next.

## Next steps

BitRouter is a drop-in proxy for any runtime that supports a custom OpenAI or Anthropic base URL. [Configuration](/docs/get-started/configuration) gets you routing in under a minute; per-runtime recipes (Claude Code, OpenClaw, Codex, and more) live in [Integrations](/docs/integrations), and machine-readable docs in [AI Resources](/docs/ai-resources).

Wondering how BitRouter compares to OpenRouter, LiteLLM, and others, or whether to self-host or use Cloud? See the [FAQ](/docs/get-started/faqs).
