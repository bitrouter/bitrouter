---
title: Supported Models
description: The full catalog of models any BitRouter account can call — with pricing, reachable over your own keys or one hosted BitRouter Cloud account, with automatic discounts on open models.
---

Every model BitRouter can route to is listed below. Reach any of them over your own provider keys ([BYOK](/docs/features/byok), paid to the providers at their list price) or one [BitRouter Cloud](/docs/get-started/configuration) account — one sign-in, no upstream keys, billed per request with failed requests not charged. Running your own model? See [local & private models](/docs/integrations/models) (free).

Prices are USD per **million tokens**, taken from the current [registry](https://github.com/bitrouter/bitrouter/tree/main/registry) snapshot (each model's lowest-priced provider; `—` means no per-token provider lists it yet). Open models are served **25% below official** by default — see [Discounted open models](#discounted-open-models) below. Every model is served by one or more registered providers — see [Supported Providers](/docs/get-started/supported-providers) for the full list and how to register your own.

## Model catalog

| Model | Name | Context | Modalities | Open weights | Input $/M | Output $/M |
| --- | --- | --- | --- | --- | --- | --- |
| `anthropic/claude-fable-5` | Anthropic: Claude Fable 5 | 1M | text, image | — | $10 | $50 |
| `anthropic/claude-haiku-4.5` | Anthropic: Claude Haiku 4.5 | 200K | text, image | — | $1 | $5 |
| `anthropic/claude-opus-4.6` | Anthropic: Claude Opus 4.6 | 200K | text, image | — | $5 | $25 |
| `anthropic/claude-opus-4.7` | Anthropic: Claude Opus 4.7 | 200K | text, image | — | $4.5 | $22.5 |
| `anthropic/claude-opus-4.8` | Anthropic: Claude Opus 4.8 | 1M | text, image | — | $5 | $25 |
| `anthropic/claude-sonnet-4.6` | Anthropic: Claude Sonnet 4.6 | 1M | text, image | — | $3 | $15 |
| `anthropic/claude-sonnet-5` | Anthropic: Claude Sonnet 5 | 1M | text, image | — | $2 | $10 |
| `deepseek/deepseek-v3.2` | DeepSeek: DeepSeek V3.2 | 128K | text | ✅ | $0.2288 | $0.3432 |
| `deepseek/deepseek-v4-flash` | DeepSeek: DeepSeek V4 Flash | 256K | text | ✅ | $0.09 | $0.18 |
| `deepseek/deepseek-v4-pro` | DeepSeek: DeepSeek V4 Pro | 256K | text | ✅ | $0.435 | $0.87 |
| `google/gemini-3.1-flash-lite-preview` | Google: Gemini 3.1 Flash Lite Preview | 1M | text, image | — | $0.25 | $1.5 |
| `google/gemini-3.1-pro-preview` | Google: Gemini 3.1 Pro Preview | 2M | text, image | — | $2 | $12 |
| `google/gemini-3.5-flash` | Google: Gemini 3.5 Flash | 1M | text, image, audio | — | $1.5 | $9 |
| `google/gemma-4-31b` | Google: Gemma 4 31B | 128K | text, image | ✅ | $0.13 | $0.4 |
| `inclusionai/ling-2.6-1t` | Ling 2.6 1T | 256K | text | ✅ | $0.3 | $2.5 |
| `inclusionai/ling-2.6-flash` | Ling 2.6 Flash | 256K | text | ✅ | $0.1 | $0.3 |
| `inclusionai/ring-2.6-1t` | Ring 2.6 1T | 256K | text | ✅ | $0.3 | $2.5 |
| `meituan/longcat-2.0` | LongCat 2.0 | 1M | text | ✅ | — | — |
| `minimax/minimax-m2.5` | MiniMax: M2.5 | 192K | text | ✅ | $0.15 | $1.15 |
| `minimax/minimax-m2.7` | MiniMax: M2.7 | 192K | text | ✅ | $0.25 | $1 |
| `minimax/minimax-m3` | MiniMax: M3 | 1M | text, image | ✅ | $0.3 | $1.2 |
| `moonshotai/kimi-k2.5` | Kimi: K2.5 | 256K | text, image | ✅ | $0.375 | $2.025 |
| `moonshotai/kimi-k2.6` | Kimi: K2.6 | 256K | text | ✅ | $0.66 | $3.41 |
| `moonshotai/kimi-k2.7-code` | Kimi: K2.7 Code | 256K | text, image | ✅ | $0.612 | $3.069 |
| `nex-agi/nex-n2-pro` | Nex AGI: Nex-N2-Pro | 256K | text, image | ✅ | — | — |
| `nvidia/nemotron-3-super-120b-a12b` | Nemotron 3 Super 120B-A12B | 256K | text | ✅ | — | — |
| `nvidia/nemotron-3-ultra-550b-a55b` | Nemotron 3 Ultra 550B-A55B | 1M | text | ✅ | — | — |
| `openai/gpt-5.4` | OpenAI: GPT-5.4 | 128K | text, image | — | $2.5 | $15 |
| `openai/gpt-5.4-mini` | OpenAI: GPT-5.4 Mini | 128K | text, image | — | $0.75 | $4.5 |
| `openai/gpt-5.5` | OpenAI: GPT-5.5 | 128K | text, image | — | $5 | $30 |
| `qwen/qwen3.5-122b-a10b` | Qwen: Qwen3.5 122B-A10B | 256K | text, image | ✅ | $0.26 | $2.08 |
| `qwen/qwen3.5-27b` | Qwen: Qwen3.5 27B | 256K | text, image | ✅ | $0.25 | $2 |
| `qwen/qwen3.6-27b` | Qwen: Qwen3.6 27B | 256K | text, image | ✅ | $0.3 | $3.2 |
| `qwen/qwen3.6-35b-a3b` | Qwen: Qwen3.6 35B-A3B | 256K | text, image | ✅ | $0.2 | $1.6 |
| `qwen/qwen3.6-flash` | Qwen: Qwen3.6 Flash | 1M | text, image | — | $0.165 | $0.99 |
| `qwen/qwen3.7-max` | Qwen: Qwen3.7 Max | 1M | text | — | $1.25 | $3.75 |
| `qwen/qwen3.7-plus` | Qwen: Qwen3.7 Plus | 1M | text, image | — | $0.276 | $1.101 |
| `stepfun/step-3.5-flash` | StepFun: Step 3.5 Flash | 256K | text | ✅ | $0.09 | $0.3 |
| `stepfun/step-3.7-flash` | StepFun: Step 3.7 Flash | 256K | text, image | ✅ | $0.2 | $1.15 |
| `tencent/hy3` | Hunyuan 3 | 256K | text | ✅ | $0.066 | $0.26 |
| `x-ai/grok-4.20` | xAI: Grok 4.20 | 128K | text, image | — | $1.25 | $2.5 |
| `x-ai/grok-4.20-multi-agent` | xAI: Grok 4.20 Multi-Agent | 1M | text, image | — | $1.25 | $2.5 |
| `x-ai/grok-4.3` | xAI: Grok 4.3 | 1M | text, image | — | $1.25 | $2.5 |
| `x-ai/grok-build-0.1` | xAI: Grok Build 0.1 | 256K | text | — | $1 | $2 |
| `xiaomi/mimo-v2.5` | Xiaomi: MiMo V2.5 | 256K | text | ✅ | $0.14 | $0.28 |
| `xiaomi/mimo-v2.5-pro` | Xiaomi: MiMo V2.5 Pro | 256K | text | ✅ | $0.435 | $0.87 |
| `z-ai/glm-4.7` | Zhipu: GLM-4.7 | 200K | text | ✅ | $0.6 | $2.2 |
| `z-ai/glm-5` | Zhipu: GLM-5 | 198K | text | ✅ | $0.6 | $1.92 |
| `z-ai/glm-5.1` | Zhipu: GLM-5.1 | 128K | text | ✅ | $0.95 | $3.15 |
| `z-ai/glm-5.2` | Zhipu: GLM-5.2 | 1M | text | ✅ | $0.979 | $3.08 |

## Using BitRouter Cloud

The **BitRouter Cloud provider** lets an agent call any model above with a single BitRouter account — no upstream provider keys, no per-provider signups. You pay BitRouter directly at the prices listed here, billed per request; failed requests aren't billed.

```bash
bitrouter cloud login   # one-time device-flow sign-in
bitrouter start         # the `bitrouter` provider auto-enables once signed in
```

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
