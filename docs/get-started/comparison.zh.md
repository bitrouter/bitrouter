---
title: Comparison
description: BitRouter 与 OpenRouter、LiteLLM、TensorZero、Portkey、Bifrost 的差异——唯一为整个智能体循环做成本优化的路由器。
sourceHash: 4347981e13132d55c473e289cb3a9783141fa87545d4923b5f4dace5a50222a6
---

BitRouter 与下面这些网关都路由 LLM 流量。差别在于它们*路由什么*、*优化什么*。BitRouter 是唯一一个把**模型、工具与智能体视为单一可路由面**、并按成本优化整个生产**循环**的方案——开源、可自托管、且以 Rust 原生。

|  | **BitRouter** | **OpenRouter** | **LiteLLM** | **TensorZero** | **Portkey** | **Bifrost** |
| --- | --- | --- | --- | --- | --- | --- |
| **最适合** | 为智能体循环做成本优化 | 模型市场 | 统一各家提供商 SDK | 模型优化 | 快速统一网关 | 快速统一网关 |
| **可路由的原语** | 模型 + 工具 + **智能体**（MCP + ACP） | 模型 | 模型 + 工具（MCP） | 模型 | 模型 + 工具（MCP） | 模型 + 工具（MCP） |
| **优化对象** | **循环**，按成本 | 静态路由 | 静态路由 | 模型本身 | 静态路由 | 静态路由 |
| **模型目录** | 精选 + 可接入任意提供商 | **1,600+ 市场** | 任意提供商 | 精选 | **1,600+** | 23+ 提供商 |

_除 OpenRouter 外均为开源且可自托管；BitRouter 与 TensorZero 采用 Rust。TensorZero 已停止维护。_

**TL;DR** —— OpenRouter 是面向人工选模型的云端 API 市场。LiteLLM（Python）、Portkey（TypeScript）与 Bifrost（Go）是统一网关——快速、OpenAI 兼容、自带护栏——但它们路由的是模型。TensorZero（Rust）增加了一个生产反馈循环，但优化的是模型本身，而非循环。BitRouter 是唯一一个把模型、工具与智能体视为单一可路由面的方案——一个以 Rust 原生、为整个生产循环做成本优化的网关，开箱即带跨协议路由、MCP 与 ACP 网关以及护栏。

本页余下部分把整个领域分为三类。没有任何一类能同时覆盖一个智能体循环所需的全部。

## vs 云端 SaaS 路由器（OpenRouter 等同类产品）

云端 SaaS 路由器——以 **OpenRouter** 为代表——通过托管端点跨数百个模型路由请求，主要面向人工交互应用。

- **可自托管** — 云端 SaaS 路由器闭源、仅云端；BitRouter 采用 Apache 2.0，可作为单一二进制在任意环境运行。
- **无许可访问** — 这类服务需创建账户并使用信用卡或加密货币充值；BitRouter 托管服务使用 x402/Solana，无 KYC、无地域限制，Agent 按请求付费。
- **Agent 优先功能** — 云端 SaaS 路由器没有 Agent 防火墙、MCP/ACP 网关或技能注册中心；BitRouter 围绕这些功能构建。
- **更低延迟** — 路由开销低于 10ms，托管路由器通常约 25–40ms。

## vs 自托管代理（LiteLLM 等同类产品）

自托管代理——以 **LiteLLM** 为代表——是常用于后端服务的开源 SDK 与 Python 代理。BYOK 模式，对基础设施依赖较重。

- **零运维** — 这类代理生产环境通常需要 Postgres、Redis 和 Docker/K8s；BitRouter 仅需一个二进制文件，无任何依赖。
- **性能** — 基于 Python 的代理在大规模并发时受 GIL 限制，尾延迟会下降；BitRouter 的 Rust 异步运行时延迟稳定。
- **支付** — 自托管代理仅支持 BYOK，不处理支付；BitRouter 托管服务支持自主 Agent 支付。
- **Agent 运行时** — 这类代理没有内联内容安全、KYA 身份或技能注册中心；BitRouter 都有。

## vs 通用 API 网关（Portkey、Kong AI、AWS Bedrock Gateway 等）

通用 API 网关把 LLM 视作普通的上游 API，通常提供日志、缓存、限流、提供商故障转移、BYOK 计费和管理面板。

它们通常不提供：

- Agent 身份或运行时模型发现
- 自主支付协议（x402/MPP）
- MCP 或 ACP 网关功能
- Agent 能力的技能注册中心
- 亚 10ms 的原生二进制部署

这类网关适合传统的 API 运维场景。BitRouter 的存在是因为**自主 Agent 需要不同的接入面**——运行时模型选择、支付委托、内联安全，以及一套用于工具和子 Agent 发现的开放标准。
