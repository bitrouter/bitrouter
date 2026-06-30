---
title: Model Fallback
description: Pass an ordered list of models and BitRouter walks the list when the primary fails.
sourceHash: c1e0ae231186a6055d3ed8910d3afe6f8ae561718f0dd887db20a5ff863a7034
---

LLM endpoints fail. Rate limits, model outages, context-overflow errors, and content filters all surface as request errors that would otherwise stall an agent loop. **Model fallback** lets you pass a ranked list of models in a single request — BitRouter walks the list until one succeeds, then returns that response.

This is a body-level extension to the OpenAI, Anthropic, and Google protocol surfaces. No SDK required — set one field.

## Quick example

```bash
curl http://127.0.0.1:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openai/gpt-4o",
    "models": [
      "openai/gpt-4o",
      "anthropic/claude-sonnet-4-6",
      "google/gemini-2.5-pro"
    ],
    "messages": [{"role": "user", "content": "Summarize the Iliad in one sentence."}]
  }'
```

The `model` field stays the primary for billing and routing semantics. The `models` array overrides it as an ordered preference list — first one that returns successfully wins.

<Callout type="info">
**You can omit `model` entirely when `models` is set.** If both are present, the first entry of `models` is the primary and `model` is ignored. We recommend always passing `model` so error logs in your app stay readable.
</Callout>

## What triggers a fallback

BitRouter falls through to the next model on errors that are **upstream-side and likely transient**, and surfaces 4xx errors caused by your request directly to the caller.

| Outcome | Signal | Behavior |
| --- | --- | --- |
| Rate limited | `429` | Fall through |
| Server error | `5xx` | Fall through |
| Timeout / connection drop | `408`, network error | Fall through |
| Context window exceeded | provider-specific code | Fall through |
| Content filter / refusal | provider-specific code | Fall through |
| Mid-stream failure (no tokens emitted) | stream aborted before first token | Fall through |
| Mid-stream failure (after first token) | stream aborted mid-response | **Surfaced** — partial output already sent |
| Authentication error | `401` | Surfaced |
| Forbidden / quota exhausted | `402`, `403` | Surfaced |
| Validation / bad request | `400`, `422` | Surfaced |
| Explicit cancel | client disconnect | Surfaced |

Fallback is single-pass. BitRouter attempts each model at most once per request, in order. There is no exponential backoff between attempts — the assumption is that you'd rather retry on a different model immediately than wait on a failing one.

<Callout type="warn">
**Fallback runs per request, not per token.** If a stream succeeds on model A and disconnects after token 50, BitRouter does not silently restart on model B — that would emit a discontinuous response. Wrap fallback at the request boundary; checkpoint and resume in your agent for stream-level resilience.
</Callout>

## Inspecting which model answered

Three breadcrumbs:

- **Response body `model` field** — set to the model that actually generated the response, not the one you requested first. (OpenAI convention.)
- **Response header `bitrouter-served-by`** — `<provider-id>/<model-id>`, e.g. `anthropic-direct/anthropic/claude-sonnet-4-6`.
- **Response header `bitrouter-fallback-trace`** — comma-separated list of attempts and outcomes, e.g. `openai/gpt-4o:rate_limit,anthropic/claude-sonnet-4-6:served`. Only emitted when at least one fallback fired.

## Cost and latency tradeoffs

Each fallback attempt is a fresh upstream request. Practical advice:

- Lowest expected cost: order by **cheapest first**, accept higher tail latency under load.
- Lowest expected latency: order by **most reliable first**, accept higher per-token cost.
- For long-running agent loops: bias toward reliability. The cost of a stalled loop is much higher than the marginal cost difference between two frontier models.

For declarative cost or latency optimization across providers of a single model, see [Provider Selection](/docs/features/provider-selection). Fallback and provider selection compose: BitRouter picks the best provider for each model in your `models` array, falling through to the next model only after the chosen provider for the current one has exhausted its retry budget.

## Anthropic and Google surfaces

The `models` field works identically on `/v1/messages` (Anthropic Messages) and `/v1beta/models/{model}:generateContent` (Google Generative AI). On Anthropic, the field is read alongside the existing `model` field; on Google, BitRouter accepts it as an extension to the request body — the upstream `:generateContent` path is rewritten per attempt.

## Limits

- Maximum 8 entries in `models`.
- Each model ID must resolve to a registered model in the [registry](/docs/guides/register-as-a-provider). Unknown IDs return `400` before any upstream attempt.
- Streaming is supported. The first model that begins emitting tokens wins; later models are not attempted even if the stream fails after the first token.
