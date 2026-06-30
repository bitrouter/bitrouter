---
title: 介绍
description: 面向 LLM Agent 的开放智能路由器
sourceHash: d98ac30ce61a9f4c773cb4c3d2cb2e7f5123012dacae3b17c6f32904ecdeeefb
---

## 什么是 BitRouter？

BitRouter 是**面向 LLM Agent 的开放智能路由器**——一个开源、可本地部署、无前端的 Rust 二进制，为任意 Agent 提供统一端点，用以发现、路由并支付任意 LLM、工具或子 Agent。原生为自主 Agent 循环而生，具备一流的 CLI 运行时控制、可靠性、可观测性与防护；并作为一个无许可的市场与路由器运行——任意提供商可注册，任意消费者可使用。

[核心（Core）](/docs/get-started/installation)**以 Apache 2.0 协议开源，可免费自托管**——自带密钥或运行本地模型即可，无需付费。[云（Cloud）](/docs/get-started/self-hosted-vs-cloud)是可选的托管层，在其之上额外提供托管提供商、Agent 自主支付与账户级防护策略管理。完整的[模型与定价](/docs/get-started/models-and-providers)目录见此。

## 功能

- **通用 LLM API** — 单一二进制，四套协议：OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、Google Generative AI。以你偏好的协议访问任意 LLM。
- **面向长时运行 Agent 的可靠性** — 自动重试、模型与提供商回退、连接复用、请求级幂等性，专为运行数小时乃至数天的 Agent 循环设计。
- **免费 BYOK** — 自带提供商密钥，零费用使用。BitRouter 自动从环境变量探测密钥——无需配置文件。你也可以将 BitRouter 指向自己的本地模型（Ollama、vLLM、LM Studio），完全免费——详见[本地与私有模型](/docs/integrations/models)。
- **MCP & ACP 网关** — 代理 [MCP](https://modelcontextprotocol.io) 服务器，让 Agent 跨主机发现与调用工具。支持 [ACP](https://github.com/zed-industries/acp)，实现 Agent 身份、发现与任务分派。
- **运行时可观测性** — 原生 OpenTelemetry：trace 与指标通过 OTLP 导出到你运行的任意后端，并可用 `bitrouter observe status` 查看导出器的实时状态。
- **运行时防护** — 用正则规则在代理层对风险内容脱敏或拦截。无需任何应用层改动。
- **智能路由** — 多提供商路由，按成本与性能优化。跨协议路由（OpenAI ↔ Anthropic）、可编程回退、策略驱动的升级。
- **Agent 原生认证与支付** — KYA（Know-Your-Agent）身份与 x402/MPP 按使用付费的托管服务。Agent 自主认证与付款——无需信用卡、预充值或发票。
- **开放生态** — 无许可的[提供商注册](/docs/guides/register-as-a-provider)。任何暴露 OpenAI 或 Anthropic 兼容端点的提供商，都可以加入网络并被网络上的 Agent 发现。

## 愿景

为自主 Agent 构建一层开放、无许可的智能层——任何 Agent 都能在没有守门人的前提下，发现、支付并调用任何模型、工具或子 Agent。BitRouter 是其中的连接组织：开源、Agent 原生、由运营者掌控。

## Agent 运行时

BitRouter 是任何支持自定义 OpenAI 或 Anthropic base URL 的运行时的即插即用代理——将其指向 `http://127.0.0.1:4356` 即可。

OpenClaw、Claude Code、ZeroClaw、Codex CLI、OpenCode 等的接入方法详见[集成](/docs/integrations)。

机器可读文档与即插即用 Agent 技能，详见 [AI 资源](/docs/ai-resources)。
