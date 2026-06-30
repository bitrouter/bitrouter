---
title: Hermes
description: Route Nous Research's Hermes agent through BitRouter by setting a custom model endpoint.
sourceHash: e2af206e53c098a3a99612591e241869057d46d6cdbf5418686ed8d8d59da7f0
---

Hermes is Nous Research's self-improving agent. It accepts a custom OpenAI-compatible model endpoint, so you can point it at BitRouter and route its model calls across the whole [registry](/docs/concepts/models).

## Prerequisites

- BitRouter running — local proxy at `http://127.0.0.1:4356`, or [BitRouter Cloud](/docs/get-started/quickstart) at `https://api.bitrouter.ai`.
- Hermes installed. See [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent) for setup.

## Point Hermes at BitRouter

Set the model provider to `custom` and the base URL to BitRouter's `/v1` root in `~/.hermes/config.yaml`:

```yaml
model:
  provider: custom
  base_url: http://127.0.0.1:4356/v1
  api_key: local-placeholder    # any value for the local proxy
  model: openai/gpt-4o
```

The same can be set from the CLI:

```bash
hermes config set model.base_url http://127.0.0.1:4356/v1
```

<Callout type="info">
For **Cloud**, set `base_url` to `https://api.bitrouter.ai/v1` and use your BitRouter key. The local proxy ignores `api_key`, so a placeholder is fine. Confirm the exact field names against the [Hermes configuration docs](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/configuration.md) for your version.
</Callout>

<Callout type="info">
**Want to run on Hermes models specifically?** Nous's Hermes models are in the registry too — request `nousresearch/hermes-4-405b` (or any provider-qualified Hermes id) as the model and BitRouter routes to whichever provider serves it.
</Callout>

## Pick a model

The `model` field takes any registry id in `provider/model` form, optionally with a `:cost` / `:latency` variant. See [Models](/docs/concepts/models).

## Learn more

- [Hermes — configuration](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/configuration.md)
- [Model fallback](/docs/features/model-fallback) · [Provider selection](/docs/features/provider-selection)
