---
title: Migrate from OpenRouter
description: Complete guide for migrating from OpenRouter to BitRouter
sourceHash: f8824812608f57f3f8a2d74827c279aeae1739971c7822f1722d2555fa60d3cb
---

# Migrating from OpenRouter to BitRouter

This guide walks you through migrating your existing OpenRouter integration to BitRouter, highlighting key differences and advantages.

## Why Migrate?

BitRouter offers several advantages over OpenRouter for agent workloads:

| Feature | OpenRouter | BitRouter |
|---------|------------|-----------|
| **Deployment** | Cloud-only (closed-source) | Self-host or cloud (Apache 2.0) |
| **Platform Fee** | 5.5% via Stripe | 2% via stablecoins · 5% via Stripe |
| **Agent Features** | Basic routing | Headless CLI, agentic payment, MCP/ACP gateway |
| **Access** | Account registration required | No registration required |

## Migration Path

### Step 1: Get your BitRouter API key

**Option A: Headless CLI (recommended)**

Install the CLI and sign in — `bitrouter auth login` runs the RFC 8628 device-code flow, opens an approval URL in your browser, and persists the resulting OAuth credential (auto-refreshed) under `$XDG_DATA_HOME/bitrouter/account-credentials.json`. After that, every request through the `bitrouter` provider is authenticated automatically — no API key in your config:

```bash
npm install -g bitrouter
bitrouter auth login
```

Inspect the local session with `bitrouter auth whoami`, and manage account-level resources (API keys, usage, billing, policies, BYOK, OAuth clients) with `bitrouter cloud --help`.

**Option B: Dashboard**

Sign up at [cloud.bitrouter.ai](https://cloud.bitrouter.ai) and copy your API key from the dashboard.

### Step 2: Update base URL and API key

That's the full migration. Replace your OpenRouter endpoint and key with BitRouter's:

<Tabs items={['Python', 'JavaScript', 'curl']}>
<Tab value="Python">
```python
# Before (OpenRouter)
client = openai.OpenAI(
    base_url="https://openrouter.ai/api/v1",
    api_key=OPENROUTER_API_KEY,
)

# After (BitRouter)
client = openai.OpenAI(
    base_url="https://api.bitrouter.ai/v1",
    api_key=BITROUTER_API_KEY,
)
```
</Tab>
<Tab value="JavaScript">
```javascript
// Before (OpenRouter)
const client = new OpenAI({
  baseURL: 'https://openrouter.ai/api/v1',
  apiKey: OPENROUTER_API_KEY,
});

// After (BitRouter)
const client = new OpenAI({
  baseURL: 'https://api.bitrouter.ai/v1',
  apiKey: BITROUTER_API_KEY,
});
```
</Tab>
<Tab value="curl">
```bash
# Before (OpenRouter)
curl https://openrouter.ai/api/v1/chat/completions \
  -H "Authorization: Bearer $OPENROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "openai/gpt-4o", "messages": [{"role": "user", "content": "Hello"}]}'

# After (BitRouter)
curl https://api.bitrouter.ai/v1/chat/completions \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "openai/gpt-4o", "messages": [{"role": "user", "content": "Hello"}]}'
```
</Tab>
</Tabs>


## Headers to Remove

If your OpenRouter integration sets any of these headers, you can safely drop them:

| OpenRouter header | Status in BitRouter |
|-------------------|---------------------|
| `HTTP-Referer` | Not used |
| `X-Title` | Not used |
| `transforms` | Not used — guardrails are configured server-side |
| `route` | Not used — provider routing is configured in the dashboard |

## Next Steps

<Cards>
  <Card title="Quick Start" href="/docs/get-started/quickstart" description="Get BitRouter running in under a minute" />
  <Card title="Configuration" href="/docs/features/presets" description="Advanced routing and configuration options" />
  <Card title="Integrations" href="/docs/integrations" description="Integration guides for specific agents" />
  <Card title="API Reference" href="/docs/reference" description="Complete API documentation" />
</Cards>

## Need Help?

- **Discord**: [Join our community](https://discord.gg/G3zVrZDa5C) for migration support
- **GitHub**: [Report issues](https://github.com/bitrouter/bitrouter/issues) or contribute
- **Email**: contact@bitrouter.ai for enterprise migration assistance
