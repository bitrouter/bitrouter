---
title: 对比
description: BitRouter 与 OpenRouter、LiteLLM 及其他 LLM API 网关的差异。
sourceHash: 4347981e13132d55c473e289cb3a9783141fa87545d4923b5f4dace5a50222a6
---

BitRouter 是唯一**专为 Agent 而生的智能路由器与开放市场**——开放（Apache 2.0、可本地部署、提供商注册与消费者使用均无许可）、智能（多提供商路由、可编程回退、运行时防护，路由开销低于 10ms）、Agent 原生（KYA 身份、自主 x402/MPP 支付、MCP/ACP 网关，以及为长时运行循环设计的可靠性原语）。

现有方案大致分为三类，没有任何一类同时覆盖以上三个属性。

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
