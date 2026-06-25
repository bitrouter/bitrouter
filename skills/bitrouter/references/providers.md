# Providers & routing

How to configure `providers:`, `models:`, `mcp_servers:`, and `agents:` in `bitrouter.yaml`. Reflects the v1 schema in `crates/bitrouter-sdk/src/config/mod.rs` — fields and strategies not listed here do not exist.

## Known providers

bitrouter already knows how to talk to these providers — their `api_base`,
`api_protocol`, and credential env var come from the **fetched provider
registry** (below), so listing one with an empty body in `bitrouter.yaml`
enables it and the registry fills the rest. The definitions are **not** vendored
into the binary; they are fetched at startup and disk-cached (see the registry
section). The one in-binary exception is the hosted `bitrouter` cloud gateway.

| Provider id | Env var | Auth | Notes |
|---|---|---|---|
| `openai` | `OPENAI_API_KEY` | Bearer | Chat Completions default; Responses API also live |
| `anthropic` | `ANTHROPIC_API_KEY` | Header `x-api-key` | Messages API |
| `google` | `GEMINI_API_KEY` | Header `x-goog-api-key` | Generative Language API — **not** `GOOGLE_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` | Bearer | Forwards every OpenRouter model |
| `github-copilot` | — (local OAuth) | Device flow | `bitrouter login github-copilot`; per-model protocol map (Claude → Anthropic, gpt-5.x-codex → Responses, rest → Chat) |
| `openai-codex` | — (local PKCE) | ChatGPT subscription | `bitrouter login openai-codex` |
| `opencode-zen` | `OPENCODE_ZEN_API_KEY` | Bearer | Per-family protocol routing |
| `opencode-go` | `OPENCODE_ZEN_API_KEY` (shared) | Bearer | Low-cost subscription tier — same credential as Zen |

Zero-config mode auto-enables every API-key provider whose env var is present;
an API-key provider without its credential gets `active: false` and falls out of
the routing table. Local-OAuth/PKCE providers (`github-copilot`, `openai-codex`)
are enabled by `bitrouter login`, not an env var. **First run with no network
and no cache**: the registry is empty, so only fully-specified local providers
and the in-binary `bitrouter` cloud gateway are available — the known-provider
shorthand needs one prior successful fetch. Startup still succeeds.

## Provider registry (catalog + priority)

BitRouter fetches the public provider registry
(`https://github.com/bitrouter/provider-registry`) at startup and on reload: a
curated, deterministic catalog of the providers above (their transport + auth),
the canonical models, and which providers serve them. It is fetched from the
generated `dist/` artifacts, disk-cached under
`$XDG_CACHE_HOME/bitrouter/provider-registry.json` (24h TTL, stale-fallback on a
network outage), and merged into the routing table. If a fetch fails the cache
is reused; with no cache (first run, offline) the registry is empty and only
locally-configured providers route. The merge routes a **canonical** model id
(e.g. `anthropic/claude-sonnet-4.6`) to a provider that serves it, translating
to that provider's own upstream id.

Rules:

- **Public providers only.** Every public registry provider is merged; only
  `private` ones (the pooled / invite-only entries, no public registration) are
  skipped. The registry classifies each provider by how a caller obtains
  access: `api_key` (a portable key), `local_oauth` / `local_pkce` (a local
  interactive login — e.g. `github-copilot`, `openai-codex`), or `private`.
- **Credential-gated.** An `api_key` provider becomes routable only when its key
  is present, read from the convention `${NAME}_API_KEY` (uppercased, hyphens →
  underscores — e.g. `DEEPSEEK_API_KEY`, `ZAI_CODING_PLAN_API_KEY`), or from the
  built-in's env var when the provider also has a built-in entry. No key ⇒ not
  enabled. Declare the provider explicitly with `api_key: ${MY_VAR}` to override
  the env-var name. A `local_oauth` / `local_pkce` provider is not env-gated —
  it activates after `bitrouter login <provider>`.
- **Full catalog via the sync channel.** A provider may declare an `auto_sync`
  feed (the channel the registry itself curates from). BitRouter reads the same
  channel to pull the provider's **full** catalog beyond the curated canonical
  subset: a `v1_models` feed (the gateways) is probed at `GET {api_base}/models`
  on startup; a `models_dev` feed pulls the provider's models from models.dev.
  The curated canonical models keep the highest route priority.
- **BitRouter Cloud serves everything.** When the `bitrouter` provider is
  active (env key or `bitrouter auth login`), it is populated with every model
  in the canonical list.

```yaml
registry:
  enabled: true            # default; set false (or inherit_defaults: false) to disable the merge
  url: "https://raw.githubusercontent.com/bitrouter/provider-registry/main/dist"
  provider_priority:       # default ladder, highest first
    - first-party-subscription
    - gateway-subscription
    - first-party-api
    - bitrouter-cloud
    - third-party-api
```

When several active providers serve the same canonical model, the auto-cascade
orders them by this `provider_priority` ladder (a provider's class comes from
the registry / built-in). Override per provider with `class:` or a numeric
`priority:` (lower = preferred) under `providers.<id>`.

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

### Model-backed server tools (advisor / sub-agent / fusion)

BitRouter can also expose server tools backed by *nested model completions*. Each is advertised only when the caller declares it (a provider-defined tool in the request `tools`, e.g. `{"type":"bitrouter:advisor"}`) and runs on a loop-less sub-pipeline metered to the caller.

```yaml
server_tools:
  advisor: true               # bitrouter:advisor — consult a stronger model mid-task
  subagent: true              # bitrouter:subagent — delegate a task to a worker model
  fusion:                     # bitrouter:fusion — multi-model panel + judge deliberation
    panel: [anthropic/claude-opus-4.8, openai/gpt-latest, google/gemini-pro]
    judge: anthropic/claude-opus-4.8
    # optional: outer_model, alias (default bitrouter/fusion), synthesizer, web_tools
```

Setting `fusion:` also enables the `bitrouter/fusion` model alias, which expands a request to the configured panel + judge. The judge *compares* the panel's answers (consensus / contradictions / partial coverage / unique insights / blind spots); the calling model writes the final answer from that analysis.

### Built-in web search (BYOK)

The `web_search` server tool gives *any* model routed through BitRouter a web search, served by a search backend you bring keys for. Advertised only when the caller declares `{"type":"bitrouter:web_search"}` (optionally with `backend` / `max_results` overrides). The model calls `web_search` with a `query`; BitRouter runs it and returns `{backend, answer?, results:[{url,title?,snippet?,content?,published?,score?}]}` — `answer` is present only for the answer-engine `native` backend.

```yaml
server_tools:
  web_search:
    max_results: 5             # optional default cap (caller may lower it)
    backends:                  # preference + failover order; first that resolves a key is default
      - kind: parallel         # HTTP, key from api_key or PARALLEL_API_KEY
      - kind: exa              # HTTP, key from api_key or EXA_API_KEY
      - kind: firecrawl        # HTTP, key from api_key or FIRECRAWL_API_KEY
      - kind: tavily           # HTTP, key from api_key or TAVILY_API_KEY
      - kind: native           # reuse a provider's NATIVE search for every model
        name: native           # backend id a caller pins (default "native")
        model: anthropic/claude-opus-4.8
        tool: { type: "anthropic:web_search_20250305" }
```

HTTP backends (`parallel` / `exa` / `firecrawl` / `tavily`) take an optional `api_key` (supports `${VAR}`) and `api_base`; a backend with no resolvable key is skipped. The `native` backend runs a nested completion (so it needs a routable model) — it forwards a provider's own search tool, making one provider's native web search usable from models that lack it.

### Built-in web fetch (BYOK)

The `web_fetch` server tool gives any model routed through BitRouter the ability to fetch and read a specific URL's full content, served by a BYOK extraction backend. Advertised only when the caller declares `{"type":"bitrouter:web_fetch"}` (optionally with `backend` / `max_content_tokens` overrides). The model calls `web_fetch` with a `url`; BitRouter fetches it and returns `{status, backend, url, title?, content, published?}`.

```yaml
server_tools:
  web_fetch:
    max_content_tokens: 25000   # default per-fetch content cap (caller may lower)
    backends:                   # preference / failover order
      - kind: exa               # POST /contents, key from api_key or EXA_API_KEY
      - kind: firecrawl         # POST /v2/scrape, key from api_key or FIRECRAWL_API_KEY
      - kind: tavily            # POST /extract, key from api_key or TAVILY_API_KEY
```

BYOK extraction happens on the provider's infrastructure (BitRouter does not dereference the URL itself), so the backends own the fetch safety surface.

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
