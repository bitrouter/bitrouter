---
title: Models
description: The full catalog of models any BitRouter account can call — live pricing, reachable over your own keys or one hosted BitRouter Cloud account, with automatic discounts on open models.
sourceHash: 86d87ac9130c04fd5ca161b83bc6f263323dbb88eb5d4be1d45f451a216af27c
---

Every model BitRouter can route to is listed below, with live pricing. Reach any of them over your own provider keys ([BYOK](/docs/features/byok), paid to the providers at their list price) or one [BitRouter Cloud](/docs/get-started/self-hosted-and-cloud) account — one sign-in, no upstream keys, billed per request with failed requests not charged. Running your own model? See [local & private models](/docs/integrations/models) (free).

Prices are USD per **million tokens**, refreshed continuously from the live catalog. Open models are served **25% below official** by default — see [Discounted open models](#discounted-open-models) below.

<ModelsTable />

## Using BitRouter Cloud

The **BitRouter Cloud provider** lets an agent call any model above with a single BitRouter account — no upstream provider keys, no per-provider signups. You pay BitRouter directly at the prices listed here, billed per request; failed requests aren't billed.

```bash
bitrouter cloud login   # one-time device-flow sign-in
bitrouter start         # the `bitrouter` provider auto-enables once signed in
```

## Providers

Every model above is served by one or more **registered providers**. Membership lives in the public, open-source [provider-registry](https://github.com/bitrouter/provider-registry) — anyone can [register a provider](/docs/guides/register-as-a-provider). The list refreshes from the registry continuously, so a newly-merged provider shows up within minutes.

<ProvidersTable />

## Discounted open models

BitRouter runs its own **self-hosted provider** for open models, priced **25% below official** rates. You get that price automatically — and open-source builders can apply for a deeper custom discount.

### 25% off by default

Every model **except** the closed-source families — OpenAI (`gpt-*`), Anthropic (`claude-*`), Google (`gemini-*`), and xAI (`grok-*`) — is served by BitRouter's self-hosted provider at **25% below the model's official price**.

This takes **no suffix and no configuration**. Because the self-hosted provider is the cheapest source for these models, normal routing already sends your requests there and bills the discounted rate. (The four closed-source families above aren't on the self-hosted provider, so they route to their usual upstreams at standard pricing.)

### Pin to the self-hosted provider with `:discount`

Append `:discount` to a model id to route the request **specifically to BitRouter's self-hosted provider**:

```bash
curl http://127.0.0.1:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "moonshotai/kimi-k2.6:discount",
    "messages": [{"role": "user", "content": "Translate to French: Hello."}]
  }'
```

The suffix rides on the `model` string — no body fields, no SDK — and works the same on the OpenAI, Anthropic, and Google surfaces (`/v1/messages`, `/v1beta/models/{model}:generateContent`). Use it to guarantee your traffic lands on the discounted self-hosted supply; it's also where any custom discount on your account applies.

<Callout type="info">
`:discount` never changes authorization. [Guardrail](/docs/features/guardrails) allowlists and [BYOK](/docs/features/byok) rules judge `moonshotai/kimi-k2.6:discount` exactly as `moonshotai/kimi-k2.6` — the suffix can't widen or bypass a policy.
</Callout>

### Custom discounts up to 50% for open-source projects

Building an **open-source agent harness** or another open-source project on BitRouter? We offer **customized discounts — up to 50% off** — for you and your community.

Reach out to set it up:

- **Email** [kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai)
- **Or book a meeting with the founder:**

<CalInline />
