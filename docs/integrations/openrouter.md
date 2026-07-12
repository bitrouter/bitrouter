---
title: OpenRouter
description: Bring your OpenRouter key into BitRouter — one aggregator key, hundreds of models, behind the registry.
sourceHash: 177d9a2dbc0eff17580a51b6a4fa1440253c8a787a604e413b84a60c32a659dd
---

[OpenRouter](https://openrouter.ai) is an OpenAI-compatible aggregator that fronts hundreds of models behind one key. BitRouter has a built-in `openrouter` provider, so bringing your key in is a single block in `bitrouter.yaml` — and from there OpenRouter's catalog joins the rest of your [registry](/docs/concepts/models), with selection and fallback on top.

<Callout type="info">
**Migrating off OpenRouter entirely?** If you want to *replace* OpenRouter rather than route through it, see [Migrate from OpenRouter](/docs/guides/migrate-from-openrouter) — it's a base-URL-and-key swap.
</Callout>

## Prerequisites

- BitRouter installed, with a `bitrouter.yaml` ([scaffold one](/docs/integrations/models#scaffold-a-config) with `bitrouter init`).
- An OpenRouter API key from [openrouter.ai/keys](https://openrouter.ai/keys), exported as `OPENROUTER_API_KEY`.

## Add OpenRouter to BitRouter

The `openrouter` provider ships with the right `api_base` and Bearer auth built in — you only supply the key and the models you want:

```yaml
# bitrouter.yaml
providers:
  openrouter:
    api_key: ${OPENROUTER_API_KEY}     # resolved from the environment at load
    models:
      - id: openai/gpt-4o
      - id: anthropic/claude-sonnet-4-6
      - id: google/gemini-2.5-pro
```

Each `models` id is an [OpenRouter model id](https://openrouter.ai/models). The base URL (`https://openrouter.ai/api/v1`) and `chat_completions` protocol are the provider's defaults, so you don't need to set them.

<Callout type="info">
**Attribution headers.** OpenRouter's optional `HTTP-Referer` / `X-Title` leaderboard headers are not injected. If you want them, add them as per-provider header overrides in `bitrouter.yaml`.
</Callout>

## Route to it

```bash
bitrouter route openrouter:openai/gpt-4o
```

Then [start BitRouter and send a request](/docs/integrations/models#start-bitrouter-and-send-a-request). Use the provider-qualified id `openrouter:openai/gpt-4o` to pin the request to OpenRouter, or the bare model name to let BitRouter cascade across sources.

## Learn more

- [OpenRouter — API reference](https://openrouter.ai/docs/api-reference/overview)
- [Provider selection](/docs/features/provider-selection) · [Model fallback](/docs/features/model-fallback)
