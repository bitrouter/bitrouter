---
title: FAQs
description: 关于运行 BitRouter 的常见问题——自托管 vs Cloud、Cloud 的增量、是否需要供应商密钥，以及 BitRouter 与 OpenRouter、LiteLLM、Portkey、Bifrost、TensorZero 的对比。
---

## 部署

### 我需要自己的供应商密钥吗？

只有在以 BYOK 方式自托管时才需要——你把密钥设为环境变量，并按各供应商官方价直接向其付费。使用 [BitRouter Cloud](/docs/get-started/configuration) 账户则**无需上游密钥**：一次登录即可覆盖整个托管网络，按请求计费，失败请求不收费。你也可以把 Cloud 附加到自托管二进制上，两者同时使用。

### Cloud 是单独的部署或另一个二进制吗？

都不是——Cloud 是**你附加的一个账户**，而非一个分支。两扇入口都运行同一套开源核心（Apache 2.0），路由引擎完全相同。`bitrouter cloud login` 在 BYOK 密钥之外启用 `bitrouter` 托管供应商；可随时移除。参见 [Configuration](/docs/get-started/configuration#attach-cloud-to-a-self-hosted-binary)。

### 自托管还是 Cloud？

所有核心能力两种方式都完全一致——Cloud 只补充那些需要你不用自己运维的服务器才能提供的功能。

- **自托管**：你已有供应商密钥、运行本地/私有模型、有数据驻留要求，或独立做原型验证。
- **Cloud**：你想要无需管理密钥且按请求计费、无需注册供应商即可用折扣开放模型、需要团队工作区，或需要可用性 SLA。

### Cloud 在开源核心之上额外提供了什么？

| 能力 | 自托管（OSS） | 云 |
| --- | --- | --- |
| 通用 API + 跨协议路由 | ✅ | ✅ |
| BYOK（自带供应商密钥） | ✅ | ✅ |
| 本地 / 私有模型部署 | ✅ | ✅ |
| 模型回退与供应商选择 | ✅ | ✅ |
| 模型变体与预设 | ✅ | ✅ |
| 防护（Guardrails） | ✅ | ✅ |
| 可观测性（OTLP 追踪 + 指标导出） | ✅ | ✅ |
| MCP 与 ACP 网关 | ✅ | ✅ |
| 结构化输出 | ✅ | ✅ |
| 命名空间隔离原语 | 通过兼容的管理部署 | ✅ |
| 托管供应商网络（无需上游密钥） | — | ✅ |
| 开放模型价格折扣 | — | ✅ |
| 团队席位与工作区级访问控制 | — | ✅ |
| 托管可观测性控制台 | — | ✅ |
| 托管计费（统一钱包，按请求计费） | — | ✅ |
| 托管端点 SLA | — | ✅ |
| 优先支持 | — | ✅ |
| Agent 自主支付市场 | — | ✅ |

简而言之，Cloud 额外提供：托管供应商网络（[托管模型](/docs/get-started/supported-models)——无需上游密钥，开放模型享折扣）、带严格作用域密钥的团队[工作区](/docs/features/namespaces)、托管可观测性控制台、托管的按请求计费，以及可用性 SLA。其余能力两种模式皆有。

具体到变体与预设：自托管 OSS 支持在 `bitrouter.yaml` 中定义 `variants` 与 `presets`；Cloud 额外提供内置的托管路由 profile，以及按命名空间管理的路由预设。

## 对比

BitRouter 与下面这些网关都路由 LLM 流量。差别在于它们*路由什么*、*优化什么*——BitRouter 是唯一一个把**模型、工具与智能体视为单一可路由面**、并优化整个生产**循环**（如今按成本，设计上支持多目标）的方案。

|  | **BitRouter** | **OpenRouter** | **LiteLLM** | **TensorZero** | **Portkey** | **Bifrost** |
| --- | --- | --- | --- | --- | --- | --- |
| **可路由的原语** | 模型 + 工具 + **智能体**（MCP + ACP） | 模型 | 模型 + 工具（MCP） | 模型 | 模型 + 工具（MCP） | 模型 + 工具（MCP） |
| **优化对象** | **循环**，多目标（如今按成本） | 静态路由 | 静态路由 | 模型本身 | 静态路由 | 静态路由 |

_除 OpenRouter 外均为开源且可自托管；BitRouter 与 TensorZero 采用 Rust。_

### BitRouter 与 OpenRouter 有何不同？

OpenRouter 是面向人工选模型的闭源、仅云端的市场。BitRouter 采用 Apache 2.0 且可自托管、无许可（x402/Solana 按请求付费——无 KYC、无地域限制）、Agent 优先（防火墙、MCP/ACP 网关、技能注册中心），且延迟更低（低于 10ms，对比约 25–40ms）。

### BitRouter 与 LiteLLM 有何不同？

LiteLLM 是仅 BYOK、对基础设施依赖较重的开源 Python 代理（生产环境需 Postgres/Redis/K8s）。BitRouter 仅一个二进制、无任何依赖、Rust 原生（Python 会遇到 GIL 而它尾延迟稳定），支持自主 Agent 支付，并增加了内联安全、KYA 身份和技能注册中心。

### BitRouter 与 Portkey、Bifrost 及其他通用网关有何不同？

通用网关把 LLM 视作普通的上游 API——日志、缓存、限流、故障转移、BYOK 计费。它们缺少 Agent 身份与运行时模型发现、自主支付协议（x402/MPP）、MCP/ACP 网关、技能注册中心，以及亚 10ms 的原生部署——正是 BitRouter 围绕构建的 Agent 接入面。

### BitRouter 与 TensorZero 有何不同？

TensorZero（Rust）优化的是**模型本身**——提示词、权重、模型选择。BitRouter 优化的是**整个循环**——跨越模型、工具与子 Agent（MCP + ACP）——如今按成本，延迟与准确率即将到来。
