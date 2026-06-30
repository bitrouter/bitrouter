---
title: 模型变体
description: 在模型 ID 后追加 :cost、:latency 或 :throughput，即可内联选择 BitRouter 如何对供应商排序——逐请求、无需任何请求体字段。
sourceHash: 9704500065fd7a0150d6564893f63556411fefc50300238ffb7d68e85410b8eb
---

当一个模型由多个供应商承载时，BitRouter 必须为每个请求挑选发往哪个上游。**模型变体**让你内联做出这个选择：在模型 ID 后追加一个 `:<profile>` 后缀，BitRouter 就会按你指定的维度对供应商排序。

后缀本身就是 `model` 字符串的一部分，因此既不需要请求体字段、也不需要 SDK——在 OpenAI、Anthropic、Google 三套接口上行为一致。

## 三种 profile

| 模型 ID | 供应商排序依据 |
| --- | --- |
| `openai/gpt-4o` | **均衡**（默认）——成本、延迟、吞吐的加权综合。 |
| `openai/gpt-4o:cost` | 最便宜优先。 |
| `openai/gpt-4o:latency` | 最低 p50 首 token 时延。 |
| `openai/gpt-4o:throughput` | 最高输出 tokens/秒。 |

这三个维度与 [供应商选择](/docs/features/provider-selection) 中描述的完全一致，只是以内联简写的形式暴露。不带后缀的模型 ID 等价于 `:balanced`。

## 快速示例

```bash
curl http://127.0.0.1:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-sonnet-4.6:cost",
    "messages": [{"role": "user", "content": "把 Hello 翻译成法语。"}]
  }'
```

该后缀在 `/v1/messages`（Anthropic）和 `/v1beta/models/{model}:generateContent`（Google）上行为一致——它随模型 ID 一起传递。

## 行为

- **不带后缀即均衡默认。** 不加后缀等价于 `:balanced`，与你现在得到的路由逐字节一致。变体只会对已有候选供应商**重新排序**，绝不改变哪些供应商有资格。
- **偏置是激进的。** 一个 profile 会把所选维度的权重压得很高，另两个维度仅作平手裁决之用；因此 `:cost` 会挑出真正最便宜的供应商，而不是“稍微便宜一点”。需要综合评分时，请用 `:balanced`（或不加后缀）。
- **未知后缀不是 profile。** 只有 `cost`、`latency`、`throughput`、`balanced` 会被识别为 profile。其他任何写法——比如 `openai/gpt-4o:fast`——都会被当作模型 ID 的一部分，从而以普通的“未知模型” `404` 失败，而非被静默改路由。（这与把 `openai:gpt-4o` 这类 `provider:model` ID 原样保留是同一条规则。）另一个被识别的后缀 [`:discount`](/docs/get-started/models-and-providers) 会路由到 BitRouter 的自托管折扣供应商——并非路由 profile——并被单独处理。
- **仅在 ≥2 个供应商时才有意义。** 只由单一供应商承载的模型，加不加后缀路由结果都一样。

<Callout type="info">
**成本从第一天起就精确，延迟与吞吐需要预热。** `:cost` 按上游当前定价计算，因此立即生效。`:latency` 与 `:throughput` 读取最近 1 小时的滑动遥测窗口——在冷启动或低流量的模型上，会先贴近均衡评分，直到样本累积足够。
</Callout>

## `:discount` 后缀

除了上面的排序 profile，BitRouter 还接受一个 `:discount` 后缀，把请求路由到 BitRouter 的**自托管供应商**——开放模型在那里以低于官方的价格提供：

```text
moonshotai/kimi-k2.6:discount     # 路由到自托管折扣供应商
```

由于它固定供应商、而非对候选供应商重新排序，`:discount` **不是**路由 profile，也不与 `:cost` / `:latency` / `:throughput` 有意义地组合。开放模型即使不加该后缀也默认立享 25% 折扣；该后缀会强制使用折扣的自托管供应来源，账户上的定制折扣也在此生效。完整行为见[折扣模型](/docs/get-started/models-and-providers)——包括面向开源项目的最高 50% 定制折扣。

## 变体绝不改变鉴权

profile 只影响供应商的**排序**。其余一切都以底层模型 ID 为准：

- [Guardrail](/docs/features/guardrails) 的模型白名单/黑名单与 BYOK 规则，会把 `anthropic/claude-sonnet-4.6:cost` 完全当作 `anthropic/claude-sonnet-4.6` 来判定——profile 永远无法放宽或绕过策略。
- [BYOK](/docs/features/byok) 供应商仍然排在平台供应商之前；profile 只在每个层级**内部**排序。
- 计费不变——你按所选供应商对底层模型的费率付费。

## 查看某次请求用了哪个 profile

所选 profile 会记录在每次请求上，并在用量历史中返回。`GET /v1/namespaces/{nsid}/requests` 的每一行都带有 `routing_profile` 字段（`balanced` / `cost` / `latency` / `throughput`），便于你审计各策略各自占了多少流量。

## 与 `provider.sort` 的关系

目前选择路由 profile 的受支持方式就是模型 ID 后缀。请求体形式的等价物 `provider.sort` 在 [供应商选择](/docs/features/provider-selection) 中有描述，但**尚未启用**；在它上线之前，请使用后缀。
