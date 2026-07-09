---
title: Supported Models
description: 任何 BitRouter 账户都能调用的完整模型目录——附定价，可经你自己的密钥或一个托管的 BitRouter Cloud 账户触达，并对开放模型自动打折。
---

BitRouter 能路由到的每个模型都列在下面。你可以经自己的供应商密钥（[BYOK](/docs/features/byok)，按各供应商官方价直接向其付费）触达它们中的任意一个，也可以经一个 [BitRouter Cloud](/docs/get-started/configuration) 账户——一次登录，无需上游密钥，按请求计费且失败请求不计费。要运行自己的模型？参见[本地与私有模型](/docs/integrations/models)（免费）。

价格以美元 / **百万 token** 计，取自当前[注册表](https://github.com/bitrouter/bitrouter/tree/main/registry)快照（每个模型价格最低的供应商；`—` 表示暂无按量计费的供应商提供）。开放模型默认以**官方价低 25%** 提供——参见下文[折扣开放模型](#discounted-open-models)。每个模型都由一个或多个已注册供应商提供服务——完整列表以及如何注册自己的供应商，见[支持的供应商](/docs/get-started/supported-providers)。

## 模型目录

| 模型 | 名称 | 上下文 | 模态 | 开源权重 | 输入 $/M | 输出 $/M |
| --- | --- | --- | --- | --- | --- | --- |
| `anthropic/claude-fable-5` | Anthropic: Claude Fable 5 | 1M | text, image | — | $10 | $50 |
| `anthropic/claude-haiku-4.5` | Anthropic: Claude Haiku 4.5 | 200K | text, image | — | $1 | $5 |
| `anthropic/claude-opus-4.6` | Anthropic: Claude Opus 4.6 | 200K | text, image | — | $5 | $25 |
| `anthropic/claude-opus-4.7` | Anthropic: Claude Opus 4.7 | 200K | text, image | — | $4.5 | $22.5 |
| `anthropic/claude-opus-4.8` | Anthropic: Claude Opus 4.8 | 1M | text, image | — | $5 | $25 |
| `anthropic/claude-sonnet-4.6` | Anthropic: Claude Sonnet 4.6 | 1M | text, image | — | $3 | $15 |
| `anthropic/claude-sonnet-5` | Anthropic: Claude Sonnet 5 | 1M | text, image | — | $2 | $10 |
| `deepseek/deepseek-v3.2` | DeepSeek: DeepSeek V3.2 | 128K | text | ✅ | $0.21 | $0.315 |
| `deepseek/deepseek-v4-flash` | DeepSeek: DeepSeek V4 Flash | 256K | text | ✅ | $0.09 | $0.18 |
| `deepseek/deepseek-v4-pro` | DeepSeek: DeepSeek V4 Pro | 256K | text | ✅ | $0.435 | $0.87 |
| `google/gemini-3.1-flash-lite-preview` | Google: Gemini 3.1 Flash Lite Preview | 1M | text, image | — | $0.25 | $1.5 |
| `google/gemini-3.1-pro-preview` | Google: Gemini 3.1 Pro Preview | 2M | text, image | — | $2 | $12 |
| `google/gemini-3.5-flash` | Google: Gemini 3.5 Flash | 1M | text, image, audio | — | $1.5 | $9 |
| `google/gemma-4-31b` | Google: Gemma 4 31B | 128K | text, image | ✅ | $0.13 | $0.4 |
| `inclusionai/ling-2.6-1t` | Ling 2.6 1T | 256K | text | ✅ | $0.075 | $0.625 |
| `inclusionai/ling-2.6-flash` | Ling 2.6 Flash | 256K | text | ✅ | $0.01 | $0.03 |
| `inclusionai/ring-2.6-1t` | Ring 2.6 1T | 256K | text | ✅ | $0.075 | $0.625 |
| `meituan/longcat-2.0` | LongCat 2.0 | 1M | text | ✅ | — | — |
| `meta/muse-spark-1.1` | Meta: Muse Spark 1.1 | 1M | text, image | — | $1.25 | $4.25 |
| `minimax/minimax-m2.5` | MiniMax: M2.5 | 192K | text | ✅ | $0.15 | $0.9 |
| `minimax/minimax-m2.7` | MiniMax: M2.7 | 192K | text | ✅ | $0.225 | $0.9 |
| `minimax/minimax-m3` | MiniMax: M3 | 1M | text, image | ✅ | $0.3 | $1.2 |
| `moonshotai/kimi-k2.5` | Kimi: K2.5 | 256K | text, image | ✅ | $0.375 | $2.025 |
| `moonshotai/kimi-k2.6` | Kimi: K2.6 | 256K | text | ✅ | $0.66 | $3.41 |
| `moonshotai/kimi-k2.7-code` | Kimi: K2.7 Code | 256K | text, image | ✅ | $0.612 | $3.069 |
| `nex-agi/nex-n2-pro` | Nex AGI: Nex-N2-Pro | 256K | text, image | ✅ | $0.25 | $1 |
| `nvidia/nemotron-3-super-120b-a12b` | Nemotron 3 Super 120B-A12B | 256K | text | ✅ | $0.08 | $0.45 |
| `nvidia/nemotron-3-ultra-550b-a55b` | Nemotron 3 Ultra 550B-A55B | 1M | text | ✅ | $0.5 | $2.2 |
| `openai/gpt-5.4` | OpenAI: GPT-5.4 | 128K | text, image | — | $2.5 | $15 |
| `openai/gpt-5.4-mini` | OpenAI: GPT-5.4 Mini | 128K | text, image | — | $0.75 | $4.5 |
| `openai/gpt-5.5` | OpenAI: GPT-5.5 | 128K | text, image | — | $5 | $30 |
| `openai/gpt-5.6-luna` | OpenAI: GPT-5.6 Luna | 400K | text, image | — | $1 | $6 |
| `openai/gpt-5.6-sol` | OpenAI: GPT-5.6 Sol | 1M | text, image | — | $5 | $30 |
| `openai/gpt-5.6-terra` | OpenAI: GPT-5.6 Terra | 1M | text, image | — | $2.5 | $15 |
| `qwen/qwen3.5-122b-a10b` | Qwen: Qwen3.5 122B-A10B | 256K | text, image | ✅ | $0.26 | $2.08 |
| `qwen/qwen3.5-27b` | Qwen: Qwen3.5 27B | 256K | text, image | ✅ | $0.195 | $1.56 |
| `qwen/qwen3.6-27b` | Qwen: Qwen3.6 27B | 256K | text, image | ✅ | $0.285 | $2.4 |
| `qwen/qwen3.6-35b-a3b` | Qwen: Qwen3.6 35B-A3B | 256K | text, image | ✅ | $0.14 | $1 |
| `qwen/qwen3.6-flash` | Qwen: Qwen3.6 Flash | 1M | text, image | — | $0.165 | $0.99 |
| `qwen/qwen3.7-max` | Qwen: Qwen3.7 Max | 1M | text | — | $1.25 | $3.75 |
| `qwen/qwen3.7-plus` | Qwen: Qwen3.7 Plus | 1M | text, image | — | $0.276 | $1.101 |
| `stepfun/step-3.5-flash` | StepFun: Step 3.5 Flash | 256K | text | ✅ | $0.072 | $0.216 |
| `stepfun/step-3.7-flash` | StepFun: Step 3.7 Flash | 256K | text, image | ✅ | $0.15 | $0.8625 |
| `tencent/hy3` | Hunyuan 3 | 256K | text | ✅ | $0 | $0 |
| `x-ai/grok-4.20` | xAI: Grok 4.20 | 128K | text, image | — | $1.25 | $2.5 |
| `x-ai/grok-4.20-multi-agent` | xAI: Grok 4.20 Multi-Agent | 1M | text, image | — | $1.25 | $2.5 |
| `x-ai/grok-4.3` | xAI: Grok 4.3 | 1M | text, image | — | $1.25 | $2.5 |
| `x-ai/grok-4.5` | xAI: Grok 4.5 | 500K | text, image | — | $2 | $6 |
| `x-ai/grok-build-0.1` | xAI: Grok Build 0.1 | 256K | text | — | $1 | $2 |
| `xiaomi/mimo-v2.5` | Xiaomi: MiMo V2.5 | 256K | text | ✅ | $0.14 | $0.28 |
| `xiaomi/mimo-v2.5-pro` | Xiaomi: MiMo V2.5 Pro | 256K | text | ✅ | $0.435 | $0.87 |
| `z-ai/glm-4.7` | Zhipu: GLM-4.7 | 200K | text | ✅ | $0.4 | $1.75 |
| `z-ai/glm-5` | Zhipu: GLM-5 | 198K | text | ✅ | $0.6 | $1.92 |
| `z-ai/glm-5.1` | Zhipu: GLM-5.1 | 128K | text | ✅ | $0.83 | $3.33 |
| `z-ai/glm-5.2` | Zhipu: GLM-5.2 | 1M | text | ✅ | $0.979 | $3.08 |

## 使用 BitRouter Cloud

**BitRouter Cloud 供应商**让 agent 只用一个 BitRouter 账户即可调用上面的任意模型——无需上游供应商密钥，也无需逐个供应商注册。你按这里列出的价格直接向 BitRouter 付费，按请求计费；失败的请求不计费。

```bash
bitrouter cloud login   # one-time device-flow sign-in
bitrouter start         # the `bitrouter` provider auto-enables once signed in
```

## 折扣开放模型

BitRouter 为开放模型运营自己的**自托管供应商**，价格比官方定价**低 25%**。你会自动享有该价格——开源项目的开发者还可申请更深的定制折扣。

### 默认立享 25% 折扣

除闭源系列——OpenAI（`gpt-*`）、Anthropic（`claude-*`）、Google（`gemini-*`）、xAI（`grok-*`）、Meta（`muse-spark-*`）——之外的所有模型，都由 BitRouter 的自托管供应商提供，价格比该模型的**官方价低 25%**。

这**无需任何后缀、无需任何配置**。由于自托管供应商是这些模型最便宜的来源，常规路由已经会把你的请求发往那里，并按折扣价计费。（上述五个闭源系列不在自托管供应商上，因此仍按标准价路由到各自的常规上游。）

### 用 `:discount` 固定到自托管供应商

在模型 ID 后追加 `:discount`，即可把该请求**专门路由到 BitRouter 的自托管供应商**：

```bash
curl http://127.0.0.1:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "moonshotai/kimi-k2.6:discount",
    "messages": [{"role": "user", "content": "把 Hello 翻译成法语。"}]
  }'
```

该后缀随 `model` 字符串一起传递——无需请求体字段、无需 SDK——在 OpenAI、Anthropic、Google 三套接口（`/v1/messages`、`/v1beta/models/{model}:generateContent`）上行为一致。当你想确保流量落在折扣的自托管供应商上时使用它；账户上的任何定制折扣也在这里生效。

<Callout type="info">
`:discount` 绝不改变鉴权。[Guardrail](/docs/features/guardrails) 白名单与 [BYOK](/docs/features/byok) 规则会把 `moonshotai/kimi-k2.6:discount` 完全当作 `moonshotai/kimi-k2.6` 来判定——该后缀无法放宽或绕过任何策略。
</Callout>

### 面向开源项目的最高 50% 定制折扣

正在 BitRouter 上构建**开源 agent harness** 或其他开源项目？我们为你和你的社区提供**最高 50% 的定制折扣**。

欢迎联系我们安排：

- **邮件** [kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai)
- **或预约与创始人的会议：**

<CalInline />
