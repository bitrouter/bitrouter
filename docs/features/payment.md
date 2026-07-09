---
title: Agentic Payments
description: Let agents pay per request autonomously via the Machine Payment Protocol (x402/MPP) over Stripe, Tempo, or Solana — no cards, no prepaid credits.
sourceHash: 98dd1f115fd37d1ae47633f50c9f47de23dbd9fa7ca7acaff854e936c94629de
---

**Agentic payments** let an agent settle for each request on its own, without you
provisioning an API key or pre-funding an account for it. BitRouter Cloud speaks
the **Machine Payment Protocol (x402 / MPP)**, so a request can carry its own
payment over **Stripe**, **Tempo**, or **Solana** — no cards, no prepaid credits.

This is the agent-facing half of the gateway: where [namespaces](/docs/features/namespaces)
scope *what* an agent may do and [BYOK](/docs/features/byok) decides *whose key*
pays, agentic payments let the agent itself *pay per call* — useful when you want
a fleet of agents to transact without sharing a single provisioned credential.

> [!NOTE]
> Setup and rule-syntax documentation for x402 / MPP is in progress. If you're
> building against it today, reach out and we'll point you at the current API.
