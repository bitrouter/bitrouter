---
title: Self-Hosted & Cloud
description: Install the open-source BitRouter binary and run it self-hosted, or sign in to BitRouter Cloud — the same core either way. How to install both, and what Cloud adds on top.
---

BitRouter has two front doors, and both run the **same open-source core** (Apache 2.0). Self-host the binary with your own keys, or sign in to BitRouter Cloud — the routing engine is identical. This page shows how to install and run both, then breaks down what a Cloud account adds so you can pick a starting point.

## Install the binary

Install the open-source binary:

<Tabs items={['macOS / Linux', 'Homebrew', 'npm', 'cargo']}>
<Tab value="macOS / Linux">

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/bitrouter/bitrouter/releases/latest/download/bitrouter-installer.sh | sh
```

</Tab>
<Tab value="Homebrew">

```bash
brew install bitrouter/tap/bitrouter
```

</Tab>
<Tab value="npm">

```bash
npm install -g bitrouter
```

</Tab>
<Tab value="cargo">

```bash
cargo install bitrouter
```

</Tab>
</Tabs>

## Run self-hosted

Set your provider keys in the environment and start the proxy:

```bash
export OPENAI_API_KEY=sk-...    # ANTHROPIC_API_KEY / GEMINI_API_KEY also work
bitrouter start
# Proxy running at http://127.0.0.1:4356
```

BitRouter auto-detects any key set in the environment — no config file needed. Any provider whose key is present is immediately available. See [BYOK](/docs/features/byok) for the full list of recognized variables, or [local & private models](/docs/integrations/models) to point BitRouter at Ollama, vLLM, or LM Studio for free.

For advanced routing rules, guardrails, or multi-account failover, scaffold a config file:

```bash
bitrouter init          # writes ./bitrouter.yaml (override with `-c <path>`)
bitrouter start
```

## Use BitRouter Cloud

Sign in to a BitRouter Cloud account from the terminal — one account covers every model the hosted network offers, with no upstream provider keys required:

```bash
bitrouter cloud login   # RFC 8628 device flow against api.bitrouter.ai
bitrouter start         # the `bitrouter` provider auto-enables once signed in
```

You can also point an agent straight at the hosted endpoint without running a local binary. Either way the core is the same — a Cloud account is an account and network, not a separate deployment. See the [Models](/docs/get-started/models) catalog for pricing.

## Point your agent at the proxy

However you start it, BitRouter is a drop-in proxy. Point your agent runtime at the proxy base URL — `http://127.0.0.1:4356` when self-hosting — and every model call routes through BitRouter with no harness changes.

## The core is identical either way

Every routing, fallback, model-variant, BYOK, local-model, guardrail, observability, MCP, ACP, and structured-output capability works the same whether you self-host the binary or attach a Cloud account to it. Cloud adds what needs a server _you_ don't run — it does not replace or restrict the core.

### Capability comparison

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

Cloud's managed provider network means you make model requests without setting up upstream accounts or storing API keys. Today this is [Managed Models](/docs/get-started/models) — one account, requests billed per token, open models served at a discount below official pricing. Managed Tools and Agents are on the roadmap.

### Team workspaces

A Cloud account gives you workspaces: isolated environments with their own API keys, routing policies, usage data, and access controls. Each member gets a seat scoped to specific workspaces. Credential scoping is strict — a workspace-baked key cannot reach other workspaces or manage billing. See [Cloud Workspaces](/docs/features/namespaces) for the full model.

For the OSS namespace isolation primitive (available in both self-hosted and Cloud), see [Namespaces](/docs/features/namespaces).

### Hosted observability

The Cloud console surfaces per-workspace request history, spend, and usage breakdowns without you running any infrastructure. The self-hosted binary exports the same data via OTLP if you run your own observability stack.

### Billing and SLA

Cloud provides managed billing (one wallet, per-request, failed requests not charged) and an uptime SLA on the hosted endpoint. Self-hosted has no software licensing cost and no SLA commitment on your own infrastructure.

## Attaching Cloud to a self-hosted binary

Cloud is not a different binary — it's an account you attach:

```bash
bitrouter cloud login
# Opens a browser to sign in and pick a workspace.
# Your local binary now routes Cloud-managed models alongside your BYOK keys.
```

You can add or remove the Cloud account at any time. The binary's self-hosted capabilities are unaffected either way.

## Next steps

<Cards>
  <Card title="Quick Start" href="/docs/get-started/quickstart" description="Get an agent routing through BitRouter in under a minute" />
  <Card title="Models" href="/docs/get-started/models" description="The full catalog, pricing, and open-model discounts" />
</Cards>
