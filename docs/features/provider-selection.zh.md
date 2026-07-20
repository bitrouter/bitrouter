---
title: 供应商选择
description: 在 BitRouter Cloud 中，当一个模型由多个供应商承载时，选择供应商排序方式——按成本、延迟或吞吐。
sourceHash: 08b021ccc3b9394f871bb91d23e8b05efba77e223208d9a19f2ba08e51b49f72
---

BitRouter Cloud 上多数模型都由多个供应商承载。当你请求 `openai/gpt-4o` 时，BitRouter 必须挑选一个已注册的上游来发送请求。默认会用一个综合评分；如果想显式选择策略，请给模型 ID 添加 `model:<profile>` 后缀。

<Callout type="warn">
**目前请用 [`model:<profile>` 后缀](/docs/features/model-variants) 选择托管路由策略**——例如 `openai/gpt-4o:latency`。`provider.sort` 这类请求体字段尚在规划中、**暂未启用**；实时 Cloud 请求请使用后缀。
</Callout>

策略只有三种。哪个对当前请求最重要，就选哪个。

## 三种策略

| 策略 | 优化目标 | Tie-break |
| --- | --- | --- |
| `cost` | 最低单次请求成本，按你的 prompt 与预期 completion token 数、按上游当前定价计算。 | 更高在线率 → 更低错误率 → 供应商 ID。 |
| `latency` | 最近 1 小时滑动窗口内观测到的最低 p50 TTFT（首 token 时延）。 | 更高吞吐 → 更高在线率 → 供应商 ID。 |
| `throughput` | 最近 1 小时滑动窗口内观测到的最高输出 tokens/秒。 | 更低 TTFT → 更高在线率 → 供应商 ID。 |

遥测数据每分钟刷新一次。同样的数据可以在 registry 中各模型的页面上查看。

## 快速示例

```bash
curl https://api.bitrouter.ai/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -d '{
    "model": "openai/gpt-4o:latency",
    "messages": [{"role": "user", "content": "把 Hello 翻译成法语。"}]
  }'
```

同一个模型后缀在 `/v1/chat/completions`、`/v1/messages`（Anthropic）和 `/v1beta/models/{model}:generateContent`（Google）上都会生效。

## BYOK 供应商优先

如果你已为某个供应商 [添加了外部密钥](/docs/features/byok)，那么只要该供应商能承载某个模型，BitRouter 就会优先选它——排在所有非 BYOK 供应商之前，且不受所选 profile 影响。BYOK 请求按上游标价直接计入你自己的账号、没有抽成；而且你已经显式选择了这家供应商，让默认行为尊重这个选择，是唯一不会让你后续被意外打到的策略。

在所有 BYOK 候选之中，所选 profile 仍会生效。因此 `openai/gpt-4o:latency` 配合为 OpenAI 与 Anthropic 配置的 BYOK 密钥时，BitRouter 会先按 TTFT 在这两家之间排序；只有当两条 BYOK 通路都失败，才会按延迟向非 BYOK 供应商兜底。

在 **本地 OSS 模式** 下，内置的 Cloud profile 后缀不会预先定义。如果你需要本地逐请求路由选择器，请在 `bitrouter.yaml` 中定义 `variants` 或 presets；否则后缀会保留为字面模型 ID 的一部分。

## 默认行为

没有 profile 后缀时，BitRouter 按**综合评分**排序——成本、延迟、吞吐与在线率的加权组合，并过滤掉在线率过低的供应商。这是大多数代理的合适默认；只有当某一个维度显著重要时，才需要显式设置策略。

<Callout type="info">
**默认行为在不同版本之间不保持稳定。** 综合评分的权重会随我们对真实流量的观察而调整。如果你需要一个稳定、可复现的策略——用于成本核算、SLO 跟踪或 A/B 实验——请显式添加 profile 后缀。
</Callout>

## 与回退如何叠加

[模型回退](/docs/features/model-fallback) 与供应商选择是两层独立机制：

1. 对 `models` 列表中的每个模型（或没有回退时只有 `model`），BitRouter 都会按该模型 ID 上编码的策略挑选最佳上游；没有后缀时使用 `balanced`。
2. 若所选上游遇到不会向调用方暴露的失败（限速、5xx），BitRouter 会先在**同一模型的下一名上游**重试，再切换到列表中的下一个模型。
3. 每个回退模型字符串都会独立解析，因此你可以给所有条目使用同一个后缀，也可以为不同回退条目选择不同 profile。

具体来说：`models: ["openai/gpt-4o:cost", "anthropic/claude-sonnet-4-6:cost"]` 会先评估 GPT-4o 中最便宜的上游，再评估 Sonnet 中最便宜的上游，再返回错误。

## 指标打平时

如果两家供应商对同一个 prompt 报价完全相同，那么在线率更高的胜出；若在线率也打平，则错误率更低的胜出；若一切都打平，BitRouter 按供应商 ID 字典序排序——确定且便于审计，但**不会做负载均衡**。如果在打平的供应商间均匀分发流量对你的工作负载很重要，请到 [Discord](https://discord.gg/G3zVrZDa5C) 描述用例；若有需求，我们会增加 `provider.balance` 旋钮。

## 没在这里的

OpenRouter 暴露了大得多的旋钮面——`provider.order`、`provider.allow_fallbacks`、`provider.require_parameters`、`provider.data_collection`、`provider.ignore`、`provider.quantizations`，等等。在使用情况告诉我们需要更多之前，我们刻意只保留这一个旋钮、三个取值。如果你正从 OpenRouter 迁移过来，两条等价表达：

- **绑定到某一家供应商** — 直接使用带供应商前缀的模型 ID，例如 `model: "anthropic-direct/anthropic/claude-sonnet-4-6"`。
- **排除某一家供应商** — 在工作区的 registry 白名单里去掉它，而非放到请求体里。

如果缺失的旋钮挡住了你的真实工作负载，请到 [bitrouter](https://github.com/bitrouter/bitrouter) 提 issue。
