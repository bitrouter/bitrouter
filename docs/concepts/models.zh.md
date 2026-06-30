---
title: 模型（Models）
description: 在 BitRouter 上，模型是由众多供应商共同提供的聚合体——通过四种协议触达、逐请求排序，并提供折扣的开源供给。
sourceHash: 5db2bbc903ccb998b052947933b808037a2c8e06a473f3657c0e65672db6eecf
---

在 BitRouter 上，"模型"并不是单个端点，而是一个**聚合体**：一个逻辑模型——比如 `openai/gpt-4o` 或 `anthropic/claude-sonnet-4.6`——可以同时由多个供应商提供。你通过一个稳定的**模型 id** 来访问它，由 BitRouter 决定每个请求实际由哪个底层供应商端点来响应。

这层间接正是关键所在。你的 agent 针对 `anthropic/claude-sonnet-4.6` 编写，而它背后的供应商集合可以增加、减少或重新定价，你都无需改动一行代码。

## 四种入口协议

你可以用运行时已经会说的任意一种 API 触达模型网关。BitRouter 在同一个本地端点上并排暴露**四种协议**：

- **OpenAI Chat Completions** —— `POST /v1/chat/completions`
- **OpenAI Responses** —— `POST /v1/responses`
- **Anthropic Messages** —— `POST /v1/messages`
- **Google Generative AI** —— `POST /v1beta/models/{model}:generateContent`

挑你的 SDK 已经接好的那一个——你无需换一套新客户端。而且正因为网关四种协议都会说，它还能**跨协议路由**：以 Anthropic Messages 形式到达的请求，可以由 OpenAI 供应商来响应，反之亦然。一个模型 id，四种方式触达，任何符合条件的供应商都能作答。

## 一个 id，多个供应商

正因为模型是聚合体，请求它会触发一次**供应商选择**步骤。默认情况下，BitRouter 按一个均衡评分对符合条件的供应商排序——综合成本、延迟、吞吐与可用性——并把请求发往最优者。当所选供应商出现瞬时故障时，可以回退到下一个排名的供应商，或回退到你列出的下一个模型。

## 变体为单次请求重新排序

当一个模型有多个供应商时，你有时希望为某一次调用调整这个排序。**模型变体**是 id 上的内联后缀——`:cost`、`:latency`、`:throughput`——它仅针对该请求、沿你指定的轴对*符合条件*的供应商重新排序。它从不改变哪些供应商符合条件，也从不改变授权；不带后缀的裸 id 就是默认的均衡策略。

## 开源模型，享折扣

开源（非闭源）模型还具备第二个特性：BitRouter 通过自托管供应商以**默认低于官方定价 25%** 的价格提供它们，无需任何后缀或配置。`:discount` 后缀会把请求显式钉到该供给上，账号上的任何自定义折扣也在此生效。

## 了解如何使用

- [供应商选择](/docs/features/provider-selection) —— 一个模型背后的供应商如何排序。
- [模型回退](/docs/features/model-fallback) —— 传入有序列表，失败时逐一回退。
- [模型变体](/docs/features/model-variants) —— `:cost` / `:latency` / `:throughput` 后缀。
- [预设](/docs/features/presets) —— 具名、可复用的路由配置。
- [结构化输出](/docs/features/structured-outputs) —— 跨供应商强制执行 JSON schema。
- [添加外部密钥（BYOK）](/docs/features/byok) —— 通过你自己的供应商账号路由。
- [本地与私有模型](/docs/integrations/models) —— 把 BitRouter 指向你自己的服务器。
- [托管供应商与定价](/docs/get-started/models-and-providers) —— 托管供应商及完整目录。
