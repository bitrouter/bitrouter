# Providers & routing

How to configure `providers:`, `models:`, `mcp_servers:`, and `agents:` in `bitrouter.yaml`. Reflects the v1 schema in `crates/bitrouter-sdk/src/config/mod.rs` — fields and strategies not listed here do not exist.

## Built-in providers

Seven providers ship in the binary. Each has a baked-in `api_base`, `api_protocol`, and credential env var. Listing them with an empty body in `bitrouter.yaml` enables them — built-in defaults fill the rest.

| Provider id | Env var | Auth | Notes |
|---|---|---|---|
| `openai` | `OPENAI_API_KEY` | Bearer | Chat Completions default; Responses API also live |
| `anthropic` | `ANTHROPIC_API_KEY` | Header `x-api-key` | Messages API |
| `google` | `GEMINI_API_KEY` | Header `x-goog-api-key` | Generative Language API — **not** `GOOGLE_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` | Bearer | Forwards every OpenRouter model |
| `github-copilot` | — (OAuth) | Device flow | `bitrouter login github-copilot`; per-model protocol map (Claude → Anthropic, gpt-5.x-codex → Responses, rest → Chat) |
| `opencode-zen` | `OPENCODE_ZEN_API_KEY` | Bearer | Curated models, per-family protocol routing |
| `opencode-go` | `OPENCODE_ZEN_API_KEY` (shared) | Bearer | Low-cost subscription tier — same credential as Zen |

Zero-config mode auto-enables every built-in whose env var is present. A built-in without its credential gets `active: false` and falls out of the routing table — startup still succeeds.

## Minimal config

```yaml
# bitrouter.yaml — the smallest file that does useful work
server:
  listen: "127.0.0.1:4356"
  skip_auth: true

providers:
  openai: {}        # uses OPENAI_API_KEY
  anthropic: {}     # uses ANTHROPIC_API_KEY
  google: {}        # uses GEMINI_API_KEY

inherit_defaults: true
```

`inherit_defaults: true` (the workspace default) is what makes `openai: {}` work — it fills `api_base`, `api_protocol`, the model catalog, and the env-resolved `api_key` from the built-in entry. Set it to `false` only when you want to override everything explicitly.

## Custom OpenAI-compatible providers

Anything that speaks OpenAI Chat Completions on a known base URL fits here. No built-in needed.

```yaml
providers:
  ollama:
    api_base: "http://localhost:11434/v1"
    api_protocol: { "*": openai }     # default; can be omitted
    models:
      - { id: "llama3.1:70b" }
      - { id: "codellama:34b" }

  groq:
    api_base: "https://api.groq.com/openai/v1"
    api_key: "${GROQ_API_KEY}"
    auto_discover: true               # pull /v1/models at startup + reload

  azure:
    api_base: "https://YOUR_RESOURCE.openai.azure.com"
    api_key: "${AZURE_OPENAI_KEY}"
    # Azure speaks Chat Completions on the same base; deployment names go in `models`
    models:
      - { id: "gpt-4o", upstream_id: "gpt4-deployment" }
```

`api_protocol` accepts a glob-prefix pattern map: `{ "claude-*": anthropic, "gpt-5.5-codex": responses, "*": openai }` is valid and matches most-specific-first.

## Multi-account (failover or balance)

When you hold two subscriptions to the same upstream, expand the provider into accounts:

```yaml
providers:
  opencode-go:
    account_strategy: failover        # or "balance"
    accounts:
      - { api_key: "${OPENCODE_GO_KEY_A}", label: primary }
      - { api_key: "${OPENCODE_GO_KEY_B}", label: backup }
```

- `failover` (default): try `primary` first; drop to `backup` on retryable failure (5xx / 429 / timeout / credit-exhaustion).
- `balance`: process-random rotation so each account roughly shares load; the remaining accounts still act as failover targets for that request.

Per-account `api_base` is allowed for multi-region setups — empty inherits the provider's.

## Rate limits

```yaml
providers:
  openai:
    rate_limits:
      "gpt-4*":      { requests_per_minute: 60,  tokens_per_minute: 90000 }
      "gpt-4o-mini": { requests_per_minute: 200, tokens_per_minute: 200000 }
```

Glob-prefix patterns, same precedence as `api_protocol`. Each `(provider, pattern)` bucket gets an independent window.

## Tags & routing

Routing prefs filter providers by tag — `require_tags: [cheap]` keeps only tagged providers in scope:

```yaml
providers:
  openai:
    tags: [cloud, paid]
  ollama:
    tags: [local, free, cheap]
```

## Derives (provider inheritance)

```yaml
providers:
  openai:
    api_key: "${OPENAI_API_KEY}"
  openai-dev:
    derives: openai
    api_key: "${OPENAI_DEV_KEY}"     # only the credential differs
```

`derives` flows `api_protocol`, `rate_limits`, `models`, `tags`, and `auto_discover` from the named provider into empty fields of this one.

## Virtual models

The top-level `models:` block defines named models that don't map 1:1 to a single provider. The full schema is broader than this skill covers — see `crates/bitrouter-sdk/src/config/mod.rs::VirtualModel`. The pattern that 90% of users want is a simple alias:

```yaml
models:
  fast:
    # routes to whichever provider declares this id first
    upstream_id: "claude-haiku-4-5"
  cheap-coder:
    upstream_id: "opencode-go/glm-5.1"
```

For richer routing (priority chains, splits), check the v1 docs at <https://bitrouter.ai> — the schema is richer than what was previously documented in the agent-skills repo.

## MCP servers

```yaml
mcp_servers:
  ctx7:
    name: ctx7
    transport:
      type: http
      url: https://mcp.context7.com/mcp
      headers:
        Authorization: "Bearer ${CTX7_TOKEN}"

  git:
    name: git
    transport:
      type: stdio
      command: uvx
      args: ["mcp-server-git"]
```

Once configured, `POST /mcp/<name>` proxies JSON-RPC through. Inspect with `bitrouter tools list` / `bitrouter tools status` / `bitrouter tools discover <name>`.

## Server tools (router-executed)

Attach an MCP server's tools to LLM requests: BitRouter advertises them to the model, executes the model's calls to them itself, and loops until the model stops calling them — all inside one client response. The named servers must also be declared under `mcp_servers:` above.

```yaml
server_tools:
  mcp_servers: [ctx7, git]   # ids from mcp_servers: above
  max_iterations: 10         # optional; max tool rounds per request (default 10)
```

Tool names are prefixed (`<name>__<tool>`, or the server's `tool_prefix`) so they can't collide with the caller's own tools. Empty/unset leaves the pipeline single-shot. This is the inverse of `bitrouter mcp serve` (which makes BitRouter an MCP *server*): here BitRouter is an MCP *client* consuming those tools inside the request loop.

## ACP agents

```yaml
agents:
  claude:
    name: claude
    transport:
      type: stdio
      command: npx
      args: ["-y", "@zed-industries/claude-code-acp@latest"]

  codex:
    name: codex
    transport:
      type: stdio
      command: npx
      args: ["-y", "@zed-industries/codex-acp@latest"]
```

Editors spawn the bridge with `bitrouter agent-proxy <id>`. `bitrouter agents list` shows the bundled catalog (use `bitrouter agents install <id>` to print a paste-ready stub).

## Apply changes

```bash
bitrouter reload                      # hot-reload running daemon
# or SIGHUP to the daemon pid — same effect
# or `bitrouter restart` for a clean cycle
```

`reload` also re-pushes the current shell's provider env vars into the daemon, so an `export OPENAI_API_KEY=new...; bitrouter reload` sequence rotates the key live.

## What's not supported (don't suggest these)

Older versions of this skill mentioned features that **do not exist** in the v1 schema:

- `cost_limits` (daily/monthly USD caps per provider) — not parsed.
- `strategy: conditional` with `prompt_tokens` rules — not parsed.
- `strategy: least_cost` — not parsed.
- `safety_settings`, `features.computer_use`, `features.json_mode` provider blocks — not parsed.
- `bitrouter providers add/remove/test/stats/export/import` subcommands — only `list` and `use` (no-op) exist.
- `bitrouter config validate/reload/show` — config validation happens on load; use `bitrouter reload` for the daemon.
