# BitRouter × OpenClaw: From Zero to Agent Routing in 5 Minutes

> **Live demo script** — walk through integrating BitRouter with an [OpenClaw](https://docs.openclaw.ai) gateway from scratch. Every step is runnable; narrate as you go.

---

## What You'll Show

| #   | Feature                                                                                               | Time   |
| --- | ----------------------------------------------------------------------------------------------------- | ------ |
| 1   | [One-click integration](#1-one-click-integration) — point OpenClaw at BitRouter                       | 1 min  |
| 2   | [Multi-provider routing](#2-multi-provider-routing) — one endpoint, every LLM                         | 1 min  |
| 3   | [Model discovery](#3-model-discovery) — agents see all models dynamically                             | 30 sec |
| 4   | [Smart routing](#4-smart-routing) — route by task, not by name                                        | 1 min  |
| 5   | [Agent firewall](#5-agent-firewall) — protect secrets and block risky output                          | 1 min  |
| 6   | [Tools as a service](#6-tools-as-a-service) — tool routing through the MCP gateway                    | 1 min  |
| 7   | [Spend tracking](#7-spend-tracking) — per-request cost visibility                                     | 30 sec |
| 8   | [Key management & auth](#8-key-management--auth) — one key for the agent, all keys stay on the server | 1 min  |
| 9   | [Hot reload](#9-hot-reload) — change routes without restarting anything                               | 30 sec |

**Total: ~8 minutes of demo, expandable to 15+ with Q&A.**

---

## Prerequisites

| Tool          | Install                                                                  |
| ------------- | ------------------------------------------------------------------------ |
| **BitRouter** | `cargo install bitrouter`                                                |
| **OpenClaw**  | `npm install -g openclaw`                                                |
| **API keys**  | At least one of: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY` |

---

## The Problem (Before BitRouter)

Show a typical OpenClaw config with direct provider keys — the "before" picture:

```jsonc
// ~/.openclaw/openclaw.json — the old way
{
  "agents": {
    "defaults": {
      "model": { "primary": "anthropic/claude-sonnet-4-6" },
    },
  },
  // Every key lives in the agent process
  // No cost control, no firewall, no fallback
  // Adding a second provider = more config, more keys
}
```

**Pain points to narrate:**

- API keys live inside the agent process — one leaked tool call away from exposure
- Adding a new provider means editing OpenClaw config, adding auth profiles, and restarting
- No cross-provider fallback — if Anthropic is rate-limited, the agent is stuck
- No visibility into what the agent is spending
- No way to block the agent from leaking secrets in its prompts

> _"What if the agent could get all of this through a single endpoint?"_

---

## 1. One-Click Integration

### Step 1a: Start BitRouter (one command)

```bash
# Terminal 1 — BitRouter auto-detects your API keys from the environment
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...

bitrouter serve
# ✅ Listening on http://localhost:8787
# ✅ Detected providers: openai, anthropic
```

BitRouter reads `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` from the environment (or a `.env` file), loads the built-in provider definitions, and starts serving.

### Step 1b: Point OpenClaw at BitRouter (one config change)

```jsonc
// ~/.openclaw/openclaw.json — the new way
{
  "models": {
    "providers": {
      "bitrouter": {
        "baseUrl": "http://localhost:8787/v1",
        "apiKey": "any-string", // or a real JWT — see step 8
        "api": "openai-responses", // BitRouter speaks OpenAI protocol
        "models": [
          { "id": "smart", "name": "Smart (routed)", "reasoning": true },
          { "id": "fast", "name": "Fast (routed)" },
        ],
      },
    },
  },
  "agents": {
    "defaults": {
      "model": { "primary": "bitrouter/smart" },
    },
  },
}
```

**That's it.** Restart the gateway (`openclaw gateway restart`) and every OpenClaw agent now routes through BitRouter.

### What just happened

```
┌─────────┐         ┌───────────┐         ┌──────────┐
│ OpenClaw │──req──▶│ BitRouter  │──route─▶│ OpenAI   │
│  Agent   │◀─res──│  :8787     │◀────────│ Anthropic│
└─────────┘         └───────────┘         │ Google   │
                                           └──────────┘
```

- OpenClaw thinks it's talking to one provider ("bitrouter")
- BitRouter routes to the best upstream provider
- All API keys stay on the BitRouter side — the agent never sees them

---

## 2. Multi-Provider Routing

Create a `~/.bitrouter/bitrouter.yaml` to show explicit routing:

```yaml
# ~/.bitrouter/bitrouter.yaml
models:
  smart:
    strategy: priority
    endpoints:
      - provider: anthropic
        service_id: claude-sonnet-4-20250514
      - provider: openai
        service_id: gpt-4.1

  fast:
    strategy: priority
    endpoints:
      - provider: openai
        service_id: gpt-4.1-mini
      - provider: anthropic
        service_id: claude-haiku-3-5-20241022

  reasoning:
    strategy: priority
    endpoints:
      - provider: anthropic
        service_id: claude-opus-4-20250115
      - provider: openai
        service_id: o3
```

### Live demo

```bash
# Reload BitRouter — no restart needed
bitrouter reload

# Send a request through OpenClaw — it goes to claude-sonnet first
# If Anthropic is rate-limited, BitRouter falls back to GPT-4.1 automatically
```

**Narrate:** _"The agent asks for 'smart' — BitRouter tries Claude Sonnet first, falls back to GPT-4.1. The agent doesn't know or care which one answered. Add a third fallback? Just add a line to the YAML."_

### Load-balanced key pooling

```yaml
models:
  smart:
    strategy: load_balance
    endpoints:
      - provider: anthropic
        service_id: claude-sonnet-4-20250514
        api_key: "${ANTHROPIC_API_KEY_1}"
      - provider: anthropic
        service_id: claude-sonnet-4-20250514
        api_key: "${ANTHROPIC_API_KEY_2}"
```

_"Two Anthropic keys, round-robin. Double your rate limits without the agent knowing."_

---

## 3. Model Discovery

BitRouter exposes a model catalog that OpenClaw (or any agent) can query:

```bash
# From a terminal — what models can the agent use?
curl -s http://localhost:8787/v1/models | jq '.data[].id'
```

```
"smart"
"fast"
"reasoning"
"openai:gpt-4.1"
"openai:gpt-4.1-mini"
"anthropic:claude-sonnet-4-20250514"
...
```

The agent can query `GET /v1/models` at runtime to discover what's available — including metadata like pricing, context window, and supported modalities.

```bash
# Filter by provider
curl -s "http://localhost:8787/v1/models?provider=anthropic" | jq '.data[] | {id, max_input_tokens}'

# Filter by input modality
curl -s "http://localhost:8787/v1/models?input_modalities=image" | jq '.data[].id'
```

**Narrate:** _"The agent doesn't need a hardcoded model list. It asks BitRouter 'what can I use?' and gets a live catalog."_

### OpenClaw's `/model` command

With the BitRouter provider configured, users can switch models in chat:

```
/model bitrouter/smart
/model bitrouter/fast
/model bitrouter/reasoning
```

---

## 4. Smart Routing

> **Note:** Signal-based auto-routing is a planned feature. The YAML below shows the intended design.

Add content-based routing rules — the agent sends plain requests and BitRouter picks the right model:

```yaml
# ~/.bitrouter/bitrouter.yaml
routing:
  auto:
    signals:
      coding:
        keywords: [function, class, bug, refactor, test, compile, deploy]
      research:
        keywords: [research, analyze, compare, summarize, literature]
      creative:
        keywords: [story, poem, brainstorm, imagine, design]
    complexity:
      high_keywords: [optimize, architect, complex, production, scale]
      message_length_threshold: 1000
    models:
      coding.high: reasoning
      coding.low: smart
      research.high: reasoning
      research.low: smart
      creative.high: smart
      creative.low: fast
      default: fast
```

**Narrate:** _"The OpenClaw agent just sends a message. BitRouter detects 'this looks like a complex coding task' and routes to the reasoning model. A casual chat? Goes to the fast model. The agent doesn't pick models — it just talks."_

---

## 5. Agent Firewall

The killer feature for autonomous agents. Add guardrails to `bitrouter.yaml`:

```yaml
# ~/.bitrouter/bitrouter.yaml
guardrails:
  enabled: true
  upgoing:
    api_keys: redact # Strip API keys from prompts
    private_keys: block # Block private keys entirely
    pii_emails: redact # Redact email addresses
    credentials: redact # Redact passwords, tokens, etc.
  downgoing:
    suspicious_commands: block # Block "rm -rf", "DROP TABLE", etc.
  custom_patterns:
    - name: internal_tokens
      regex: "myapp_[A-Za-z0-9]{32}"
      direction: upgoing
      action: redact
  block_message:
    include_details: true
  tools:
    enabled: true
    providers:
      github-mcp:
        filter:
          deny: [delete_repo, delete_branch, delete_file]
        param_restrictions:
          rules:
            create_issue:
              deny: [assignees]
              action: strip
```

### Live demo

```bash
# Send a message with a secret in it (from OpenClaw)
# "Here's my config: OPENAI_API_KEY=sk-1234567890abcdef"
#
# BitRouter intercepts → redacts the key → forwards safely
# The LLM never sees the real key
```

**Narrate:** _"The agent tried to include an API key in its prompt — maybe it read a config file. BitRouter caught it, redacted it, and forwarded a clean message. The LLM never saw the secret."_

### Tool-level guardrails

_"The agent can call `create_issue` but NOT `delete_repo`. It can create issues but can't set assignees (the `assignees` field is silently stripped). This is defense-in-depth for autonomous agents."_

---

## 6. Tools as a Service

Show the MCP gateway — agents discover and call tools through BitRouter:

```yaml
# ~/.bitrouter/bitrouter.yaml
providers:
  github-mcp:
    api_protocol: mcp
    api_base: "https://api.githubcopilot.com/mcp"
    api_key: "${GITHUB_TOKEN}"
    bridge: true

tools:
  web_search:
    endpoints:
      - provider: exa
        service_id: search
    description: "Search the web using Exa's neural search"
```

```bash
# The agent discovers available tools
curl -s http://localhost:8787/v1/tools | jq '.[].name'
```

```
"web_search"
"github-mcp__create_issue"
"github-mcp__search_repos"
...
```

### OpenClaw MCP integration

OpenClaw can consume BitRouter's MCP gateway as a tool source:

```jsonc
// ~/.openclaw/openclaw.json
{
  "models": {
    "providers": {
      "bitrouter": {
        "baseUrl": "http://localhost:8787/v1",
        "apiKey": "any-string",
        "api": "openai-responses",
        "models": [{ "id": "smart", "name": "Smart (routed)", "reasoning": true }],
      },
    },
  },
  // Skills can call BitRouter tools via HTTP
  // Or use the MCP gateway directly
}
```

**Narrate:** _"One MCP gateway endpoint. The agent discovers tools from GitHub, Exa, and any other MCP server — all through BitRouter. Adding a new tool server is one YAML block."_

---

## 7. Spend Tracking

BitRouter tracks cost per request automatically:

```bash
# After a few OpenClaw conversations, check spend
bitrouter status
```

```
Server: running (http://localhost:8787)
Uptime: 12m 34s

Models:
  smart     │ 47 requests │ $0.23 total │ avg 1.2s
  fast      │ 112 requests│ $0.04 total │ avg 0.3s
  reasoning │ 8 requests  │ $0.89 total │ avg 4.1s

Tools:
  web_search │ 15 calls │ $0.015 total
```

### Per-request cost breakdown

Every response from BitRouter includes cost metadata that agents or observability tools can consume:

```
Input:  2,341 tokens (cache_read: 1,800 @ $0.30/M, no_cache: 541 @ $3.00/M)
Output: 856 tokens (text: 856 @ $15.00/M)
Total:  $0.014382
```

**Narrate:** _"Every request is metered. You know exactly what the agent is spending, broken down by model, by cache hit rate, by input vs output. No surprises on your bill."_

---

## 8. Key Management & Auth

Show the full auth story — the agent gets one JWT, BitRouter holds all the provider keys:

### Generate a key for the OpenClaw agent

```bash
# Create a wallet (one-time)
bitrouter wallet create --name demo --words 12

# Create an API key with budget and model restrictions
bitrouter key sign \
  --wallet demo \
  --models "smart,fast,reasoning" \
  --budget 5000000 \
  --exp 30d
```

```
eyJhbGciOiJTT0xfRUREU0EiLCJ0eXAiOiJKV1QifQ...
```

### Use it in OpenClaw

```jsonc
// ~/.openclaw/openclaw.json
{
  "models": {
    "providers": {
      "bitrouter": {
        "baseUrl": "http://localhost:8787/v1",
        "apiKey": "eyJhbGciOiJTT0xfRUREU0EiLCJ0eXAiOiJKV1QifQ...",
        "api": "openai-responses",
        "models": [
          { "id": "smart", "name": "Smart (routed)", "reasoning": true },
          { "id": "fast", "name": "Fast (routed)" },
          { "id": "reasoning", "name": "Deep Reasoning", "reasoning": true },
        ],
      },
    },
  },
  "agents": {
    "defaults": {
      "model": { "primary": "bitrouter/smart" },
      // Fallback through BitRouter models — each one already has provider failover
      "model": {
        "primary": "bitrouter/smart",
        "fallbacks": ["bitrouter/fast"],
      },
    },
  },
}
```

**Key properties (encoded in the JWT):**

| Field    | Value                            | Meaning                         |
| -------- | -------------------------------- | ------------------------------- |
| `models` | `["smart", "fast", "reasoning"]` | Agent can only use these routes |
| `budget` | `5000000` (= $5.00)              | Agent's spending cap (μUSD)     |
| `exp`    | 30 days                          | Key auto-expires                |

**Narrate:** _"The agent gets one token. It can access 'smart', 'fast', and 'reasoning' — nothing else. It has a $5 budget. When the budget runs out or the key expires, it stops working. The actual OpenAI/Anthropic keys never leave the BitRouter server."_

### Revoke a key

```bash
bitrouter key revoke --id <key_id>
# Immediate — the agent's next request is rejected
```

---

## 9. Hot Reload

Show that you can change everything without downtime:

```bash
# Add a new model route while BitRouter is running
cat >> ~/.bitrouter/bitrouter.yaml << 'EOF'
  creative:
    strategy: priority
    endpoints:
      - provider: anthropic
        service_id: claude-sonnet-4-20250514
EOF

# Reload — no restart, no dropped connections
bitrouter reload

# Verify
curl -s http://localhost:8787/v1/models | jq '.data[].id' | grep creative
# "creative"
```

Or send a SIGHUP:

```bash
kill -HUP $(cat ~/.bitrouter/run/bitrouter.pid)
```

Dynamic routes also work at runtime without touching the config:

```bash
bitrouter route add creative-v2 anthropic:claude-sonnet-4-20250514
# Available immediately, survives reload but not restart
```

**Narrate:** _"Changed your mind about routing? Edit the YAML and reload. No restart, no dropped connections, no agent downtime. Dynamic routes are even faster — one CLI command."_

---

## Architecture: The Full Picture

```
┌────────────────────────────────────────────────────────────────┐
│                        OpenClaw Gateway                        │
│                                                                │
│  WhatsApp ─┐                                                   │
│  Discord  ─┤  ┌────────┐    ┌─────────────────────────────┐   │
│  Telegram ─┼─▶│ Agent  │───▶│ models.providers.bitrouter  │   │
│  Slack    ─┤  │ (Pi)   │    │ baseUrl: localhost:8787/v1   │   │
│  iMessage ─┘  └────────┘    └──────────────┬──────────────┘   │
│                                             │                  │
└─────────────────────────────────────────────┼──────────────────┘
                                              │
                    ┌─────────────────────────▼──────────────────────────┐
                    │                   BitRouter                        │
                    │                                                    │
                    │  ┌──────────┐  ┌───────────┐  ┌──────────────┐   │
                    │  │ Firewall │  │  Routing   │  │   Spend      │   │
                    │  │ inspect  │─▶│  priority  │─▶│   Tracking   │   │
                    │  │ redact   │  │  balance   │  │   per-req $  │   │
                    │  │ block    │  │  auto      │  └──────────────┘   │
                    │  └──────────┘  └─────┬─────┘                      │
                    │                      │                             │
                    │         ┌─────────────┼─────────────┐             │
                    │         ▼             ▼             ▼             │
                    │    ┌─────────┐  ┌──────────┐  ┌─────────┐       │
                    │    │ OpenAI  │  │Anthropic │  │ Google  │       │
                    │    │ $KEY_1  │  │ $KEY_1   │  │ $KEY_1  │       │
                    │    │ $KEY_2  │  │ $KEY_2   │  │         │       │
                    │    └─────────┘  └──────────┘  └─────────┘       │
                    │                                                    │
                    │  ┌────────────────────────────────────────────┐   │
                    │  │              MCP Gateway                    │   │
                    │  │  GitHub MCP  │  Exa Search  │  Custom...   │   │
                    │  └────────────────────────────────────────────┘   │
                    └────────────────────────────────────────────────────┘
```

---

## What BitRouter Gives OpenClaw Agents

| Without BitRouter             | With BitRouter                |
| ----------------------------- | ----------------------------- |
| API keys in the agent process | Keys stay on the server       |
| One provider at a time        | Multi-provider failover       |
| Manual model selection        | Content-aware auto-routing    |
| No cost visibility            | Per-request spend tracking    |
| No content inspection         | Agent firewall (redact/block) |
| Tool configs per agent        | Centralized MCP gateway       |
| Restart to change models      | Hot reload + dynamic routes   |
| No budget enforcement         | JWT budget caps per agent     |

---

## Appendix: Minimal Config Files

### BitRouter — `~/.bitrouter/bitrouter.yaml`

```yaml
# Minimal BYOK config — BitRouter auto-detects env keys
models:
  smart:
    strategy: priority
    endpoints:
      - provider: anthropic
        service_id: claude-sonnet-4-20250514
      - provider: openai
        service_id: gpt-4.1
  fast:
    strategy: priority
    endpoints:
      - provider: openai
        service_id: gpt-4.1-mini

guardrails:
  enabled: true
  upgoing:
    api_keys: redact
    private_keys: block
    credentials: redact
  downgoing:
    suspicious_commands: block
```

### BitRouter — `~/.bitrouter/.env`

```bash
OPENAI_API_KEY=sk-...
ANTHROPIC_API_KEY=sk-ant-...
# Optional:
# GEMINI_API_KEY=...
# EXA_API_KEY=...
# GITHUB_TOKEN=ghp_...
```

### OpenClaw — `~/.openclaw/openclaw.json`

```jsonc
{
  "models": {
    "providers": {
      "bitrouter": {
        "baseUrl": "http://localhost:8787/v1",
        "apiKey": "any-string",
        "api": "openai-responses",
        "models": [
          { "id": "smart", "name": "Smart (routed)", "reasoning": true },
          { "id": "fast", "name": "Fast (routed)" },
        ],
      },
    },
  },
  "agents": {
    "defaults": {
      "model": {
        "primary": "bitrouter/smart",
        "fallbacks": ["bitrouter/fast"],
      },
    },
  },
}
```

---

## Appendix: Multi-Agent OpenClaw + BitRouter

For multi-agent OpenClaw setups, each agent can target different BitRouter routes:

```jsonc
{
  "models": {
    "providers": {
      "bitrouter": {
        "baseUrl": "http://localhost:8787/v1",
        "apiKey": "${BITROUTER_JWT}",
        "api": "openai-responses",
        "models": [
          { "id": "smart", "name": "Smart", "reasoning": true },
          { "id": "fast", "name": "Fast" },
          { "id": "reasoning", "name": "Deep Reasoning", "reasoning": true },
        ],
      },
    },
  },
  "agents": {
    "list": [
      {
        "id": "daily",
        "model": { "primary": "bitrouter/fast" },
        "workspace": "~/.openclaw/workspace-daily",
      },
      {
        "id": "coding",
        "model": { "primary": "bitrouter/smart" },
        "workspace": "~/.openclaw/workspace-coding",
      },
      {
        "id": "research",
        "model": { "primary": "bitrouter/reasoning" },
        "workspace": "~/.openclaw/workspace-research",
      },
    ],
  },
  "bindings": [
    { "agentId": "daily", "match": { "channel": "whatsapp" } },
    { "agentId": "coding", "match": { "channel": "discord" } },
    { "agentId": "research", "match": { "channel": "telegram" } },
  ],
}
```

Each OpenClaw agent sees a different BitRouter model alias, but underneath, BitRouter handles all the provider routing, failover, firewall, and spend tracking.

---

## Appendix: Future Integrations (Planned)

These features align with the BitRouter product roadmap and will deepen the OpenClaw integration:

### Native OpenClaw Plugin

A first-party BitRouter plugin for OpenClaw that auto-configures the provider, syncs model catalogs, and surfaces spend data in the Control UI.

```bash
openclaw plugins install @bitrouter/openclaw
openclaw plugins enable bitrouter
# Auto-discovers BitRouter at localhost:8787
# Models appear in /model list instantly
```

### Real-Time Spend in OpenClaw

Surface BitRouter's per-request spend data in OpenClaw's session metadata, so users see cost per conversation:

```
> How much did this chat cost?
This session used 3 turns:
  Turn 1: bitrouter/smart → claude-sonnet-4  │ $0.012
  Turn 2: bitrouter/smart → claude-sonnet-4  │ $0.008
  Turn 3: bitrouter/fast  → gpt-4.1-mini    │ $0.001
  Total: $0.021
```

### Agent Heartbeat Cost Monitoring

OpenClaw's heartbeat system could query BitRouter spend to alert when an agent is burning through its budget:

```jsonc
{
  "agents": {
    "defaults": {
      "heartbeat": {
        "every": "1h",
        // Future: BitRouter budget check in heartbeat
      },
    },
  },
}
```

### Shared Tool Discovery

OpenClaw skills + BitRouter tools unified: the agent discovers tools from both surfaces through a single interface. BitRouter's MCP gateway feeds into OpenClaw's tool resolution, so MCP tools configured in BitRouter appear as native OpenClaw tools.

### ACP Agent Routing Through BitRouter

BitRouter's A2A agent management could complement OpenClaw's ACP system — agents spawned via `/acp spawn` route their LLM calls through BitRouter for consistent firewall, spend tracking, and model selection across all sub-agents.

---

## Quick Reference

| Command                                                       | What it does                  |
| ------------------------------------------------------------- | ----------------------------- |
| `bitrouter serve`                                             | Start BitRouter in foreground |
| `bitrouter start`                                             | Start BitRouter as daemon     |
| `bitrouter reload`                                            | Hot-reload config             |
| `bitrouter status`                                            | Show server status + spend    |
| `bitrouter models list`                                       | List available models         |
| `bitrouter tools list`                                        | List available tools          |
| `bitrouter key sign --wallet W --models M --budget B --exp E` | Generate agent JWT            |
| `bitrouter key revoke --id ID`                                | Revoke an agent key           |
| `bitrouter route add NAME PROVIDER:MODEL`                     | Add dynamic route             |
| `openclaw gateway restart`                                    | Restart OpenClaw gateway      |
| `openclaw models status`                                      | Show resolved model + auth    |
| `/model bitrouter/smart`                                      | Switch model in chat          |

---

_Built with [BitRouter](https://bitrouter.ai) and [OpenClaw](https://openclaw.ai). Apache 2.0 + MIT._
