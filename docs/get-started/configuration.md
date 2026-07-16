---
title: Configuration
description: Install BitRouter and wire it into your agent in under a minute — via Agent Skills or the CLI/TUI, self-hosted with your own keys or attached to BitRouter Cloud.
---

This page gets BitRouter routing for your agent in under a minute, then covers every way to configure it. There are two deployment modes, and two ways to onboard either one.

- **BitRouter Cloud (default).** Hosted endpoint at `api.bitrouter.ai`. No upstream keys to manage; agent-native x402/MPP pay-per-use, or BYOK on top.
- **Local proxy.** A single binary on your machine, BYOK with your own provider keys. Zero infrastructure dependencies.

Both modes run the **same open-source core** (Apache 2.0) — the routing engine is identical, so you can start in one mode and switch later without touching your harness. Not sure which to pick? See [Self-host or Cloud?](/docs/get-started/faqs#self-host-or-cloud) in the FAQ.

## Install the binary

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

## Onboard via Agent Skills

For agent runtimes that support skills (Claude Code, Cursor, Codex, Copilot, etc.). Install once:

```bash
npx skills add bitrouter/bitrouter
```

Then ask your agent: *"Set up BitRouter for me."* — the agent runs the wizard, picks Cloud by default, and verifies the connection autonomously.

## Onboard via the CLI / TUI

For terminal-first setup, launch the wizard:

```bash
bitrouter
```

The wizard walks three steps — **credentials** (sign in to BitRouter Cloud by default, log in to a provider, or paste a BYOK key), **harness** (Claude Code or Codex), and **finish** (launch the agent, start the proxy at `http://127.0.0.1:4356`, or exit). Re-run it any time with `bitrouter init`; add `--yes` to drive it non-interactively (it emits a JSON result envelope and never blocks).

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

You can also point an agent straight at the hosted endpoint without running a local binary. Either way the core is the same — a Cloud account is an account and network, not a separate deployment. See the [Supported Models](/docs/get-started/supported-models) catalog for pricing.

## Attach Cloud to a self-hosted binary

Cloud is not a different binary — it's an account you attach:

```bash
bitrouter cloud login
# Opens a browser to sign in and pick a workspace.
# Your local binary now routes Cloud-managed models alongside your BYOK keys.
```

You can add or remove the Cloud account at any time. The binary's self-hosted capabilities are unaffected either way.

## Point your agent at the proxy

However you start it, BitRouter is a drop-in proxy. Point your agent runtime at the proxy base URL — `http://127.0.0.1:4356` when self-hosting — and every model call routes through BitRouter with no harness changes.

Verify with a request:

```bash
curl http://127.0.0.1:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openai/gpt-4o",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

Point any OpenAI-compatible runtime at `http://127.0.0.1:4356/v1` to route through BitRouter.

## Next steps

<Cards>
  <Card title="Integrations" href="/docs/integrations" description="Step-by-step guides for every supported agent runtime" />
  <Card title="Supported Models" href="/docs/get-started/supported-models" description="The full catalog, pricing, and open-model discounts" />
  <Card title="FAQs" href="/docs/get-started/faqs" description="Self-host vs Cloud, what Cloud adds, and how BitRouter compares" />
  <Card title="For Providers" href="/docs/guides/register-as-a-provider" description="List your models on the BitRouter Registry" />
</Cards>
