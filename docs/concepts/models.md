---
title: Models
description: On BitRouter a model is an aggregate served by many providers — reached through four protocols, ranked per request, with discounted open supply.
sourceHash: 5db2bbc903ccb998b052947933b808037a2c8e06a473f3657c0e65672db6eecf
---

On BitRouter a "model" is not a single endpoint. It's an **aggregate**: one logical model — say `openai/gpt-4o` or `anthropic/claude-sonnet-4.6` — that can be served by many providers at once. You address it by a stable **model id**, and BitRouter decides which underlying provider endpoint actually answers each request.

That indirection is the whole point. You write your agent against `anthropic/claude-sonnet-4.6`, and the set of providers behind it can grow, shrink, or re-price without you changing a line of code.

## Four protocols in

You reach the models gateway through whichever API your runtime already speaks. BitRouter exposes **four protocols**, side by side, on one local endpoint:

- **OpenAI Chat Completions** — `POST /v1/chat/completions`
- **OpenAI Responses** — `POST /v1/responses`
- **Anthropic Messages** — `POST /v1/messages`
- **Google Generative AI** — `POST /v1beta/models/{model}:generateContent`

Pick the one your SDK is already wired for — you don't adopt a new client. And because the gateway speaks all four, it can **route across them**: a request that arrives as Anthropic Messages can be served by an OpenAI provider, and vice-versa. One model id, reachable four ways, answerable by any eligible provider.

## One id, many providers

Because a model is an aggregate, requesting it kicks off a **provider selection** step. By default BitRouter ranks the eligible providers by a balanced score — a blend of cost, latency, throughput, and uptime — and sends your request to the best one. When the chosen provider fails transiently, it can fall through to the next-ranked provider, or to the next model you listed.

## Variants re-rank for one request

When one model has several providers, you sometimes want to bias that ranking for a single call. A **model variant** is an inline suffix on the id — `:cost`, `:latency`, `:throughput` — that re-ranks the *eligible* providers along the axis you named, for that request only. It never changes which providers are eligible, never changes authorization, and a bare id is just the balanced default.

## Open models, discounted

Open (non-closed-source) models carry a second property: BitRouter serves them through its own self-hosted provider at **25% below official pricing by default**, with no suffix or configuration. The `:discount` suffix pins a request to that supply explicitly, and it's where any custom account discount applies.

## Learn how to

- [Provider selection](/docs/features/provider-selection) — how providers behind one model are ranked.
- [Model fallback](/docs/features/model-fallback) — pass an ordered list and walk it on failure.
- [Model variants](/docs/features/model-variants) — the `:cost` / `:latency` / `:throughput` suffixes.
- [Presets](/docs/features/presets) — named, reusable routing configurations.
- [Structured outputs](/docs/features/structured-outputs) — enforce a JSON schema across providers.
- [Add external keys (BYOK)](/docs/features/byok) — route through your own provider account.
- [Local & private models](/docs/integrations/models) — point BitRouter at your own server.
- [Managed provider & pricing](/docs/get-started/models-and-providers) — the hosted provider and the full catalog.
