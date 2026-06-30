---
title: Migrate from TensorZero
description: Move a TensorZero gateway setup to BitRouter — drop the ClickHouse-backed LLMOps stack for a single agent-native binary, local or hosted.
sourceHash: 0b6c985670a25c13b4f0d958728639e8b06480832b28005456e89d61a7a85e53
---

# Migrating from TensorZero to BitRouter

[TensorZero](https://github.com/tensorzero/tensorzero) is an open-source **LLMOps platform** — a Rust gateway bundled with observability, evaluations, optimization (fine-tuning, automated prompt engineering), and experimentation, all backed by a ClickHouse data warehouse and driven by a GitOps `tensorzero.toml`. BitRouter solves a narrower problem on purpose: an **agent-native proxy** with a single OpenAI-compatible surface, runnable as a local binary (BYOK, no infra) or as a hosted endpoint with autonomous agent payments.

This guide is for teams who adopted TensorZero mainly as a **gateway** — unified provider access, routing, retries, fallback — and want a leaner, agent-first surface without operating a database. If you lean on TensorZero's optimization and experimentation loop, read [What BitRouter intentionally doesn't ship](#what-bitrouter-intentionally-doesnt-ship) before you commit.

## Why migrate?

| | TensorZero | BitRouter |
|---|---|---|
| **Runtime** | Rust gateway | Rust (single static binary) |
| **Production deps** | ClickHouse (observability store) + Docker/Compose | None |
| **Configuration** | `tensorzero.toml` — functions, variants, models, providers | Provider keys + routing presets; no schema to maintain |
| **Deployment modes** | Self-hosted only | Local binary **or** hosted (`api.bitrouter.ai`) — same OpenAI-compatible endpoint |
| **Design focus** | Full LLMOps loop: gateway + observability + evals + optimization + experimentation | Agent-first proxy: MCP / ACP / Skills, agent firewall, agentic payment |
| **Agent protocol surface** | None (gateway is provider-facing) | MCP, ACP, Skills, CLI — the product, not an add-on |
| **Agentic auth & payment** | None | x402 / MPP autonomous payment (cloud mode) |
| **License** | Apache 2.0 | Apache 2.0 |

## Two things worth highlighting

### 1. A proxy, not a platform

TensorZero is built around a feedback loop: route inference, store every call and its metrics in ClickHouse, then evaluate, fine-tune, and A/B test against that history. That's powerful if you run that loop — and it's why TensorZero needs a database and a typed config of functions and variants.

BitRouter deliberately stops at the gateway. There's no `tensorzero.toml` to maintain, no ClickHouse to operate — you set provider keys and route by `provider/model` id. [OpenTelemetry](/docs/features/opentelemetry) gives you spans and metrics for every request (the same OTLP export TensorZero offers), but they go to a backend you already run, not a warehouse the proxy manages.

### 2. Agent-native, and cloud/local share one surface

TensorZero's gateway is provider-facing — it unifies the upstreams. BitRouter adds the **agent-facing** half: an [MCP gateway](/docs/concepts/tools) for tools, an [ACP gateway](/docs/concepts/agents) for agent identity and dispatch, a [server-tool loop](/docs/features/server-tools), and [agentic payment](/docs/features/payment) so an agent can pay per request without you provisioning a key for it.

And **the hosted cloud and local binary expose the same OpenAI-compatible endpoint** — start local during development, point at `api.bitrouter.ai` for production (or vice versa) without changing client code. See the [Quick Start](/docs/get-started/quickstart) for both flows.

## Migration paths

### From an OpenAI-compatible client

If your app already talks to TensorZero's OpenAI-compatible endpoint, migration is a base-URL swap plus dropping the `tensorzero::…` model prefix for a plain `provider/model` id:

<Tabs items={['Before (TensorZero)', 'After (BitRouter)']}>
<Tab value="Before (TensorZero)">
```python
import openai

client = openai.OpenAI(
    base_url="http://localhost:3000/openai/v1",
    api_key="not-used",
)

response = client.chat.completions.create(
    # TensorZero model / function reference
    model="tensorzero::model_name::openai::gpt-4o-mini",
    messages=[{"role": "user", "content": "Hello"}],
)
```
</Tab>
<Tab value="After (BitRouter)">
```python
import openai

# Local: `bitrouter` (BYOK via env vars) — see /docs/get-started/quickstart
# Cloud: base_url="https://api.bitrouter.ai/v1", api_key=$BITROUTER_API_KEY
client = openai.OpenAI(
    base_url="http://127.0.0.1:4356/v1",
    api_key="not-used-in-local-byok",
)

response = client.chat.completions.create(
    model="openai/gpt-4o-mini",
    messages=[{"role": "user", "content": "Hello"}],
)
```
</Tab>
</Tabs>

A TensorZero **function** (a named prompt template + schema with one or more variants) has no single equivalent — the routing half maps to BitRouter, the templating/variant half stays in your app or moves to a [preset](/docs/features/presets). See the [feature mapping](#feature-mapping) below.

### From the gateway + ClickHouse stack

The infra migration replaces the Compose stack — gateway container plus ClickHouse — with one binary and no database:

<Tabs items={['Before (TensorZero Compose)', 'After (BitRouter local)']}>
<Tab value="Before (TensorZero Compose)">
```bash
# docker-compose.yml runs the gateway + a ClickHouse warehouse
export OPENAI_API_KEY=sk-...
export TENSORZERO_CLICKHOUSE_URL=http://chuser:chpass@clickhouse:8123/tensorzero
docker compose up   # gateway on :3000, ClickHouse on :8123
# config mounted from ./config/tensorzero.toml
```
</Tab>
<Tab value="After (BitRouter local)">
```bash
# Install (curl, npm, brew, or cargo — see Quick Start)
curl -fsSL https://bitrouter.ai/install.sh | sh

# Set provider keys; BitRouter auto-detects on start
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...

# Interactive wizard (Cloud or local); default local serves on :4356
bitrouter
```
</Tab>
</Tabs>

To skip the local proxy entirely, point clients at `https://api.bitrouter.ai/v1` with a BitRouter API key — no binary, no ClickHouse, no infra.

## Feature mapping

| TensorZero concept | BitRouter equivalent | Docs |
|---|---|---|
| `[models.*]` + `[models.*.providers.*]` in `tensorzero.toml` | Provider keys (auto-detected) + the model registry | [BYOK](/docs/features/byok), [Models](/docs/concepts/models) |
| `[functions.*]` (named prompt + schema) | App-side, or a routing [preset](/docs/features/presets) | [Presets](/docs/features/presets) |
| `[functions.*.variants.*]` (per-variant model) | Routing preset variants / model ids | [Presets](/docs/features/presets) |
| `routing` / `retries` / `fallbacks` | Model fallback rules | [Model fallback](/docs/features/model-fallback) |
| `load_balancing` across providers | Provider selection | [Provider selection](/docs/features/provider-selection) |
| OpenAI-compatible `/openai/v1` endpoint | OpenAI-compatible `/v1` endpoint | [API Reference](/docs/reference/openai-compatible/createChatCompletion) |
| Native `POST /inference` endpoint | OpenAI-, Anthropic-, and Google-compatible protocols | [API Reference](/docs/reference) |
| ClickHouse observability + UI | OTLP traces & metrics to your own backend, or hosted Activity on Cloud | [OpenTelemetry](/docs/features/opentelemetry), [Cloud Tracing](/docs/features/opentelemetry#cloud-activity-hosted) |
| OpenTelemetry (OTLP) export | OpenTelemetry (OTLP) export | [OpenTelemetry](/docs/features/opentelemetry) |
| Embedded structured outputs (JSON functions) | Structured outputs across all providers | [Structured outputs](/docs/features/structured-outputs) |
| — (no equivalent) | MCP / ACP / Skills agent gateways | [Tools](/docs/concepts/tools), [Agents](/docs/concepts/agents) |
| — (no equivalent) | Autonomous agent payment (x402 / MPP) | [Payment](/docs/features/payment) |

## What BitRouter intentionally doesn't ship

To set expectations honestly: BitRouter is a gateway, not an LLMOps platform. It does **not** ship TensorZero's optimization and experimentation loop — no built-in supervised fine-tuning, RLHF, automated prompt engineering, dynamic in-context learning, inference-level or workflow evaluations, or adaptive A/B testing, and no ClickHouse-backed UI for replaying historical inferences against alternative prompts.

If your workflow depends on that closed feedback loop — collect inferences and feedback, evaluate, optimize, experiment — TensorZero is the better fit and you should keep it for those workloads. BitRouter is the right move when the value you need from TensorZero is the *gateway* and the missing half is *agent infrastructure*: tool/agent gateways, an agent firewall, and pay-per-request autonomy.

## Migration checklist

<Callout type="info">
**Before migration**
- [ ] List the providers and models you actually route through TensorZero (skip the rest)
- [ ] Separate gateway use from platform use — are you using evals / optimization / experimentation, or just routing?
- [ ] Note where `[functions.*]` templates and schemas live; plan to keep them app-side or move them to a [preset](/docs/features/presets)
- [ ] Decide cloud vs. local (or both — they share the endpoint)
</Callout>

<Callout type="success">
**Migration**
- [ ] Install the BitRouter CLI ([Quick Start](/docs/get-started/quickstart))
- [ ] Export provider keys, or paste them into the cloud dashboard (sealed-box encrypted)
- [ ] Update client `base_url` to `http://127.0.0.1:4356/v1` (local) or `https://api.bitrouter.ai/v1` (cloud)
- [ ] Replace `tensorzero::…` model strings with `provider/model` ids
- [ ] Re-point OTLP export at your backend ([OpenTelemetry](/docs/features/opentelemetry))
- [ ] Verify with a sample request
- [ ] Decommission the gateway container and ClickHouse if you no longer need the platform loop
</Callout>

## Next steps

<Cards>
  <Card title="Quick Start" href="/docs/get-started/quickstart" description="Run BitRouter locally or in the cloud in under a minute" />
  <Card title="Comparison" href="/docs/get-started/comparison" description="Side-by-side with OpenRouter, LiteLLM, and generic gateways" />
  <Card title="Agent features" href="/docs/concepts/tools" description="MCP, ACP, skills, agent firewall, x402 payment" />
  <Card title="API Reference" href="/docs/reference" description="OpenAI- and Anthropic-compatible endpoints" />
</Cards>

## Get help

- **Discord**: [Join the community](https://discord.gg/G3zVrZDa5C) for migration support
- **GitHub**: [Open an issue](https://github.com/bitrouter/bitrouter/issues)
- **Email**: contact@bitrouter.ai for enterprise migration assistance
