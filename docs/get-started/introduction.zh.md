---
title: Introduction
description: 一个开源的自适应 LLM 网关，通过把模型、工具与智能体都变成可路由的原语，为你的生产智能体循环做优化——且无需修改任何 harness。
sourceHash: d98ac30ce61a9f4c773cb4c3d2cb2e7f5123012dacae3b17c6f32904ecdeeefb
---

## 什么是 BitRouter？

BitRouter 是一个**开源的自适应 LLM 网关与路由器**，服务于你的生产智能体循环。它是一个本地二进制，为任意智能体提供统一端点，把它的模型调用、工具与子智能体路由到仍能达成目标的最佳路径——且**无需修改任何 harness**——并在每一次运行中持续收紧路由。**如今它以成本为优化目标**：把你的运行时指向它，每个循环的每一步默认都不再按前沿模型的价格计费。延迟与准确率目标即将到来。

它运行在你的智能体所在的任何地方，无需安装依赖，并作为一个无许可网络运行——任意提供商可注册，任意智能体可接入。**核心（Core）以 Apache 2.0 协议开源，可免费自托管**——自带密钥或运行本地模型即可，无需付费。**云（Cloud）**是可选的托管层，在其之上额外提供托管提供商、智能体自主支付与账户级策略——[一分钟内安装任一模式](/docs/get-started/configuration)。完整的[模型与定价](/docs/get-started/supported-models)目录见此。

## 三个原语，一个网关

一个智能体循环消耗三样东西。多数路由器只治理第一样——BitRouter 让这三样都可路由、可观测、可治理：

- **模型（Models）** — 跨提供商、跨账户、跨线路协议（OpenAI、Anthropic、Google）路由 LLM 调用。参见 [Models](/docs/concepts/models)。
- **能力（Capabilities）** — 一个 **MCP 网关**与一个 **AgentSkills 网关**：工具与技能成为受治理、可路由的资源，而非硬编码的端点。参见 [Tools](/docs/concepts/tools)。
- **智能体（Agents）** — 一个 **ACP 网关**：子智能体是一等可路由原语，因此一个任务可以交给最契合目标的子智能体，就像把一次调用路由到最契合的模型一样。参见 [Agents](/docs/concepts/agents)。

优化一个循环不只是选模型——而是选出最能服务于循环目标的模型、工具*与*子智能体。每个循环都有一份 [策略（policy）](/docs/concepts/policy)，BitRouter 依据实时信号，通过持续的 act → observe → evaluate → learn 循环对其调优，因此运行越久就越好。如今它优化的是成本；延迟与准确率即将到来。

## 为什么智能体运行在 BitRouter 上

四种机制，内建于路由器——而非逐个智能体地外挂：

- **可靠性** — 运行途中跨提供商重新路由、自动重试、模型/提供商回退，让长循环在故障与 `429` 中存活。失败的请求不计费。参见 [模型回退](/docs/features/model-fallback)。
- **可观测性** — 每个智能体、模型与步骤都被追踪，成本按**每次运行**归因，并通过 OTLP 导出到任意后端。参见 [OpenTelemetry](/docs/features/opentelemetry)。
- **安全** — 正则护栏与限速在路由器层为每个智能体统一执行一次，外加逐智能体的 [KYA](/docs/features/payment) 身份。参见 [护栏](/docs/features/guardrails)。
- **效率** — 自适应路由把每次调用、工具与智能体匹配到最契合目标的路径；如今这意味着琐碎调用不再按前沿价格计费，延迟与准确率即将到来。

## 下一步

BitRouter 是任何支持自定义 OpenAI 或 Anthropic base URL 的运行时的即插即用代理。[Configuration](/docs/get-started/configuration) 让你在一分钟内开始路由；各运行时的接入方法（Claude Code、OpenClaw、Codex 等）详见 [集成](/docs/integrations)，机器可读文档见 [AI 资源](/docs/ai-resources)。

想知道 BitRouter 与 OpenRouter、LiteLLM 等的对比，或该自托管还是用 Cloud？参见 [FAQ](/docs/get-started/faqs)。
