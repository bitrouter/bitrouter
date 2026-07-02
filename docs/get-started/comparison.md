---
title: Comparison
description: How BitRouter differs from OpenRouter, LiteLLM, TensorZero, Portkey, and Bifrost — the only router that cost-optimizes the whole agent loop.
sourceHash: 4347981e13132d55c473e289cb3a9783141fa87545d4923b5f4dace5a50222a6
---

BitRouter and the gateways below all route LLM traffic. The difference is *what* they route and *what* they optimize. BitRouter is the only one that treats **models, tools, and agents as a single routable surface** and optimizes the whole production **loop** by cost — open-source, self-hostable, and Rust-native.

|  | **BitRouter** | **OpenRouter** | **LiteLLM** | **TensorZero** | **Portkey** | **Bifrost** |
| --- | --- | --- | --- | --- | --- | --- |
| **Best for** | Cost-optimizing agent loops | Model marketplace | Unifying provider SDKs | Model optimization | Fast unified gateway | Fast unified gateway |
| **Routable primitives** | Models + tools + **agents** (MCP + ACP) | Models | Models + tools (MCP) | Models | Models + tools (MCP) | Models + tools (MCP) |
| **Optimizes** | The **loop**, by cost | Static routing | Static routing | The model | Static routing | Static routing |
| **Model catalog** | Curated + bring any provider | **1,600+ marketplace** | Any provider | Curated | **1,600+** | 23+ providers |

_All but OpenRouter are open-source and self-hostable; BitRouter and TensorZero are Rust. TensorZero is no longer maintained._

**TL;DR** — OpenRouter is a cloud API marketplace for humans picking models. LiteLLM (Python), Portkey (TypeScript), and Bifrost (Go) are unified gateways — fast, OpenAI-compatible, guardrails included — but they route models. TensorZero (Rust) adds a production feedback loop, but optimizes the model itself, not the loop. BitRouter is the only one that treats models, tools, and agents as a single routable surface — a Rust-native gateway that cost-optimizes the whole production loop, with cross-protocol routing, MCP and ACP gateways, and guardrails out of the box.

The rest of this page breaks the field into three categories. None covers everything an agent loop needs at once.

## vs Cloud SaaS routers (OpenRouter and similar)

Cloud SaaS routers — like **OpenRouter** — route requests across hundreds of models behind a hosted endpoint, optimized for human-facing apps.

- **Self-hostable** — Cloud SaaS routers are closed-source and cloud-only. BitRouter is Apache 2.0; run it anywhere as a single binary.
- **Permissionless access** — These services require account creation and credit card or crypto top-up. BitRouter's hosted option uses x402/Solana — no KYC, no geo-restrictions, agents pay per request.
- **Agent-first features** — Cloud SaaS routers have no agent firewall, no MCP/ACP gateway, and no skills registry. BitRouter is built around them.
- **Lower latency** — Sub-10ms routing overhead vs ~25–40ms typical for hosted routers.

## vs Self-hosted proxies (LiteLLM and similar)

Self-hosted proxies — like **LiteLLM** — are open-source SDKs and Python proxies popular for backend services. They're BYOK and infra-heavy.

- **Zero-ops** — These proxies typically require Postgres, Redis, and Docker/K8s in production. BitRouter is one binary, no dependencies.
- **Performance** — Python-based proxies hit the GIL at scale; tail latency degrades under concurrent load. BitRouter's Rust async runtime keeps latency flat.
- **Payments** — Self-hosted proxies are BYOK only and have no payment handling. BitRouter's hosted option supports autonomous agent payments.
- **Agent runtime** — These proxies have no in-line content safety, no KYA identity, no skills registry. BitRouter does.

## vs Generic API gateways (Portkey, Kong AI, AWS Bedrock Gateway, etc.)

Generic API gateways treat LLMs as just another upstream API. They typically offer logging, caching, rate limiting, provider failover, BYOK billing, and dashboards.

They don't offer:

- Agent identity or runtime model discovery
- Autonomous payment protocols (x402/MPP)
- MCP or ACP gateway functionality
- A skills registry for agent capabilities
- Sub-10ms native binary deployment

These gateways fit traditional API operations. BitRouter exists because **autonomous agents need a different surface** — runtime model selection, payment delegation, in-line safety, and a single open standard for tool and sub-agent discovery.
