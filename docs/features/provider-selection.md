---
title: Provider Selection
description: On BitRouter Cloud, choose how providers are ranked when a model is served by more than one — by cost, latency, or throughput.
sourceHash: 08b021ccc3b9394f871bb91d23e8b05efba77e223208d9a19f2ba08e51b49f72
---

Most Cloud models on BitRouter are served by more than one provider. When you request `openai/gpt-4o`, BitRouter has to pick which registered endpoint to send the request to. By default it uses a balanced score; add a `model:<profile>` suffix when you want to choose a policy explicitly.

<Callout type="warn">
**Today, choose a managed routing policy with the [`model:<profile>` suffix](/docs/features/model-variants)** — e.g. `openai/gpt-4o:latency`. A request-body field such as `provider.sort` is planned and **not yet active**; use the suffix in live Cloud requests.
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
curl https://api.bitrouter.ai/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -d '{
    "model": "openai/gpt-4o:latency",
    "messages": [{"role": "user", "content": "Translate to French: Hello."}]
  }'
```

The same model suffix is honored on `/v1/chat/completions`, `/v1/messages` (Anthropic), and `/v1beta/models/{model}:generateContent` (Google).

## BYOK providers come first

If you've [added an external key](/docs/features/byok) for a provider, BitRouter prefers that provider for any model it can serve — ahead of every non-BYOK provider, regardless of the selected profile. Your BYOK key bills against your own account at upstream list price with no rev share, and you opted into that provider explicitly; honoring that opt-in by default is the only choice that doesn't surprise you later.

Within the BYOK-eligible set, the selected profile still applies. So `openai/gpt-4o:latency` plus BYOK keys for OpenAI and Anthropic ranks those two by TTFT first, and falls back to non-BYOK providers (also ranked by latency) only if both BYOK paths fail.

In **local OSS mode**, built-in Cloud profile suffixes are not predeclared. Define `variants` or presets in `bitrouter.yaml` when you want local per-request routing selectors; otherwise a suffix remains part of the literal model id.

## Default behavior

When no profile suffix is present, BitRouter ranks by a **balanced score** — a weighted combination of cost, latency, throughput, and uptime, with low-uptime providers filtered out. This is the right default for most agents; specify a policy only when one axis dominates.

<Callout type="info">
**The default is not stable across versions.** The weights in the balanced score are tuned over time as we learn from real traffic. If you need a fixed, reproducible policy — for cost reporting, SLO tracking, or A/B tests — add a profile suffix explicitly.
</Callout>

## How selection composes with fallback

[Model fallback](/docs/features/model-fallback) and provider selection are independent layers:

1. For each model in your `models` list (or the single `model` if no fallback), BitRouter applies the policy encoded on that model id, or `balanced` when no suffix is present.
2. If the chosen provider fails in a way that doesn't surface to the caller (rate limit, 5xx), BitRouter retries on the **next-ranked provider of the same model** before falling through to the next model in the list.
3. Each fallback model string is resolved independently, so you can use the same suffix everywhere or pick different profiles per fallback entry.

Concretely: `models: ["openai/gpt-4o:cost", "anthropic/claude-sonnet-4-6:cost"]` evaluates the cheapest provider of GPT-4o first, then the cheapest provider of Sonnet, then surfaces the error.

## When metrics are tied

If two providers price the same prompt identically, the higher-uptime one wins. If uptime is also tied, the lower-error-rate one wins. If everything is tied, BitRouter sorts by provider ID lexicographically — deterministic and audit-friendly, but it does not "load balance." If even spend distribution across tied providers matters for your workload, post a use case to [Discord](https://discord.gg/G3zVrZDa5C); we'll add a `provider.balance` knob if there's demand.

## What's not here

OpenRouter exposes a much larger surface — `provider.order`, `provider.allow_fallbacks`, `provider.require_parameters`, `provider.data_collection`, `provider.ignore`, `provider.quantizations`, and more. We are deliberately keeping this to one knob with three values until usage tells us otherwise. Two equivalent expressions if you're migrating:

- **Pin to a specific provider** — use the provider-prefixed model ID, e.g. `model: "anthropic-direct/anthropic/claude-sonnet-4-6"`.
- **Exclude a provider** — omit it from your workspace's registry allowlist, not the request body.

If a missing knob is blocking a real workload, file an issue on [bitrouter](https://github.com/bitrouter/bitrouter).
