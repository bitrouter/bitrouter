# BitRouter Origin MCP Server

BitRouter ships a built-in MCP server (`bitrouter mcp serve`) that exposes BitRouter's own routing tools. This is the **origin** server — it is **not** the MCP gateway. The two are entirely separate:

| Surface | Route / binding | Purpose |
|---|---|---|
| MCP gateway | `/mcp` on the daemon (`:4356`) | Proxies upstream MCP servers declared in `bitrouter.yaml` |
| Origin MCP server | stdio *or* `/mcp-control` on a separate port (`:4357`) | Exposes BitRouter's own `complete`/`list_models`/`status` tools |

---

## Commands

### `bitrouter mcp serve`

Starts the origin MCP server.

| Flag | Default | Description |
|---|---|---|
| `--transport` | `stdio` | `stdio` or `http` |
| `--backend` | *(derived)* | `local` or `cloud`. Omit to auto-derive: `stdio`→`local`, `http`→`cloud` |
| `--local-url` | `http://127.0.0.1:4356` | Root URL of the local BitRouter daemon |
| `--cloud-url` | `https://api.bitrouter.ai` | Root URL of BitRouter Cloud |
| `--token` | *(env fallback)* | Cloud bearer token; falls back to `BITROUTER_TOKEN` env var |
| `--bind` | `127.0.0.1:4357` | HTTP bind address (ignored for stdio) |

**Examples:**

```bash
# stdio — local daemon, no auth needed
bitrouter mcp serve

# HTTP — cloud backend, bearer token
bitrouter mcp serve --transport http --token brk_...

# HTTP — local backend override
bitrouter mcp serve --transport http --backend local --local-url http://127.0.0.1:4356

# token from env
export BITROUTER_TOKEN=brk_...
bitrouter mcp serve --transport http
```

### `bitrouter mcp install`

Prints or merges the `mcpServers` JSON config block for a given client.

| Flag | Default | Description |
|---|---|---|
| `--client` | `claude` | `claude` or `cursor` |
| `--config` | *(stdout)* | Path to an existing client config JSON file; merges non-destructively when provided |

**Examples:**

```bash
# print to stdout
bitrouter mcp install --client claude

# merge into Claude Desktop config (existing servers are preserved)
bitrouter mcp install --client claude \
  --config ~/Library/Application\ Support/Claude/claude_desktop_config.json
```

The generated block (identical for both clients today):

```json
{
  "mcpServers": {
    "bitrouter": { "command": "bitrouter", "args": ["mcp", "serve"] }
  }
}
```

---

## Tools (3)

### `complete`

Route a completion through BitRouter and return the full result.

**Arguments:**

| Arg | Type | Required | Description |
|---|---|---|---|
| `model` | string | yes | Routable model name (e.g. `openai/gpt-4o`), from `list_models` |
| `messages` | array | yes | Chat messages, OpenAI shape: `[{"role":"user","content":"…"}]` |
| `max_tokens` | integer | no | Token ceiling |
| `temperature` | number | no | Sampling temperature |
| `system` | string | no | System prompt |

**Response (JSON text):**

```json
{
  "content": "…",
  "model": "openai/gpt-4o",
  "usage": { "input_tokens": 12, "output_tokens": 34 },
  "finish_reason": "stop"
}
```

### `list_models`

List all models routable through the connected backend.

**Arguments:** none.

**Response (JSON text, array):**

```json
[
  { "id": "openai/gpt-4o", "provider": "openai", "active": true },
  { "id": "anthropic/claude-3-5-sonnet", "provider": "anthropic", "active": true }
]
```

### `status`

Report BitRouter's health and credit state. The shape differs by backend.

**Arguments:** none.

**Local backend response:**

```json
{
  "running": true,
  "listen": "127.0.0.1:4356",
  "models": 14,
  "providers": [
    { "id": "openai", "active": true },
    { "id": "anthropic", "active": false }
  ]
}
```

**Cloud backend response:**

```json
{
  "available_micro_usd": 950000,
  "balance_micro_usd": 1000000,
  "pending_micro_usd": 50000
}
```

---

## Transport ↔ Backend pairing

| Transport | Default backend | Auth needed? | MCP endpoint |
|---|---|---|---|
| `stdio` | local | no (daemon uses `skip_auth`) | n/a — stdin/stdout |
| `http` | cloud | yes — bearer token | `http://<bind>/mcp-control` |

You can override the default backend with `--backend local|cloud`. The HTTP transport always mounts at `/mcp-control` (not `/mcp`, which is the gateway route).

---

## Cloud auth (v1)

v1 uses a bearer token the user already holds, either:

- A `brk_*` API key minted on the dashboard, or
- The access token written by `bitrouter auth login` (RFC 8628 device flow).

Pass the token via `--token <value>` or the `BITROUTER_TOKEN` environment variable. In-client browser OAuth (so an MCP client can mint its own token without a pre-existing credential) is deferred to a later release.

---

## Deferred / roadmap

The following are explicitly **not** in v1:

- **Admin / mutating tools** — e.g. key rotation, provider toggle, policy edit. v1 is read/inference only.
- **Tier allowlist filter** — restricting `list_models` output to models within a user's plan tier.
- **Native in-client OAuth** — today the user must mint a `brk_*` key or run `bitrouter auth login` before the MCP server starts.
- **Per-caller multi-tenant bearer forwarding** — a single-tenant token is forwarded; per-caller isolation is deferred.
