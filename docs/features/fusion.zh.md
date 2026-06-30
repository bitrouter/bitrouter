---
title: Fusion
description: 多模型协同评议——一组模型并行作答，由裁判模型比对各自的答案，再由你的模型基于结构化分析写出最终回复。
sourceHash: 023a1f0a6746d2c7f985aa44b314fa09d8ec26655957ccc70147968d510a7211
---

**Fusion** 是一个服务端工具（server tool）：BitRouter 在生成过程中自行执行它，而不是把调用交还给你的客户端。一组**面板（panel）**模型并行回答你的提示词，一个**裁判（judge）**模型把它们的答案比对（而非合并）成结构化分析，最后由发起调用的模型基于该分析写出最终答案。适用于那些高难度、高风险、单个模型的盲点值得被发现的问题。

## 快速示例

使用 Fusion 最简单的方式是 `bitrouter/fusion` **模型别名**——把它设为 `model`，BitRouter 就会用你部署的默认面板与裁判把请求展开为一次评议。该方式适用于任何协议，包括 Chat Completions：

<Tabs items={['模型别名 (cURL)', '显式面板 (Responses)']}>
<Tab value="模型别名 (cURL)">

```bash
curl https://api.bitrouter.ai/v1/chat/completions \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "bitrouter/fusion",
    "messages": [
      {"role": "user", "content": "我们是否应该把事件管道从 Kafka 迁移到托管方案？请分析权衡。"}
    ]
  }'
```

别名会把请求改写到一个默认的外层模型上，附加 Fusion 声明，并提示模型调用一次该工具。你会得到一个经过评议的单一答案。

</Tab>
<Tab value="显式面板 (Responses)">

自行声明 Fusion，即可精确指定面板、裁判，以及（可选的）专用合成器（synthesizer）。Fusion 以服务端工具的形式挂在请求的 `tools` 数组上：

```bash
curl https://api.bitrouter.ai/v1/responses \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-opus-4.8",
    "input": "我们是否应该把事件管道从 Kafka 迁移到托管方案？",
    "tools": [
      {
        "type": "fusion",
        "panel": [
          {"model": "anthropic/claude-opus-4.8"},
          {"model": "openai/gpt-5.5"},
          {"model": "google/gemini-3-pro-preview"}
        ],
        "judge": {"model": "anthropic/claude-opus-4.8"}
      }
    ]
  }'
```

</Tab>
</Tabs>

<Callout type="warning">
**协议支持。** `tools` 数组的显式声明适用于 **OpenAI Responses API**（`/v1/responses`）与 **Anthropic Messages API**（`/v1/messages`），二者都能在传输层承载服务端工具。**Chat Completions** 的 `tools` 数组只接受 `{type:"function"}` 条目，因此在该协议下请改用 **`bitrouter/fusion` 模型别名** 来声明 Fusion。
</Callout>

## 工作原理

1. **面板** —— 面板中的每个模型并行回答你的提示词。每个成员都可以配备供应商提供的网络工具（如网页搜索）来为其答案提供依据。
2. **裁判** —— 裁判模型比对面板的各份答案，返回结构化分析：`consensus`（共识）、`contradictions`（矛盾）、`partial_coverage`（部分覆盖）、`unique_insights`（独特洞见）与 `blind_spots`（盲点）。裁判**只比对、不合并**——它呈现各模型在哪里一致、在哪里分歧，而不是把它们糅成一团。
3. **合成** —— 发起调用的模型（或专用的 `synthesizer`）读取该分析，写出最终答案。

## 配置

声明在 `tools` 数组的条目上：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `panel` | 数组 | 每个并行作答的模型一项。每项为 `{ "model": string, "tools"?: array }`，其中 `tools` 是转发给该成员的供应商网络工具。默认为请求模型上的单成员面板。上限为 **8** 个成员。 |
| `judge` | 对象 | `{ "model": string }` —— 比对面板答案的模型。默认为请求模型。 |
| `synthesizer` | 字符串 | 可选的专用模型，用于写出最终答案。省略时，由发起调用的模型基于返回的分析来撰写。 |

`bitrouter/fusion` 别名展开时所用的部署级默认值——面板、裁判、合成器，以及转发给每个成员的网络工具——在 `server_tools.fusion` 配置段中设置。请求上的显式声明会覆盖它们。

<Callout type="info">
**受服务端工具循环约束。** 与所有服务端工具一样，一次 Fusion 调用运行在一个有界循环中——默认 10 个工具轮次、单工具 30 秒、每轮总预算 120 秒。超出预算的面板会以截断（truncation）结束原因终止。
</Callout>

## 在云端

Fusion 在 **BitRouter Cloud** 上开箱即用并由平台托管——一套精选的默认面板与裁判支撑着 `bitrouter/fusion` 别名，每次运行的成本以及完整的「面板→裁判→合成」调用链都可在请求日志中查看。自托管用户只需在配置中添加 `server_tools.fusion` 段即可启用（其存在会同时开启 `fusion` 服务端工具与 `bitrouter/fusion` 别名）。

## 另见

- [Sub-agent](/docs/features/subagent) —— 把一个自包含的任务委派给单个工作模型
- [Advisor](/docs/features/advisor) —— 在任务中途咨询一个更强的模型
- [供应商选择](/docs/features/provider-selection) —— 控制由哪个供应商承接每个面板模型
