---
title: Supported Providers
description: Every provider registered on the BitRouter network — served from the public, open-source registry that anyone can join.
---

Every model in the [catalog](/docs/get-started/supported-models) is served by one or more **registered providers**. Membership lives in the public, open-source [registry](https://github.com/bitrouter/bitrouter/tree/main/registry) — anyone can [register a provider](/docs/guides/register-as-a-provider). The directory below is generated from the current registry snapshot.

## Provider directory

| Provider | Name | HQ | Protocols | Billing | Models |
| --- | --- | --- | --- | --- | --- |
| `akashml` | AkashML | US | openai | Per-token | 3 |
| `alibaba` | Alibaba Cloud | CN | anthropic, openai, responses | Per-token | 4 |
| `alibaba_cn` | Alibaba Cloud | CN | anthropic, openai, responses | Per-token | 4 |
| `alibaba_coding_plan` | Alibaba Cloud | CN | openai | Subscription | 7 |
| `ambient` | Ambient | US | openai | Per-token | 2 |
| `anthropic` | Anthropic | US | anthropic | Per-token | 8 |
| `atlascloud` | Atlas Cloud | US | openai | Per-token | 14 |
| `bitrouter` | BitRouter | US | — | Per-token | 0 |
| `byteplus` | BytePlus | CN | openai | Per-token | 1 |
| `cerebras` | Cerebras | US | openai | Per-token | 1 |
| `chutes` | Chutes | US | openai | Per-token | 7 |
| `claude-code` | Anthropic | US | anthropic | Subscription | 7 |
| `deepseek` | DeepSeek | CN | anthropic, openai | Per-token | 2 |
| `github-copilot` | GitHub | US | openai | Subscription | 13 |
| `gmicloud` | GMI Cloud | US | openai | Per-token | 13 |
| `google` | Google | US | google, openai | Per-token | 4 |
| `google-ai` | Google | US | google, openai | Subscription | 3 |
| `ionet` | io.net | US | openai | Per-token | 9 |
| `minimax` | MiniMax | CN | anthropic, openai | Per-token | 3 |
| `minimax_cn` | MiniMax | CN | anthropic, openai | Per-token | 3 |
| `moonshotai` | Moonshot AI | CN | anthropic, openai | Per-token | 3 |
| `moonshotai_coding_plan` | Moonshot AI | CN | openai | Subscription | 1 |
| `novita` | Novita AI | US | openai | Per-token | 18 |
| `openai` | OpenAI | US | openai, responses | Per-token | 3 |
| `openai-codex` | OpenAI | US | responses | Subscription | 3 |
| `opencode-go` | OpenCode | US | openai | Subscription | 18 |
| `opencode-zen` | OpenCode | US | openai | Per-token | 24 |
| `openrouter` | OpenRouter | US | anthropic, openai, responses | Per-token | 39 |
| `phala` | Phala | US | openai | Per-token | 5 |
| `qianfan` | Baidu AI Cloud | CN | openai | Per-token | 3 |
| `qianfan_cn` | Baidu AI Cloud | CN | openai | Per-token | 6 |
| `siliconflow` | SiliconFlow | CN | openai | Per-token | 17 |
| `siliconflow_cn` | SiliconFlow | CN | openai | Per-token | 13 |
| `stepfun` | StepFun | CN | anthropic, openai | Per-token | 2 |
| `stepfun_cn` | StepFun | CN | anthropic, openai | Per-token | 2 |
| `stepfun_step_plan` | StepFun | CN | anthropic, openai | Subscription | 2 |
| `stepfun_step_plan_cn` | StepFun | CN | anthropic, openai | Subscription | 2 |
| `streamlake` | StreamLake | CN | openai | Per-token | 3 |
| `supergrok` | xAI | US | openai, responses | Subscription | 4 |
| `tencent` | Tencent Cloud | CN | anthropic, openai, responses | Per-token | 11 |
| `tencent_cn` | Tencent Cloud | CN | anthropic, openai, responses | Per-token | 11 |
| `tinfoil` | Tinfoil | US | openai | Per-token | 3 |
| `volcengine` | Volcengine | CN | openai | Per-token | 2 |
| `xai` | xAI | US | openai, responses | Per-token | 4 |
| `xiaomi` | Xiaomi | CN | anthropic, openai, responses | Per-token | 5 |
| `xiaomi_token_plan` | Xiaomi | CN | anthropic, openai, responses | Subscription | 2 |
| `xiaomi_token_plan_cn` | Xiaomi | CN | anthropic, openai, responses | Subscription | 2 |
| `xiaomi_token_plan_eu` | Xiaomi | CN | anthropic, openai, responses | Subscription | 2 |
| `zai` | Z.ai | CN | anthropic, openai | Per-token | 3 |
| `zai_coding_plan` | Z.ai | CN | anthropic, openai | Subscription | 3 |

## Register your own

BitRouter is a permissionless network: any provider exposing an OpenAI- or Anthropic-compatible endpoint can join and be discovered by every agent on it. Registration is a pull request against the open-source registry — see [Register as a provider](/docs/guides/register-as-a-provider) for the full walkthrough.

## Discounted open models

BitRouter also runs its own **self-hosted provider** that serves open models **25% below official** rates by default, with custom discounts up to 50% for open-source projects. See [Discounted open models](/docs/get-started/supported-models#discounted-open-models) for how the pricing and the `:discount` suffix work.
