---
title: Introduction
description: 一个开源的智能体式 LLM 网关，通过把模型、工具与智能体都变成可路由的原语，为你的生产智能体循环做成本优化——且无需修改任何 harness。
sourceHash: d98ac30ce61a9f4c773cb4c3d2cb2e7f5123012dacae3b17c6f32904ecdeeefb
---

## 什么是 BitRouter？

BitRouter 是一个**开源的智能体式（agentic）LLM 网关与路由器，为你的生产智能体循环做成本优化**。它是一个本地二进制，为任意智能体提供统一端点，把它的模型调用、工具与子智能体路由到仍能达成目标的最便宜路径——且**无需修改任何 harness**。把你的运行时指向它，每个循环的每一步默认都不再按前沿模型的价格计费。

它运行在你的智能体所在的任何地方，无需安装依赖，并作为一个无许可网络运行——任意提供商可注册，任意智能体可接入。[核心（Core）](/docs/get-started/self-hosted-and-cloud)**以 Apache 2.0 协议开源，可免费自托管**——自带密钥或运行本地模型即可，无需付费。[云（Cloud）](/docs/get-started/self-hosted-and-cloud)是可选的托管层，在其之上额外提供托管提供商、智能体自主支付与账户级策略。完整的[模型与定价](/docs/get-started/models)目录见此。

## 三个原语，一个网关

一个智能体循环消耗三样东西。多数路由器只治理第一样——BitRouter 让这三样都可路由、可观测、可做成本治理：

- **模型（Models）** — 跨提供商、跨协议、跨账户地路由 LLM 调用（经典路由器，且跨协议）。参见 [模型路由](/docs/concepts/models)。
- **工具（Tools）** — 一个 **MCP 网关**与一个 **AgentSkills 网关**：工具与技能成为受治理、可路由的资源，而非硬编码的端点。参见 [Tools](/docs/concepts/tools)。
- **智能体（Agents）** — 一个 **ACP 网关**：子智能体是一等公民，因此你把一个任务交给更便宜的智能体，就像把一次调用路由到更便宜的模型一样。参见 [Agents](/docs/concepts/agents)。

成本优化不只是选模型——而是用仍能把循环带到目标的最便宜的模型、最便宜的工具与最便宜的子智能体。

## 自我改进的循环

BitRouter 用第二个循环把你的智能体循环包裹起来。每个循环都有自己的 [策略（policy）](/docs/concepts/policy)——一份声明其调用、工具与智能体如何路由的规格——BitRouter 对它持续运行一个**观察 → 评估 → 行动**的周期：

- **观察** — 每一次模型、工具与智能体调用，并把成本与结果归因到具体的一跳。
- **评估** — 针对该循环的目标为每次运行打分。
- **行动** — 更新策略。让一个智能体依据评估信号自我调优，或由你亲自编辑。

其结果是一个在生产中运行越久就越便宜的循环——而无需为本就不需要前沿模型的工作反复支付前沿价格。

## 为什么智能体运行在 BitRouter 上

四种机制，内建于路由器——而非逐个智能体地外挂。

### 可靠性 —— 一个提供商失败，你的智能体运行不会失败

BitRouter 在运行途中跨提供商透明地重新路由——你的智能体永远看不到那次失败的调用。带指数退避的自动重试、模型与提供商回退、连接复用与请求级幂等性，让长时的智能体循环在故障与 `429` 中存活。失败的请求不计费。参见 [模型回退](/docs/features/model-fallback) 与 [供应商选择](/docs/features/provider-selection)。

### 可观测性 —— 追踪每一跳，而非只是每次请求

完整的调用链可见性：每个智能体、每个模型、每一步，成本按**每次运行**而非每月归因。BitRouter 原生支持 OpenTelemetry——trace 与指标通过 OTLP 导出到你运行的任意后端，并可用 `bitrouter observe status` 查看导出器的实时状态。参见 [OpenTelemetry](/docs/features/opentelemetry)。

### 安全 —— 为每个智能体配置一次的护栏

用正则护栏对风险提示与输出脱敏或拦截，外加限速——在路由器层为每个智能体统一执行一次，无需任何应用层改动。结合逐智能体的 [KYA](/docs/features/payment) 身份，一个持有你密钥的自主智能体就不再是一个无人监管的攻击面。参见 [护栏](/docs/features/guardrails)。

### 效率 —— 不是每次调用都需要你最强的模型

一次运行中的多数调用都很琐碎——一次查找、一次格式化、一个是非判断。BitRouter 按任务复杂度用价格感知路由把每次调用匹配到合适的模型，让你不再按前沿价格为简单调用计费。这些节省会在每次运行中累积。

## 基础

- **通用 LLM API** — 单一二进制，四套协议：OpenAI Chat Completions、OpenAI Responses、Anthropic Messages 与 Google Generative AI。以你偏好的协议访问任意 LLM，并跨协议路由（OpenAI ↔ Anthropic）。
- **免费 BYOK** — 自带提供商密钥，零费用使用。BitRouter 自动从环境变量探测密钥——无需配置文件。你也可以将 BitRouter 指向自己的本地模型（Ollama、vLLM、LM Studio），完全免费——详见 [本地与私有模型](/docs/integrations/models)。
- **MCP & ACP 网关** — 代理 [MCP](https://modelcontextprotocol.io) 服务器，让智能体跨主机发现与调用工具。支持 [ACP](https://github.com/zed-industries/acp)，实现智能体身份、发现与任务分派。
- **智能体式认证与支付** — KYA（Know-Your-Agent）身份与 x402/MPP 按使用付费的托管服务。智能体自主认证与付款——无需信用卡、预充值或发票。
- **开放生态** — 无许可的 [提供商注册](/docs/guides/register-as-a-provider)。任何暴露 OpenAI 或 Anthropic 兼容端点的提供商，都可以加入网络并被网络上的智能体发现。

## BitRouter 如何对比

OpenRouter、LiteLLM、Portkey 与 Bifrost 都路由 LLM 流量，TensorZero 还增加了一个模型优化循环——但 BitRouter 是唯一一个把**模型、工具与智能体视为单一可路由面**、并按成本优化整个生产**循环**（而不只是静态选模型）的方案。它开源、可自托管、且以 Rust 原生，具备自动的运行途中故障转移与低于 10ms 的路由开销。完整对比见 [Comparison](/docs/get-started/comparison)。

## 我们为什么要做这件事

今天的 LLM 智能体会因单个提供商的一次中断而丢失数小时的工作，每次换模型都要重写集成代码，交付风险输出却没有一致的脱敏或拦截手段，并因每个提供商只展示自己那一片而在黑暗中运行。BitRouter 用自动回退在中断中存活，让智能体无需改代码即可换模型，在代理层脱敏或拦截风险内容，并把每一次调用、成本与错误汇聚到一个信息流中。更长远的目标，是一层开放、无许可的智能层——智能体在其中发现、路由并为自己的资源付费——由使用它的智能体与运营者所拥有，而非中间的某家网关公司。

## 智能体运行时

BitRouter 是任何支持自定义 OpenAI 或 Anthropic base URL 的运行时的即插即用代理——将其指向 `http://127.0.0.1:4356` 即可。

OpenClaw、Hermes Agent、Claude Code 等的接入方法详见 [集成](/docs/integrations)。

机器可读文档与即插即用智能体技能，详见 [AI 资源](/docs/ai-resources)。
