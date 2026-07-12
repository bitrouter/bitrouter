---
title: 智能体（Agents）
description: BitRouter 的 agent-native 含义——ACP 网关负责身份、发现与任务派发，KYA 身份让自主 agent 按次付费。
sourceHash: fc31b2f87105a5965a9c5873d86ccca9e30838d57f686649231ce9c427cea858
---

BitRouter 是 **agent-native（以智能体为中心）**的：下面这些原语都假定调用方是自主 agent，而非键盘前的人。这体现在两处——agent 如何被识别与触达，以及如何付费。

## ACP 网关 —— 身份与派发

正如 MCP 网关让 agent 触达众多工具服务器，**ACP 网关**负责 agent 这一侧：跨 host 的 **agent 身份、发现与任务派发**。它让 agent 在网络中拥有一席之地、可被发现、并能交接或接收任务——采用 BitRouter 处处一致的单端点模型。

## KYA —— 可验证、能付费的身份

一个持有你密钥的自主 agent，若没有自身的身份，便是一种隐患。**KYA（Know-Your-Agent）**赋予 agent 一个**可验证的身份**，这正是自主付费得以安全的前提：凭借该身份，agent 可以通过机器支付协议（x402/MPP）**按次付费**，自行结算每个请求，全程无需信用卡、预付额度或发票。

## 了解如何使用

- [智能体支付](/docs/features/payment) —— 通过 MPP / x402 实现自主按次付费。
