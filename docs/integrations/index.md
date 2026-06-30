---
title: Overview
description: Plug models, tools, and agent harnesses into BitRouter.
sourceHash: 04eb19e177d2ad12fc475f1bfc45165f8993c065d097985acfdad627583a6c84
---

Every integration is the same move: point something at BitRouter's endpoint (local proxy at `http://127.0.0.1:4356`, or Cloud at `https://api.bitrouter.ai`) and address models by their `provider/model` id. From there you get the whole [registry](/docs/concepts/models), provider selection, and [fallback](/docs/features/model-fallback) underneath. New to BitRouter? Start with the [Quick Start](/docs/get-started/quickstart).

Integrations come in three kinds:

## Models

Where your tokens come from — a subscription you already pay for, an aggregator key, or a model you serve yourself.

<Cards>
  <Card title="Models overview" href="/docs/integrations/models" description="The three shapes of model source" />
  <Card title="Claude subscription" href="/docs/integrations/claude-subscription" description="Route your Claude Pro/Max plan — OAuth, no key" />
  <Card title="Codex subscription" href="/docs/integrations/codex-subscription" description="Route your ChatGPT plan via the Codex backend" />
  <Card title="OpenRouter" href="/docs/integrations/openrouter" description="Bring an OpenRouter key" />
  <Card title="Ollama" href="/docs/integrations/ollama" description="Run open models locally · :11434" />
  <Card title="vLLM" href="/docs/integrations/vllm" description="High-throughput GPU serving · :8000" />
</Cards>

## Tools

Web-search and retrieval providers your models call through BitRouter's built-in [web search](/docs/features/websearch).

<Cards>
  <Card title="Tools overview" href="/docs/integrations/tools" description="How search providers back the web_search tool" />
  <Card title="Exa" href="/docs/integrations/exa" description="Neural / semantic web search" />
  <Card title="Parallel" href="/docs/integrations/parallel" description="Web search + research API for agents" />
  <Card title="Firecrawl" href="/docs/integrations/firecrawl" description="Search + scrape to LLM-ready markdown" />
  <Card title="Tavily" href="/docs/integrations/tavily" description="Search built for agents and RAG" />
</Cards>

## Harnesses

Agent runtimes that drive their own loop — point one at BitRouter and run it on any model.

<Cards>
  <Card title="Harnesses overview" href="/docs/integrations/harnesses" description="Harness vs. model source" />
  <Card title="Claude Code" href="/docs/integrations/claude-code" description="Anthropic Messages via ANTHROPIC_BASE_URL" />
  <Card title="Codex" href="/docs/integrations/codex" description="Custom provider in ~/.codex/config.toml" />
  <Card title="OpenCode" href="/docs/integrations/opencode" description="Provider block in opencode.json" />
  <Card title="Hermes" href="/docs/integrations/hermes" description="Nous Research's self-improving agent" />
  <Card title="OpenClaw" href="/docs/integrations/openclaw" description="Messaging gateway across many channels" />
  <Card title="Pi" href="/docs/integrations/pi" description="Minimal terminal coding harness via models.json" />
</Cards>

## Switching gateways

<Cards>
  <Card title="Migrate from LiteLLM" href="/docs/guides/migrate-from-litellm" description="Swap your gateway, keep your code" />
  <Card title="Migrate from OpenRouter" href="/docs/guides/migrate-from-openrouter" description="Change base URL and key" />
  <Card title="Migrate from TensorZero" href="/docs/guides/migrate-from-tensorzero" description="Drop the LLMOps stack for a single binary" />
</Cards>
