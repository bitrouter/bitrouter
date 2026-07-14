---
title: Supported Providers
description: BitRouter 网络上已注册的每个供应商——由任何人都可加入的公开、开源注册表提供服务。
---

[目录](/docs/get-started/supported-models)中的每个模型都由一个或多个**已注册供应商**提供服务。成员资格维护在公开、开源的 [registry](https://github.com/bitrouter/bitrouter/tree/main/registry) 中——任何人都可以[注册供应商](/docs/guides/register-as-a-provider)。下面的目录由当前注册表快照生成。

## 供应商目录

| 供应商 | 名称 | 总部 | 协议 | 计费 | 模型数 |
| --- | --- | --- | --- | --- | --- |
| `alibaba` | Alibaba Cloud | CN | anthropic, openai, responses | Per-token | 8 |
| `alibaba_cn` | Alibaba Cloud | CN | anthropic, openai, responses | Per-token | 13 |
| `alibaba_coding_plan` | Alibaba Cloud | CN | openai | Subscription | 8 |
| `ambient` | Ambient | US | openai | Per-token | 3 |
| `anthropic` | Anthropic | US | anthropic | Per-token | 8 |
| `atlascloud` | Atlas Cloud | US | openai | Per-token | 13 |
| `aws-bedrock` | Amazon Web Services | US | openai, responses | Per-token | 14 |
| `azure` | Microsoft Azure | US | openai, responses | Per-token | 18 |
| `bitrouter` | BitRouter | US | openai | Per-token | 57 |
| `byteplus` | BytePlus | CN | openai | Per-token | 1 |
| `cerebras` | Cerebras | US | openai | Per-token | 3 |
| `chutes` | Chutes | US | openai | Per-token | 7 |
| `claude-code` | Anthropic | US | anthropic | Subscription | 7 |
| `deepseek` | DeepSeek | CN | anthropic, openai | Per-token | 2 |
| `github-copilot` | GitHub | US | openai | Subscription | 17 |
| `gmicloud` | GMI Cloud | US | openai | Per-token | 15 |
| `google` | Google | US | google, openai | Per-token | 4 |
| `google-ai` | Google | US | google, openai | Subscription | 3 |
| `meta-ai` | Meta | US | anthropic, openai, responses | Per-token | 1 |
| `minimax` | MiniMax | CN | anthropic, openai | Per-token | 3 |
| `minimax_cn` | MiniMax | CN | anthropic, openai | Per-token | 3 |
| `moonshotai` | Moonshot AI | CN | anthropic, openai | Per-token | 3 |
| `moonshotai_coding_plan` | Moonshot AI | CN | openai | Subscription | 1 |
| `novita` | Novita AI | US | openai | Per-token | 18 |
| `openai` | OpenAI | US | openai, responses | Per-token | 6 |
| `openai-codex` | OpenAI | US | responses | Subscription | 6 |
| `opencode-go` | OpenCode | US | openai | Subscription | 18 |
| `opencode-zen` | OpenCode | US | openai | Per-token | 30 |
| `openrouter` | OpenRouter | US | anthropic, openai, responses | Per-token | 56 |
| `phala` | Phala | US | openai | Per-token | 5 |
| `qianfan` | Baidu AI Cloud | CN | openai | Per-token | 9 |
| `qianfan_cn` | Baidu AI Cloud | CN | openai | Per-token | 9 |
| `redpill` | RedPill | SG | openai | Per-token | 5 |
| `siliconflow` | SiliconFlow | CN | openai | Per-token | 19 |
| `siliconflow_cn` | SiliconFlow | CN | openai | Per-token | 13 |
| `stepfun` | StepFun | CN | anthropic, openai | Per-token | 2 |
| `stepfun_cn` | StepFun | CN | anthropic, openai | Per-token | 2 |
| `stepfun_step_plan` | StepFun | CN | anthropic, openai | Subscription | 2 |
| `stepfun_step_plan_cn` | StepFun | CN | anthropic, openai | Subscription | 2 |
| `streamlake` | StreamLake | CN | openai | Per-token | 3 |
| `supergrok` | xAI | US | openai, responses | Subscription | 5 |
| `tencent` | Tencent Cloud | CN | anthropic, openai, responses | Per-token | 11 |
| `tencent_cn` | Tencent Cloud | CN | anthropic, openai, responses | Per-token | 12 |
| `tinfoil` | Tinfoil | US | openai | Per-token | 5 |
| `vertex` | Google Cloud | US | google | Per-token | 3 |
| `volcengine` | Volcengine | CN | openai | Per-token | 2 |
| `xai` | xAI | US | openai, responses | Per-token | 5 |
| `xiaomi` | Xiaomi | CN | anthropic, openai, responses | Per-token | 5 |
| `xiaomi_token_plan` | Xiaomi | CN | anthropic, openai, responses | Subscription | 2 |
| `xiaomi_token_plan_cn` | Xiaomi | CN | anthropic, openai, responses | Subscription | 2 |
| `xiaomi_token_plan_eu` | Xiaomi | CN | anthropic, openai, responses | Subscription | 2 |
| `zai` | Z.ai | CN | anthropic, openai | Per-token | 4 |
| `zai_coding_plan` | Z.ai | CN | anthropic, openai | Subscription | 4 |

## 注册你自己的供应商

BitRouter 是一个无许可网络：任何暴露 OpenAI 或 Anthropic 兼容端点的供应商都可以加入，并被网络上的每个 agent 发现。注册即是向开源注册表提交一个 pull request——完整流程见[注册为供应商](/docs/guides/register-as-a-provider)。

## 折扣开放模型

BitRouter 还运营自己的**自托管供应商**，默认以**官方价低 25%** 提供开放模型，并为开源项目提供最高 50% 的定制折扣。定价与 `:discount` 后缀的工作方式见[折扣开放模型](/docs/get-started/supported-models#discounted-open-models)。
