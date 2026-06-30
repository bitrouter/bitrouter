---
title: OpenClaw
description: Route OpenClaw's messaging gateway through BitRouter as an OpenAI-compatible model provider.
sourceHash: cb0f07c8db3b5bdbd0bdfd4dc724e60d0512087582f5226655f70f5be1244bd8
---

OpenClaw is a self-hosted gateway that bridges messaging platforms — WhatsApp, Telegram, Slack, Discord, and more — to an LLM. Configure BitRouter as its model provider and every channel routes across the whole [registry](/docs/concepts/models).

## Prerequisites

- BitRouter running — local proxy at `http://127.0.0.1:4356`, or [BitRouter Cloud](/docs/get-started/quickstart) at `https://api.bitrouter.ai`.
- OpenClaw installed. See the [OpenClaw docs](https://docs.openclaw.ai/) for setup.

## Point OpenClaw at BitRouter

BitRouter is an OpenAI-compatible endpoint, so it goes straight into your OpenClaw config file as a provider — no plugin required. Add a `bitrouter` entry under `models.providers` and reference it as the default model:

```json5
{
  agents: {
    defaults: {
      model: { primary: "bitrouter/openai/gpt-4o" },
    },
  },
  models: {
    providers: {
      bitrouter: {
        baseUrl: "http://127.0.0.1:4356/v1",
        apiKey: "local-placeholder",   // any value for the local proxy
        api: "openai-completions",
        models: [{ id: "openai/gpt-4o", contextWindow: 128000, maxTokens: 8192 }],
      },
    },
  },
}
```

Then start the gateway:

```bash
openclaw gateway
```

<Callout type="info">
For **Cloud**, set `baseUrl` to `https://api.bitrouter.ai/v1` and use your BitRouter key. The local proxy ignores `apiKey`, so a placeholder is fine.
</Callout>

## Pick a model

Each provider model `id` is a registry id in `provider/model` form (`openai/gpt-4o`, `anthropic/claude-sonnet-4-6`, …), optionally with a `:cost` / `:latency` variant. In OpenClaw's `model.primary` it's prefixed with the provider id you chose — e.g. `bitrouter/openai/gpt-4o`. See [Models](/docs/concepts/models).

## Learn more

- [OpenClaw — model providers](https://docs.openclaw.ai/concepts/model-providers)
- [Model fallback](/docs/features/model-fallback)
