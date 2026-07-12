---
title: Sub-agent
description: 在生成过程中把一个自包含的任务委派给专注的工作模型——工作模型只看到你给它的内容，并返回最终结果。
sourceHash: 2a7c24c88c1728c4c0f164738dfd58c7f424e0dbfc5d545bab0f92d71ed6cb3a
---

**Sub-agent** 是一个服务端工具（server tool）：BitRouter 在生成过程中自行执行它，而不是把调用交还给你的客户端。发起调用的模型把一个自包含的任务——`task_name` 与 `task_description`——交给一个专注的工作模型（通常更便宜或更快），后者在隔离环境中完成任务并仅返回最终结果。用它把繁琐的体力活分发出去，而不占用主模型的上下文。

## 快速示例

在请求的 `tools` 数组上声明 `subagent` 即可启用工作模型。声明会固定工作模型与指令；在调用时由模型填入具体任务：

<Tabs items={['Responses API', 'Anthropic Messages']}>
<Tab value="Responses API">

```bash
curl https://api.bitrouter.ai/v1/responses \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-opus-4.8",
    "input": "为这 20 张支持工单各写一句摘要，然后按主题分组。",
    "tools": [
      {
        "type": "subagent",
        "model": "anthropic/claude-haiku-4.5",
        "instructions": "你是一个简洁的摘要器。每张工单返回一句话。"
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
    "model": "anthropic/claude-opus-4.8",
    "max_tokens": 4096,
    "messages": [
      {"role": "user", "content": "为这 20 张支持工单各写一句摘要，然后按主题分组。"}
    ],
    "tools": [
      {
        "type": "subagent",
        "name": "subagent",
        "model": "anthropic/claude-haiku-4.5",
        "instructions": "你是一个简洁的摘要器。每张工单返回一句话。"
      }
    ]
  }'
```

</Tab>
</Tabs>

<Callout type="warning">
**协议支持。** `subagent` 声明适用于 **OpenAI Responses API**（`/v1/responses`）与 **Anthropic Messages API**（`/v1/messages`），二者都能在传输层承载服务端工具。**Chat Completions** 的 `tools` 数组只接受 `{type:"function"}` 条目，无法承载该声明。
</Callout>

## 工作原理

当发起调用的模型调用该工具时，它会提供：

| 参数 | 说明 |
| --- | --- |
| `task_name` | 任务的简短标识。 |
| `task_description` | 完整、自包含的任务：上下文、输入与期望输出。 |

工作模型**只**看到 `task_description`——没有任何其他对话上下文——执行任务，其最终结果返回给发起调用的模型。由于工作模型是隔离的，发起调用的模型必须把工作模型所需的一切都写进描述里。

## 配置

声明在 `tools` 数组的条目上：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `model` | 字符串 | 工作模型。默认为父请求模型。 |
| `instructions` | 字符串 | 给工作模型的系统指令。 |
| `tools` | 数组 | 工作模型可使用的供应商服务端工具（如网页搜索），以供应商命名空间的声明形式给出。 |

<Callout type="info">
**受服务端工具循环约束。** 一次 sub-agent 调用运行在一个有界循环中——默认 10 个工具轮次、单工具 30 秒、每轮总预算 120 秒。
</Callout>

## 在云端

Sub-agent 在 **BitRouter Cloud** 上开箱即用并由平台托管，工作模型调用的每次运行成本都可在请求日志中查看。自托管用户用 `server_tools.subagent: true` 启用；启用后，仅当调用方声明它时才会在该请求上对外公开。

## 另见

- [Advisor](/docs/features/advisor) —— 咨询一个更强的模型获取建议，而非委派整个任务
- [Fusion](/docs/features/fusion) —— 让一组模型对同一个提示词进行评议
- [供应商选择](/docs/features/provider-selection) —— 控制由哪个供应商承接工作模型
