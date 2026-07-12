---
title: Advisor
description: 在任务中途咨询一个更强的模型——发起调用的模型提出一个自包含的问题并取回可据以行动的建议，而无需把整个任务交出去。
sourceHash: 1654b07a58e9e24b42977f9f0ba02d4fa5a60097ad0862b2195f09c13dfac363
---

**Advisor** 是一个服务端工具（server tool）：BitRouter 在生成过程中自行执行它，而不是把调用交还给你的客户端。当发起调用的模型遇到拿不准的地方时，它会向一个更强的顾问模型提出一个自包含的问题，并取回可据以行动的建议——而不必委派整个任务。用它让一个快速、廉价的主模型，把那些难啃的子问题升级给更强的模型。

## 快速示例

在请求的 `tools` 数组上声明 `advisor` 即可启用顾问模型。声明会固定顾问模型与指令；在调用时由模型提出它的问题：

<Tabs items={['Responses API', 'Anthropic Messages']}>
<Tab value="Responses API">

```bash
curl https://api.bitrouter.ai/v1/responses \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-haiku-4.5",
    "input": "重构这个 auth 模块。在改动 token 轮换逻辑之前先咨询 advisor。",
    "tools": [
      {
        "type": "advisor",
        "model": "anthropic/claude-opus-4.8",
        "instructions": "你是一名资深安全工程师。对认证边界情况要表述精确。"
      }
    ]
  }'
```

</Tab>
<Tab value="Anthropic Messages">

```bash
curl https://api.bitrouter.ai/v1/messages \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-haiku-4.5",
    "max_tokens": 4096,
    "messages": [
      {"role": "user", "content": "重构这个 auth 模块。在改动 token 轮换逻辑之前先咨询 advisor。"}
    ],
    "tools": [
      {
        "type": "advisor",
        "name": "advisor",
        "model": "anthropic/claude-opus-4.8",
        "instructions": "你是一名资深安全工程师。对认证边界情况要表述精确。"
      }
    ]
  }'
```

</Tab>
</Tabs>

<Callout type="warning">
**协议支持。** `advisor` 声明适用于 **OpenAI Responses API**（`/v1/responses`）与 **Anthropic Messages API**（`/v1/messages`），二者都能在传输层承载服务端工具。**Chat Completions** 的 `tools` 数组只接受 `{type:"function"}` 条目，无法承载该声明。
</Callout>

## 工作原理

当发起调用的模型调用该工具时，它会提供：

| 参数 | 说明 |
| --- | --- |
| `prompt` | 给顾问的一个清晰、自包含的问题。 |
| `model` | 可选，按本次调用覆盖顾问模型。 |

顾问作答，其建议返回给发起调用的模型，后者随即继续任务。与 [Sub-agent](/docs/features/subagent) 不同，顾问不会接管工作——它只回答一个问题。

## 配置

声明在 `tools` 数组的条目上：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `model` | 字符串 | 顾问模型。缺省时回退到本次调用的 `model` 参数，再回退到父请求模型。 |
| `instructions` | 字符串 | 给顾问的系统指令。 |
| `tools` | 数组 | 顾问可使用的供应商服务端工具（如网页搜索），以供应商命名空间的声明形式给出。 |

<Callout type="info">
**受服务端工具循环约束。** 一次 advisor 调用运行在一个有界循环中——默认 10 个工具轮次、单工具 30 秒、每轮总预算 120 秒。
</Callout>

## 在云端

Advisor 在 **BitRouter Cloud** 上开箱即用并由平台托管，顾问调用的每次运行成本都可在请求日志中查看。自托管用户用 `server_tools.advisor: true` 启用；启用后，仅当调用方声明它时才会在该请求上对外公开。

## 另见

- [Sub-agent](/docs/features/subagent) —— 委派一个自包含的任务，而非提一个问题
- [Fusion](/docs/features/fusion) —— 让一组模型对同一个提示词进行评议
- [模型变体](/docs/features/model-variants) —— 用某个模型的更强变体作为顾问
