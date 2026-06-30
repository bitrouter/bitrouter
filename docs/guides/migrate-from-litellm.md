---
title: Migrate from LiteLLM
description: Move a LiteLLM SDK or Proxy setup to BitRouter — local binary, hosted cloud, or both.
sourceHash: 374ade959220237745e7bd565e3c56fbe9c79914947aea588f4507ef86fb5121
---

# Migrating from LiteLLM to BitRouter

LiteLLM is a Python SDK and self-hosted proxy for unifying access to 100+ LLM providers. BitRouter solves the same problem from a different angle: an **agent-native proxy** with a single OpenAI-compatible surface — runnable as a local binary (BYOK, no infra) or as a hosted endpoint with autonomous agent payments. This guide is for teams whose workload has shifted from backend services to agent runtimes and want a leaner, agent-first surface.

## Why migrate?

| | LiteLLM Proxy | BitRouter |
|---|---|---|
| **Runtime** | Python | Rust (single static binary) |
| **Production deps** | Postgres + Redis + Docker/K8s | None |
| **Deployment modes** | Self-hosted only | Local binary **or** hosted (`cloud.bitrouter.ai`) — same OpenAI-compatible endpoint |
| **Agentic auth & payment** | None | x402 / MPP autonomous payment (cloud mode) |
| **Design focus** | All-in-one LLM gateway: admin UI, virtual keys, budgets, with agent gateways added alongside | Agent-first proxy: MCP / ACP / Skills, agent firewall, agentic payment as the core surface |
| **Agent protocol surface** | MCP, A2A, Skills, CLI — bolted onto the horizontal gateway | MCP, ACP, Skills, CLI — the product, not an add-on |
| **License** | MIT (SDK) / paid enterprise tier | Apache 2.0 throughout |

## Two things worth highlighting

### 1. Cloud and local share the same surface

With LiteLLM, the proxy is something you operate. With BitRouter, **the hosted cloud and local binary expose the same OpenAI-compatible endpoint** — you can start local during development, then point at `cloud.bitrouter.ai` for production (or vice versa) without changing client code. The CLI, the wizard, and Agent Skills all work in either mode; toggle with one keypress in the setup TUI. See the [Quick Start](/docs/get-started/quickstart) for both flows.

This matters when your agent should *pay per request* without you provisioning keys for it — Cloud mode supports [x402 / MPP autonomous payments](/docs/features/payment), which LiteLLM has no equivalent for.

### 2. Agent-native, not all-in-one

LiteLLM has shipped MCP, A2A, Skills, and a CLI alongside its horizontal LLM gateway — virtual keys, team budgets, spend dashboards, admin UI all included. BitRouter inverts that: agent primitives *are* the product, the team-admin stack is intentionally minimal. What you get on the BitRouter surface:

- [MCP gateway](/docs/concepts/tools) — proxy MCP servers so agents discover tools across hosts.
- [ACP gateway](/docs/concepts/agents) — first-class support for the Agent Client Protocol used by Claude Code, Codex, OpenCode, and others.
- [Guardrails](/docs/features/guardrails) — regex rules on the proxy hop that redact or block matching content inline.
- [Cloud Tracing](/docs/features/opentelemetry#cloud-activity-hosted) — built-in spend and request tracing, no external collector required.
- Agent Skills gateway (coming soon) — install and route capabilities by skill, not by raw model name.
- [Headless CLI](/docs/concepts/cli) — TUI wizard plus scriptable commands for setup and ops.
- [Agentic auth & payment](/docs/features/payment) — x402 / MPP so an agent can pay per request without you provisioning a key for it. LiteLLM has no equivalent.

If you depend on LiteLLM's team-admin UI, virtual keys, and per-user budgets, LiteLLM is still the better fit. If you're building agents — especially agents that should transact autonomously — BitRouter is.

## Migration paths

### From the LiteLLM Python SDK

LiteLLM as a library becomes a BitRouter base URL swap on the standard OpenAI SDK:

<Tabs items={['Before (LiteLLM SDK)', 'After (BitRouter)']}>
<Tab value="Before (LiteLLM SDK)">
```python
from litellm import completion

response = completion(
    model="openai/gpt-4o",
    messages=[{"role": "user", "content": "Hello"}],
    api_key="sk-...",
)
```
</Tab>
<Tab value="After (BitRouter)">
```python
import openai

# Local: `bitrouter` (BYOK via env vars) — see /docs/get-started/quickstart
# Cloud: base_url="https://cloud.bitrouter.ai/v1", api_key=$BITROUTER_API_KEY
client = openai.OpenAI(
    base_url="http://127.0.0.1:4356/v1",
    api_key="not-used-in-local-byok",
)

response = client.chat.completions.create(
    model="openai/gpt-4o",
    messages=[{"role": "user", "content": "Hello"}],
)
```
</Tab>
</Tabs>

Fallbacks and provider selection that you'd configure with `litellm.Router` move into BitRouter's [routing presets](/docs/features/presets) and [model fallback rules](/docs/features/model-fallback) — declared once, not per call site.

### From the LiteLLM Proxy

The proxy migration replaces the Python process + Postgres + Redis with a single binary. Install and launch:

<Tabs items={['Before (LiteLLM Proxy)', 'After (BitRouter local)']}>
<Tab value="Before (LiteLLM Proxy)">
```bash
docker run -d -p 5432:5432 -e POSTGRES_PASSWORD=pass postgres
docker run -d -p 6379:6379 redis
pip install 'litellm[proxy]'
litellm --config config.yaml --port 8000
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

To skip the local proxy entirely, point clients at `https://cloud.bitrouter.ai/v1` with a BitRouter API key — no binary, no infra. Same endpoint shape.

## Feature mapping

| LiteLLM concept | BitRouter equivalent | Docs |
|---|---|---|
| `model_list` in `config.yaml` | Provider keys + routing presets | [Presets](/docs/features/presets) |
| `router_settings` (retries, fallback) | Model fallback rules | [Model fallback](/docs/features/model-fallback) |
| `routing_strategy` (least-busy, latency) | Provider selection | [Provider selection](/docs/features/provider-selection) |
| `cache` (Redis/DynamoDB backed) | Not built into the proxy — handle in app/edge if needed | — |
| Virtual keys + budgets + admin UI | Workspace keys (cloud); env-var keys (local) | [BYOK](/docs/features/byok), [Workspaces](/docs/features/namespaces) |
| Guardrails / PII / content filter | Agent firewall on the proxy hop | [Guardrails](/docs/features/guardrails) |
| Callbacks (Langfuse, Datadog, etc.) | Built-in spend + request logs; OTLP export | [OpenTelemetry](/docs/features/opentelemetry) |
| MCP Gateway | MCP gateway | [MCP](/docs/concepts/tools) |
| A2A Agent Gateway | ACP gateway | [ACP](/docs/concepts/agents) |
| Skills Gateway / `/skills` endpoint | Skills gateway with [agentskills.io](https://agentskills.io) registry | [Agent Skills](/docs/concepts/tools) |
| LiteLLM Proxy CLI | `bitrouter` CLI / TUI | [Headless CLI](/docs/concepts/cli) |
| — (no equivalent) | Autonomous agent payment (x402 / MPP) | [Payment](/docs/features/payment) |

## What BitRouter intentionally doesn't ship

To set expectations honestly: BitRouter does not ship a built-in admin UI for team/user budgets, virtual-key generation by API, or a spend analytics dashboard at parity with LiteLLM Enterprise. Per-workspace key scoping in cloud mode and env-var-scoped keys in local mode cover the common cases, but if your migration depends on per-user virtual keys with quotas enforced inside the proxy, plan for that gap or stay on LiteLLM for those workloads.

## Migration checklist

<Callout type="info">
**Before migration**
- [ ] List the providers and models you actually use (skip the rest)
- [ ] Note any custom callbacks/middleware — see if a [guardrail rule](/docs/features/guardrails) covers it
- [ ] Decide cloud vs. local (or both — they share the endpoint)
</Callout>

<Callout type="success">
**Migration**
- [ ] Install the BitRouter CLI ([Quick Start](/docs/get-started/quickstart))
- [ ] Export provider keys, or paste them into the cloud dashboard (sealed-box encrypted)
- [ ] Update client `base_url` to `http://127.0.0.1:4356/v1` (local) or `https://cloud.bitrouter.ai/v1` (cloud)
- [ ] Verify with a sample request
- [ ] Decommission Postgres / Redis if local-only setup is sufficient
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
