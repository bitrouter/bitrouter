---
title: Quick Start
description: Run BitRouter in under a minute, via Agent Skills or the CLI/TUI.
sourceHash: d13266c9cc32a83fe7b97c37ae8d00fd6ebd116673c84b9b10de0f3432b3f1db
---

This guide gets BitRouter routing for your agent in under a minute. There are two deployment modes — and two ways to onboard either mode:

- **BitRouter Cloud (default).** Hosted endpoint at `cloud.bitrouter.ai`. No keys to manage; agent-native x402/MPP pay-per-use, or BYOK on top.
- **Local proxy.** Single binary on your machine, BYOK with your own provider keys. Zero infrastructure dependencies.

Both modes onboard through **Agent Skills** (let your agent install and configure itself) or the **BitRouter CLI / TUI** (interactive wizard). Both default to Cloud; switch to local with a single keypress.

## Agent Skills

For agent runtimes that support skills (Claude Code, Cursor, Codex, Copilot, etc.). Install once:

```bash
npx skills add bitrouter/bitrouter
```

Then ask your agent: *"Set up BitRouter for me."* — the agent runs the wizard, picks Cloud by default, and verifies the connection autonomously.

## BitRouter CLI

For terminal-first setup. Install the binary:

<Tabs items={['curl', 'npm', 'Homebrew', 'cargo']}>
<Tab value="curl">

```bash
curl -fsSL https://bitrouter.ai/install.sh | sh
```

</Tab>
<Tab value="npm">

```bash
npm install -g bitrouter
```

</Tab>
<Tab value="Homebrew">

```bash
brew install bitrouter/tap/bitrouter
```

</Tab>
<Tab value="cargo">

```bash
cargo install bitrouter
```

</Tab>
</Tabs>

Launch the TUI wizard:

```bash
bitrouter
```

The wizard asks **Cloud or local?** (default Cloud), prompts for keys if you choose local, then starts the proxy at `http://127.0.0.1:4356`.

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

## Next Steps

Guides for connecting specific agent runtimes — Claude Code, OpenClaw, Codex CLI, OpenCode, and more — live under Integrations.

<Cards>
  <Card title="Integrations" href="/docs/integrations" description="Step-by-step guides for every supported agent runtime" />
  <Card title="Comparison" href="/docs/get-started/comparison" description="How BitRouter differs from OpenRouter, LiteLLM, and other gateways" />
  <Card title="For Providers" href="/docs/guides/register-as-a-provider" description="List your models on the BitRouter Registry" />
  <Card title="API Reference" href="/docs/reference" description="Endpoint and schema documentation" />
  <Card title="GitHub" href="https://github.com/bitrouter" description="Source code and contributions" external />
  <Card title="Discord" href="https://discord.gg/G3zVrZDa5C" description="Community help and updates" external />
</Cards>
