---
title: Agentic Payments
description: 让 agent 通过机器支付协议（x402/MPP）经由 Stripe、Tempo 或 Solana 自主按请求付款——无需绑卡，无需预充值。
sourceHash: 98dd1f115fd37d1ae47633f50c9f47de23dbd9fa7ca7acaff854e936c94629de
---

**Agentic payments（自主支付）** 让 agent 为每个请求自行结算，你无需为它预置 API
密钥或预充值账户。BitRouter Cloud 支持**机器支付协议（x402 / MPP）**，请求可以自带
支付凭证，经由 **Stripe**、**Tempo** 或 **Solana** 完成——无需绑卡，无需预充值。

这是网关面向 agent 的一半：[命名空间](/docs/features/namespaces)界定 agent *能做什么*，
[BYOK](/docs/features/byok)决定*用谁的密钥*付费，而自主支付让 agent *按调用自行付款*
——当你希望一批 agent 在不共享同一套预置凭证的情况下完成交易时尤其有用。

> [!NOTE]
> x402 / MPP 的配置与规则语法文档正在编写中。如果你现在就要基于它开发，请联系我们，
> 我们会为你指引当前的 API。
