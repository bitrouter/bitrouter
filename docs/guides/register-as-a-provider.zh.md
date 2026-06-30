---
title: 注册成为供应商
description: 通过开放注册表，无许可地接入 BitRouter。
sourceHash: fbd8e3657e0ad52a64d4e1c7c60b40456b011144613137df839642da530169ec
---

<Callout type="warn">
BitRouter Registry 目前处于**实验阶段**。在此期间，PR 会逐个审阅与合并，且我们保留拒绝或下架提供商的权利——详见下方[使用条款](#使用条款)。最终目标是完全无许可的接入流程；当前的审阅环节仅是为了在过渡期间维护网络的健康。
</Callout>

## BitRouter 注册表

BitRouter 被设计为一个面向推理的**开放市场**。任何运行 OpenAI 或 Anthropic 兼容推理端点的服务都可以申请注册——无需商业协议、无销售流程、无业务表单。

提供商列表存放于一个开源公共仓库：

> **[github.com/bitrouter/provider-registry](https://github.com/bitrouter/provider-registry)**

通过提交一份包含提供商清单（manifest）的 PR 即可申请注册。合并后，BitRouter 及网络上的任何 Agent 都能发现你的模型、向其路由请求，并通过 x402/MPP（可选）按请求付款。

## 注册流程

1. Fork [provider-registry](https://github.com/bitrouter/provider-registry)。
2. 在 `providers/<your-provider-id>.yaml` 添加一份清单，描述你的端点和模型目录。
3. 提交 PR。CI 会运行 schema 校验与基础可达性检查。
4. 维护者会审阅 PR。我们以开放接入为目标，但在实验阶段会逐个评估每个 PR。
5. 合并后，你的模型会在 BitRouter 网络与 bitrouter.ai 托管市场上变得可被发现。

<Callout type="info">
**进入注册表 ≠ 一定有流量。** 注册表只是让你的模型可被发现，并不保证任何推理流量。BitRouter 运行一套独立的**路由系统**，持续监控、评分并排名所有提供商——详见下方[可靠性与路由](#可靠性与路由)。流量会流向得分较好的提供商；信号较弱的会被降权，与是否在注册表中无关。
</Callout>

## 提供商清单（Manifest）

清单声明端点元数据、支付方式，以及 BitRouter 用于轮询的模型目录 URL。

```yaml
id: example-provider
name: Example AI
endpoint: https://api.example.ai/v1
protocol: openai      # openai | anthropic
models_url: https://api.example.ai/v1/models
payment:
  modes: [byok, x402]
  x402_address: "0x..."          # 可选，用于托管模式下的 Agent 支付
homepage: https://example.ai
support: https://example.ai/support
```

## 模型端点

BitRouter 会周期性地轮询你的 `models_url`，并按以下 schema 解析所有可用模型：

```json
{
  "data": [
    {
      "id": "example/sonnet-1",
      "name": "Example: Sonnet 1",
      "created": 1704067200,
      "input_modalities": ["text", "image"],
      "output_modalities": ["text"],
      "context_length": 1000000,
      "max_output_length": 128000,
      "quantization": "fp8",
      "pricing": {
        "prompt": "0.000003",
        "completion": "0.000015",
        "image": "0",
        "request": "0",
        "input_cache_read": "0"
      },
      "supported_sampling_parameters": ["temperature", "top_p", "stop"],
      "supported_features": ["tools", "json_mode", "structured_outputs", "reasoning"],
      "is_ready": true
    }
  ]
}
```

关键字段：

- **`id`** — BitRouter 转发请求时使用的精确模型标识符。
- **`pricing`** — 每 token 的美元价格，使用字符串避免浮点精度问题。`image` 与 `request` 为单位价。
- **`is_ready`** — 设为 `false` 可在不上线的情况下暂存模型（适用于尚未发布的模型）。默认为 `true`。

合法 `quantization`：`int4`、`int8`、`fp4`、`fp6`、`fp8`、`fp16`、`bf16`、`fp32`。

合法 `supported_features`：`tools`、`json_mode`、`structured_outputs`、`logprobs`、`web_search`、`reasoning`。

合法 `supported_sampling_parameters`：`temperature`、`top_p`、`top_k`、`min_p`、`top_a`、`frequency_penalty`、`presence_penalty`、`repetition_penalty`、`stop`、`seed`、`max_tokens`、`logit_bias`。

### 分级定价

对于长上下文分级定价，可将 `pricing` 提供为数组。第一个条目为基础档位，后续条目在输入 token 数达到 `min_context` 时生效。

```json
{
  "pricing": [
    { "prompt": "0.000002", "completion": "0.000012" },
    { "prompt": "0.000004", "completion": "0.000018", "min_context": 200000 }
  ]
}
```

### 弃用

为计划下线的模型设置 `deprecation_date`（ISO 8601，`YYYY-MM-DD`）。BitRouter 会向消费者展示弃用提示，并可能在弃用日期后自动隐藏该模型。

## 可靠性与路由

注册表告诉 BitRouter 你的端点上**有什么模型**，**路由系统**则决定向你发送多少流量——两者是独立的系统。

路由系统会持续监控每个端点的稳定性与性能，对所有提供商实时评分与排名。评分实时更新——若某个提供商的表现下滑，流量会在几分钟内迁移走。

**稳定性** — 成功请求数 ÷ 总请求数，扣除用户输入错误。

- 计入：`401`、`402`、`404`、`5xx`、流式中断错误、success-with-error-finish-reason。
- 不计入：`400`、`413`、`429`、`403`（单独追踪）。

**性能** — TTFT（首个 token 时间）与吞吐量（输出 token 数 ÷ 总生成时间，包含排队 + 取数 + 流式时间）。两者会公开展示在每个模型页面上。

要获得高分与流量：

- 过载时尽快返回 `429`，避免排队。
- token 一就绪即开始流式输出。
- 在生成 token 之前的长耗时阶段（如推理模型），通过 SSE 注释发送心跳，避免路由系统因超时而切换到回退提供商。

长期得分较差的提供商会被降权。滥用网络或违反下方[使用条款](#使用条款)的提供商会被直接下架。

## 支付

提供商可在清单中声明三种支付模式：

- **`byok`** — 消费者自带你服务的 API 密钥，BitRouter 不参与支付。
- **`x402`** — 接受 Agent 原生的 x402/MPP 按请求付款。BitRouter 将支付凭证转发到你清单中指定的地址。
- **`invoice`** — 为 BitRouter 聚合的托管模式流量启用发票账单。

新接入的提供商通常先开启 `byok` 与 `x402`，待流量增长后再启用 `invoice`。

## 更新你的列表

清单更新（新模型、价格变更、端点迁移）通过对 registry 提交 PR 完成。价格变更在合并后即时生效。要下线某个模型，可在下次模型端点轮询中将其设置为 `is_ready: false`，或提交 PR 删除该条目。

## 使用条款

注册成为提供商即表示你同意以下条款。违反条款将导致从注册表与托管市场中下架。

- **禁止有害内容** — 端点不得用于生成或明知协助生成 CSAM、大规模杀伤性武器相关知识、定向骚扰，或其他适用法律所禁止的内容。
- **禁止恶意行为** — 不得进行凭证窃取、对 Agent 的 prompt injection 攻击、响应篡改或隐蔽数据外泄。
- **元数据真实** — 清单与模型端点必须如实反映你的端点实际提供的内容：定价、上下文长度、能力、模态。元数据造假将导致立即下架。
- **合理可用性** — 长期未通过健康检查或对维护者沟通无回应的提供商，可能被下架。
- **合法运营** — 提供商应遵守其运营所在司法辖区的法律，包括相关的出口管制与制裁要求。

在实验阶段，我们保留以自身判断拒绝或下架任何提供商的权利。随着网络与工具链成熟，我们会将这一裁量权逐步收敛为客观、自动化的标准。

## 获取支持

- 在 [provider-registry](https://github.com/bitrouter/provider-registry/issues) 提交 Issue。
- 加入我们的 [Discord](https://discord.gg/G3zVrZDa5C)。
