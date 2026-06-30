---
title: Provider Selection
description: Choose how BitRouter ranks providers when a model is served by more than one — by cost, latency, or throughput.
sourceHash: 08b021ccc3b9394f871bb91d23e8b05efba77e223208d9a19f2ba08e51b49f72
---

Most models on BitRouter are served by more than one provider. When you request `openai/gpt-4o`, BitRouter has to pick which registered endpoint to send the request to. By default it uses a balanced score; with the `provider.sort` field, you choose the policy explicitly.

<Callout type="warn">
**Today, choose a policy with the [`model:<profile>` suffix](/docs/features/model-variants)** — e.g. `openai/gpt-4o:latency`. The `provider.sort` request-body field described on this page is planned and **not yet active**; the suffix is the supported surface.
</Callout>

There are three policies. Pick whichever matters most for the request.

## The three policies

| Policy | Optimizes for | Tie-break |
| --- | --- | --- |
| `cost` | Lowest cost per request, computed against your prompt and expected completion tokens at current upstream pricing. | Higher uptime → lower error rate → provider ID. |
| `latency` | Lowest observed p50 TTFT (time to first token) over the rolling 1-hour window. | Higher throughput → higher uptime → provider ID. |
| `throughput` | Highest observed output tokens per second over the rolling 1-hour window. | Lower TTFT → higher uptime → provider ID. |

Telemetry is refreshed every minute. The same data is visible on each model's page in the registry.

## Quick example

```bash
curl http://127.0.0.1:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openai/gpt-4o",
    "provider": { "sort": "latency" },
    "messages": [{"role": "user", "content": "Translate to French: Hello."}]
  }'
```

The same `provider.sort` field works on `/v1/messages` (Anthropic) and `/v1beta/models/{model}:generateContent` (Google).

## BYOK providers come first

If you've [added an external key](/docs/features/byok) for a provider, BitRouter prefers that provider for any model it can serve — ahead of every non-BYOK provider, regardless of `provider.sort`. Your BYOK key bills against your own account at upstream list price with no rev share, and you opted into that provider explicitly; honoring that opt-in by default is the only choice that doesn't surprise you later.

Within the BYOK-eligible set, the `provider.sort` policy still applies. So `provider.sort: "latency"` plus BYOK keys for OpenAI and Anthropic ranks those two by TTFT first, and falls back to non-BYOK providers (also ranked by latency) only if both BYOK paths fail.

In **local mode** this section is a no-op — every provider is BYOK by definition.

## Default behavior

When `provider` is not set, BitRouter ranks by a **balanced score** — a weighted combination of cost, latency, throughput, and uptime, with low-uptime providers filtered out. This is the right default for most agents; specify a policy only when one axis dominates.

<Callout type="info">
**The default is not stable across versions.** The weights in the balanced score are tuned over time as we learn from real traffic. If you need a fixed, reproducible policy — for cost reporting, SLO tracking, or A/B tests — set `provider.sort` explicitly.
</Callout>

## How selection composes with fallback

[Model fallback](/docs/features/model-fallback) and provider selection are independent layers:

1. For each model in your `models` list (or the single `model` if no fallback), BitRouter applies your `provider.sort` policy to pick the best provider.
2. If the chosen provider fails in a way that doesn't surface to the caller (rate limit, 5xx), BitRouter retries on the **next-ranked provider of the same model** before falling through to the next model in the list.
3. The same `provider.sort` policy applies to every model in the fallback list — you cannot specify a different policy per model.

Concretely: `models: ["openai/gpt-4o", "anthropic/claude-sonnet-4-6"]` with `provider.sort: "cost"` evaluates the cheapest provider of GPT-4o first, then the cheapest provider of Sonnet, then surfaces the error.

## When metrics are tied

If two providers price the same prompt identically, the higher-uptime one wins. If uptime is also tied, the lower-error-rate one wins. If everything is tied, BitRouter sorts by provider ID lexicographically — deterministic and audit-friendly, but it does not "load balance." If even spend distribution across tied providers matters for your workload, post a use case to [Discord](https://discord.gg/G3zVrZDa5C); we'll add a `provider.balance` knob if there's demand.

## What's not here

OpenRouter exposes a much larger surface — `provider.order`, `provider.allow_fallbacks`, `provider.require_parameters`, `provider.data_collection`, `provider.ignore`, `provider.quantizations`, and more. We are deliberately keeping this to one knob with three values until usage tells us otherwise. Two equivalent expressions if you're migrating:

- **Pin to a specific provider** — use the provider-prefixed model ID, e.g. `model: "anthropic-direct/anthropic/claude-sonnet-4-6"`.
- **Exclude a provider** — omit it from your workspace's registry allowlist, not the request body.

If a missing knob is blocking a real workload, file an issue on [bitrouter](https://github.com/bitrouter/bitrouter).
