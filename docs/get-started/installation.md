---
title: Installation
description: Install the open-source BitRouter binary and run it locally, or sign in to BitRouter Cloud — same core either way.
sourceHash: 3e9c4c0d2e7d148bb2cdf6f0abf5e02101e9a3da5de30dd3f091335a5c59423e
---

BitRouter has two front doors, and both run the **same open-source core**. Self-host the binary with your own keys, or sign in to BitRouter Cloud — the routing engine is identical. See [Self-host vs Cloud](/docs/get-started/self-hosted-vs-cloud) for what the hosted account adds on top.

## Self-host the binary

Install the open-source binary (Apache 2.0):

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

Set your provider keys in the environment and start the proxy:

```bash
export OPENAI_API_KEY=sk-...    # ANTHROPIC_API_KEY / GOOGLE_API_KEY also work
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
bitrouter auth login    # RFC 8628 device flow against api.bitrouter.ai
bitrouter start         # the `bitrouter` provider auto-enables once signed in
```

You can also point an agent straight at the hosted endpoint without running a local binary. Either way the core is the same — a Cloud account is an account and network, not a separate deployment. See [Self-host vs Cloud](/docs/get-started/self-hosted-vs-cloud) for the full breakdown, and the [managed provider](/docs/get-started/models-and-providers) for the model catalog and pricing.

## Point your agent at the proxy

However you start it, BitRouter is a drop-in proxy. Point your agent runtime at the proxy base URL — `http://127.0.0.1:4356` when self-hosting — and every model call routes through BitRouter with no harness changes.

<Cards>
  <Card title="Quick Start" href="/docs/get-started/quickstart" description="Get an agent routing through BitRouter in under a minute" />
  <Card title="Self-host vs Cloud" href="/docs/get-started/self-hosted-vs-cloud" description="Identical core; what a Cloud account adds on top" />
</Cards>
