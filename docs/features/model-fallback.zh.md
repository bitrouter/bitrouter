---
title: 模型回退
description: 在请求中传入有序的模型列表，主模型失败时 BitRouter 会沿列表向下尝试。
sourceHash: c1e0ae231186a6055d3ed8910d3afe6f8ae561718f0dd887db20a5ff863a7034
---

LLM 上游会失败。限速、模型宕机、上下文溢出、内容过滤——这些原本会让代理循环卡住的错误，**模型回退** 让你在一次请求里传入一份有序的模型列表，BitRouter 会依次尝试，直到某个模型成功，并将那次成功的响应返回。

这是对 OpenAI、Anthropic、Google 三种协议表面在请求体层面的扩展。无需 SDK——只需设置一个字段。

## 快速示例

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
    "messages": [{"role": "user", "content": "用一句话概括《伊利亚特》。"}]
  }'
```

`model` 字段仍是计费与路由的主模型。`models` 数组以一个有序的优先级列表覆盖它——第一个成功返回的模型胜出。

<Callout type="info">
**当 `models` 已设置时，可以省略 `model`。** 若两者同时存在，`models` 的第一项被视为主模型，`model` 被忽略。建议始终传入 `model`，让你应用里的错误日志保持可读。
</Callout>

## 触发回退的错误

对于**上游侧、可能瞬时**的错误，BitRouter 会顺势尝试下一个模型；对由请求本身引起的 4xx 错误则直接返回给调用方。

| 情形 | 信号 | 行为 |
| --- | --- | --- |
| 触发限速 | `429` | 回退 |
| 服务端错误 | `5xx` | 回退 |
| 超时 / 连接中断 | `408`、网络错误 | 回退 |
| 超出上下文窗口 | 各上游特定错误码 | 回退 |
| 内容过滤 / 拒答 | 各上游特定错误码 | 回退 |
| 流中失败（尚未输出 token） | 在首个 token 之前流被中断 | 回退 |
| 流中失败（已输出 token 之后） | 响应过程中流被中断 | **直接返回**——已有部分输出发出 |
| 鉴权错误 | `401` | 直接返回 |
| 余额不足 / 配额耗尽 | `402`、`403` | 直接返回 |
| 校验 / 请求格式错误 | `400`、`422` | 直接返回 |
| 客户端主动取消 | 客户端断开 | 直接返回 |

回退仅遍历一遍。BitRouter 按顺序对每个模型最多尝试一次，且回退之间不做指数退避——前提是：与其在失败的模型上等待，不如立刻切换到另一个模型。

<Callout type="warn">
**回退以请求为粒度，而非以 token 为粒度。** 如果模型 A 已成功回流到第 50 个 token 后断开，BitRouter 不会悄悄在模型 B 上重启——那会输出一段不连续的响应。请把回退作用在请求边界上；在你的代理里做检查点和续传，以获得流级别的容错。
</Callout>

## 查看是哪个模型应答的

三条线索：

- **响应体的 `model` 字段** — 设置为实际生成响应的模型，而不是你最初请求的那个（OpenAI 约定）。
- **响应头 `bitrouter-served-by`** — `<provider-id>/<model-id>`，例如 `anthropic-direct/anthropic/claude-sonnet-4-6`。
- **响应头 `bitrouter-fallback-trace`** — 以逗号分隔的尝试与结果列表，例如 `openai/gpt-4o:rate_limit,anthropic/claude-sonnet-4-6:served`。仅在至少触发了一次回退时输出。

## 成本与延迟权衡

每次回退都是一次新的上游请求。实操建议：

- 期望最低成本：把 **最便宜的放最前**，接受高负载下尾延迟变差。
- 期望最低延迟：把 **最稳的放最前**，接受单 token 价更高。
- 长时间运行的代理循环：偏向稳定。一次卡住的代理循环带来的代价，远高于两个前沿模型之间那点边际单价差。

如果想跨「同一个模型的不同上游」声明式地优化成本或延迟，参见 [供应商选择](/docs/features/provider-selection)。回退与供应商选择是叠加的：BitRouter 先按你设置的策略为 `models` 中的每个模型挑选最佳上游，仅当当前模型上的上游重试预算耗尽时，才回退到列表中的下一个模型。

## Anthropic 与 Google 协议表面

`models` 字段在 `/v1/messages`（Anthropic Messages）与 `/v1beta/models/{model}:generateContent`（Google Generative AI）上行为一致。在 Anthropic 上，与既有的 `model` 字段一同读取；在 Google 上，BitRouter 将其作为请求体扩展接受——每次尝试时上游的 `:generateContent` 路径会被相应改写。

## 限制

- `models` 最多 8 项。
- 每个模型 ID 必须能在 [registry](/docs/guides/register-as-a-provider) 中解析到一个已注册的模型。未识别的 ID 会在任何上游尝试之前返回 `400`。
- 支持流式。第一个开始输出 token 的模型胜出；即使其后流出现失败，也不会再尝试后续模型。
