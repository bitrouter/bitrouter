---
title: Overview
description: Bring any model source under BitRouter — a Claude or Codex subscription, an OpenRouter key, or a model you serve yourself.
sourceHash: 7be29fb8987333cd00928957202cd1263655a3e44205aef251f6c3d22aebfc2b
---

A **model source** is wherever your tokens actually come from. BitRouter puts every source behind one endpoint and one [registry](/docs/concepts/models), so an agent addresses models by their `provider/model` id and never sees the difference between a subscription, an aggregator, and a GPU in your closet.

There are three shapes of source, by how you authenticate:

<Cards>
  <Card title="Claude subscription" href="/docs/integrations/claude-subscription" description="Route your Claude Pro/Max plan — OAuth, no API key" />
  <Card title="Codex subscription" href="/docs/integrations/codex-subscription" description="Route your ChatGPT plan through the Codex backend — OAuth" />
  <Card title="OpenRouter" href="/docs/integrations/openrouter" description="Bring an OpenRouter key — one aggregator, hundreds of models" />
  <Card title="Ollama" href="/docs/integrations/ollama" description="Run open models locally · :11434 · no key" />
  <Card title="vLLM" href="/docs/integrations/vllm" description="High-throughput GPU serving · :8000" />
</Cards>

## How a source is wired

| Source | Auth | Where it's configured |
| --- | --- | --- |
| **Subscriptions** (Claude, Codex) | OAuth — your plan's login | `bitrouter login <provider>` (no key, no yaml) |
| **Aggregators / hosted** (OpenRouter, …) | Bring-your-own API key | A provider block in `bitrouter.yaml` |
| **Self-hosted** (Ollama, vLLM) | Usually none — loopback | A provider block in `bitrouter.yaml` |

Subscriptions skip `bitrouter.yaml` entirely: `bitrouter providers login anthropic` or `bitrouter providers login openai-codex` runs the plan's OAuth flow and stores the refreshing token for BitRouter to attach at request time. Everything else is a provider block — an `api_base`, an optional `api_key`, and the models that source serves.

```yaml
# bitrouter.yaml
providers:
  openrouter:                          # an id you pick (or a built-in id)
    api_base: https://openrouter.ai/api/v1
    api_key: ${OPENROUTER_API_KEY}     # resolved from the environment at load
    api_protocol:
      - "*": chat_completions          # upstream wire format
    models:
      - id: openai/gpt-4o
```

`providers` is a map keyed by an id you choose; `api_base` is the source's base URL; `api_protocol` is the upstream wire format (`chat_completions` for any OpenAI-compatible host — also the inferred default); each `models` entry is a model that source serves.

## Scaffold a config

Generate a starter `bitrouter.yaml` (defaults to `./bitrouter.yaml`):

```bash
bitrouter init
```

This writes a commented config with `skip_auth: true`, ready for a provider block. Use `-c <path>` to write it elsewhere, then follow your source's page to fill in the block. (Subscriptions need no config — just `bitrouter login`.)

## Start BitRouter and send a request

Once your source is wired — a provider block in place, or a subscription logged in — start the proxy. It listens on `127.0.0.1:4356` by default:

```bash
bitrouter
```

Then hit the OpenAI-compatible endpoint with a model id from your config (swap in your provider id / model):

```bash
curl http://localhost:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openrouter:openai/gpt-4o",
    "messages": [{"role": "user", "content": "Hello from BitRouter"}]
  }'
```

The bare model name also works — BitRouter auto-cascades it to whichever active source declares it. The provider-qualified form (`openrouter:openai/gpt-4o`) pins the request to that exact source.

<Callout type="info">
**Mix sources freely.** Declare a [virtual model](/docs/features/model-fallback) whose endpoints list a local source first and a hosted one second: requests run on your own hardware for free and fail over to the hosted model on error or overload — one model name, automatic failover.
</Callout>

For the concepts behind this — provider selection, fallback, and registry detection — see [Models](/docs/concepts/models). To serve models yourself, see the local-server integrations: [Ollama](/docs/integrations/ollama) and [vLLM](/docs/integrations/vllm).
