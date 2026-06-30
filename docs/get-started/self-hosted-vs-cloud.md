---
title: Self-Hosted vs Cloud
description: Understand what running BitRouter self-hosted gives you out of the box, and what BitRouter Cloud adds on top — so you can choose the right starting point.
sourceHash: 285427fef893d159acdf9770fd34cc0f2d3a7543b7951105c987fabbcbba5c67
---

BitRouter ships as a single open-source binary under Apache 2.0. You can run it entirely self-hosted with your own provider keys and never pay for the software itself. **BitRouter Cloud** is an optional hosted layer you attach to that same binary when you want managed infrastructure, team features, or a provider network you don't have to wire up yourself.

This page is the one place in get-started where Cloud is framed against the self-hosted option. Read it once, decide, then move on.

## The core is identical either way

Every routing, fallback, model-variant, BYOK, local-model, guardrail, observability, MCP, ACP, and structured-output capability works the same whether you self-host the binary or attach a Cloud account to it. Cloud adds what needs a server _you_ don't run — it does not replace or restrict the core.

## Capability comparison

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

## What each option is best for

**Self-hosted** is the right default if you:

- Already have provider API keys and want full control over where your traffic goes.
- Are running local or private models that never leave your network.
- Have compliance or data-residency requirements that prevent traffic from leaving your infrastructure.
- Are prototyping alone and don't need team access controls yet.

**Cloud** makes sense when you:

- Want a single account and no upstream key management — one bill, billed per request, failed requests not charged.
- Need open models served at a discount without setting up your own provider accounts.
- Are working with a team and need per-workspace isolation, seat management, and a hosted console.
- Want an uptime SLA and priority support for production workloads.

## What Cloud adds in detail

### Managed provider network

Cloud's managed provider network means you make model requests without setting up upstream accounts or storing API keys. Today this is [Managed Models](/docs/get-started/models-and-providers) — one account, requests billed per token, open models served at a discount below official pricing. Managed Tools and Agents are on the roadmap.

### Team workspaces

A Cloud account gives you workspaces: isolated environments with their own API keys, routing policies, usage data, and access controls. Each member gets a seat scoped to specific workspaces. Credential scoping is strict — a workspace-baked key cannot reach other workspaces or manage billing. See [Cloud Workspaces](/docs/features/namespaces) for the full model.

For the OSS namespace isolation primitive (available in both self-hosted and Cloud), see [Namespaces](/docs/features/namespaces).

### Hosted observability

The Cloud console surfaces per-workspace request history, spend, and usage breakdowns without you running any infrastructure. The self-hosted binary exports the same data via OTLP if you run your own observability stack.

### Billing and SLA

Cloud provides managed billing (one wallet, per-request, failed requests not charged) and an uptime SLA on the hosted endpoint. Self-hosted has no software licensing cost and no SLA commitment from Anthropic on your own infrastructure.

## Attaching Cloud to a self-hosted binary

Cloud is not a different binary — it's an account you attach:

```bash
bitrouter auth login
# Opens a browser to sign in and pick a workspace.
# Your local binary now routes Cloud-managed models alongside your BYOK keys.
```

You can add or remove the Cloud account at any time. The binary's self-hosted capabilities are unaffected either way.

## Next steps

- **Using Cloud managed models** — see [Managed Models](/docs/get-started/models-and-providers) for the model catalog, pricing, and how to make your first managed request.
- **Namespaces (OSS isolation)** — see [Namespaces](/docs/features/namespaces) for the self-hosted isolation primitive.
- **Team workspaces** — see [Cloud Workspaces](/docs/features/namespaces) for seats, credential scoping, and per-workspace policies.
- **Full Cloud overview** — see [Cloud Overview](/docs/get-started/self-hosted-vs-cloud) for the complete hosted layer reference.
