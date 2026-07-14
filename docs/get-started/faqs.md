---
title: FAQs
description: Common questions about running BitRouter — self-host vs Cloud, what Cloud adds, whether you need provider keys, and how BitRouter compares to OpenRouter, LiteLLM, Portkey, Bifrost, and TensorZero.
---

## Deployment

### Do I need my own provider keys?

Only if you self-host with BYOK — you bring keys as environment variables and pay each provider directly. A [BitRouter Cloud](/docs/get-started/configuration) account needs **no upstream keys**: one sign-in covers the whole hosted network, billed per request, failed requests not charged. You can also attach Cloud to a self-hosted binary and use both.

### Is Cloud a separate deployment or a different binary?

Neither — Cloud is **an account you attach**, not a fork. Both front doors run the same open-source core (Apache 2.0) with an identical routing engine. `bitrouter cloud login` enables the `bitrouter` managed provider alongside your BYOK keys; remove it any time. See [Configuration](/docs/get-started/configuration#attach-cloud-to-a-self-hosted-binary).

### Self-host or Cloud?

Every core capability works the same either way — Cloud only adds what needs a server you don't run.

- **Self-host** if you already have provider keys, run local/private models, have data-residency rules, or are prototyping solo.
- **Cloud** if you want no key management and per-request billing, discounted open models without provider signups, team workspaces, or an uptime SLA.

### What does Cloud add on top of the open-source core?

| Capability | Self-hosted (OSS) | Cloud |
| --- | --- | --- |
| Universal API + cross-protocol routing | ✅ | ✅ |
| BYOK (bring your own provider keys) | ✅ | ✅ |
| Local / private model serving | ✅ | ✅ |
| Model fallback & provider selection | ✅ | ✅ |
| Model variants & presets | ✅ | ✅ |
| Guardrails | ✅ | ✅ |
| Observability (OTLP trace + metric export) | ✅ | ✅ |
| MCP & ACP gateways | ✅ | ✅ |
| Structured outputs | ✅ | ✅ |
| Namespace isolation primitive | ✅ | ✅ |
| Managed provider network (no upstream keys needed) | — | ✅ |
| Open-model pricing discounts | — | ✅ |
| Team seats & per-workspace access control | — | ✅ |
| Hosted observability console | — | ✅ |
| Managed billing (one wallet, per-request) | — | ✅ |
| SLA on the hosted endpoint | — | ✅ |
| Priority support | — | ✅ |
| Agentic payment marketplace | — | ✅ |

In short, Cloud adds a managed provider network ([Managed Models](/docs/get-started/supported-models) — no upstream keys, open models discounted), team [workspaces](/docs/features/namespaces) with strictly scoped keys, a hosted observability console, managed per-request billing, and an uptime SLA. Everything else is in both.

## Comparison

BitRouter and the gateways below all route LLM traffic. The difference is *what* they route and *what* they optimize — BitRouter is the only one that treats **models, tools, and agents as a single routable surface** and optimizes the whole production **loop** (cost today, multi-objective by design).

|  | **BitRouter** | **OpenRouter** | **LiteLLM** | **TensorZero** | **Portkey** | **Bifrost** |
| --- | --- | --- | --- | --- | --- | --- |
| **Routable primitives** | Models + tools + **agents** (MCP + ACP) | Models | Models + tools (MCP) | Models | Models + tools (MCP) | Models + tools (MCP) |
| **Optimizes** | The **loop**, multi-objective (cost today) | Static routing | Static routing | The model | Static routing | Static routing |

_All but OpenRouter are open-source and self-hostable; BitRouter and TensorZero are Rust._

### How is BitRouter different from OpenRouter?

OpenRouter is a closed-source, cloud-only marketplace for humans picking models. BitRouter is Apache-2.0 and self-hostable, permissionless (x402/Solana pay-per-request — no KYC or geo-restrictions), agent-first (firewall, MCP/ACP gateway, skills registry), and lower-latency (sub-10ms vs ~25–40ms).

### How is BitRouter different from LiteLLM?

LiteLLM is an open-source Python proxy that's BYOK-only and infra-heavy (Postgres/Redis/K8s in production). BitRouter is one binary with no dependencies, Rust-native (flat tail latency where Python hits the GIL), supports autonomous agent payments, and adds in-line safety, KYA identity, and a skills registry.

### How is BitRouter different from Portkey, Bifrost, and other generic gateways?

Generic gateways treat LLMs as just another upstream API — logging, caching, rate limiting, failover, BYOK billing. They lack agent identity and runtime model discovery, autonomous payment protocols (x402/MPP), MCP/ACP gateways, a skills registry, and sub-10ms native deployment — the agent surface BitRouter is built around.

### How is BitRouter different from TensorZero?

TensorZero (Rust) optimizes **the model itself** — prompts, weights, selection. BitRouter optimizes **the whole loop** — across models, tools, and sub-agents (MCP + ACP) — cost today, with latency and accuracy next.
