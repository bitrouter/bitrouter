---
title: Model Variants
description: Append :cost, :latency, or :throughput to a model id to choose how BitRouter ranks providers — inline, per request, no body fields.
sourceHash: 9704500065fd7a0150d6564893f63556411fefc50300238ffb7d68e85410b8eb
---

When a model is served by more than one provider, BitRouter has to choose which endpoint to send each request to. A **model variant** lets you make that choice inline: append a `:<profile>` suffix to the model id and BitRouter ranks providers by the axis you named.

The suffix is part of the `model` string itself, so it needs no body fields and no SDK — it works the same on the OpenAI, Anthropic, and Google surfaces.

## The profiles

| Model id | Ranks providers by |
| --- | --- |
| `openai/gpt-4o` | **Balanced** (default) — a weighted blend of cost, latency, and throughput. |
| `openai/gpt-4o:cost` | Cheapest first. |
| `openai/gpt-4o:latency` | Lowest p50 time-to-first-token. |
| `openai/gpt-4o:throughput` | Highest output tokens/sec. |

These are the same three axes described in [Provider Selection](/docs/features/provider-selection), exposed as an inline shorthand. A bare model id behaves exactly like `:balanced`.

## Quick example

```bash
curl http://127.0.0.1:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-sonnet-4.6:cost",
    "messages": [{"role": "user", "content": "Translate to French: Hello."}]
  }'
```

The suffix behaves identically on `/v1/messages` (Anthropic) and `/v1beta/models/{model}:generateContent` (Google) — it travels with the model id.

## Behavior

- **A bare id is the balanced default.** Adding no suffix is identical to `:balanced` and is byte-for-byte the routing you get today. Variants only ever *re-rank* the eligible providers — they never change which providers are eligible.
- **The bias is aggressive.** A profile weights its named axis heavily and keeps the other two only as tie-breakers, so `:cost` picks the genuinely cheapest provider rather than leaning slightly cheaper. Reach for `:balanced` (or no suffix) when you want the blended score.
- **Unknown suffixes are not routing profiles.** Only `cost`, `latency`, `throughput`, and `balanced` are recognized as profiles. Anything else — `openai/gpt-4o:fast` — is treated as part of the model id, so it falls through to a normal "unknown model" `404` rather than being silently rerouted. (This is the same rule that leaves a `provider:model` id like `openai:gpt-4o` intact.) The one other recognized suffix, [`:discount`](#the-discount-suffix), routes to BitRouter's self-hosted discounted provider — it's not a routing profile, and is handled separately.
- **It only matters with 2+ providers.** A model served by a single provider routes identically with or without a suffix.

<Callout type="info">
**Cost is exact from day one; latency and throughput warm up.** `:cost` is computed against current upstream pricing, so it is accurate immediately. `:latency` and `:throughput` read a rolling 1-hour telemetry window — on a cold or low-traffic model they stay close to the balanced score until enough samples accrue.
</Callout>

## The `:discount` suffix

Separate from the ranking profiles above, BitRouter accepts a `:discount` suffix that routes a request to BitRouter's **self-hosted provider**, where open models are served below official pricing:

```text
moonshotai/kimi-k2.6:discount     # route to the self-hosted discounted provider
```

Because it pins the provider rather than re-ranking the eligible ones, `:discount` is **not** a routing profile and doesn't combine meaningfully with `:cost` / `:latency` / `:throughput`. Open models are already 25% off by default without it; the suffix forces the discounted self-hosted supply and is where custom account discounts apply. See [Discounted Models](/docs/get-started/models-and-providers) for the full behavior — including custom discounts up to 50% for open-source projects.

## Variants never change authorization

The profile affects provider *ranking* only. Everything else keys off the base model id:

- [Guardrail](/docs/features/guardrails) model allowlists/denylists and BYOK rules judge `anthropic/claude-sonnet-4.6:cost` exactly as `anthropic/claude-sonnet-4.6` — a profile can never widen or bypass a policy.
- [BYOK](/docs/features/byok) providers still rank ahead of platform providers; the profile orders providers *within* each tier.
- Billing is unchanged — you pay the selected provider's rate for the base model.

## Seeing which profile a request used

The selected profile is recorded on every request and returned in your usage history. `GET /v1/namespaces/{nsid}/requests` includes a `routing_profile` field (`balanced` / `cost` / `latency` / `throughput`) on each row, so you can audit how much traffic each strategy is taking.

## Relationship to `provider.sort`

The model-id suffix is the supported way to choose a routing profile today. A request-body equivalent — `provider.sort` — is described in [Provider Selection](/docs/features/provider-selection) but is **not yet active**; until it ships, use the suffix.
