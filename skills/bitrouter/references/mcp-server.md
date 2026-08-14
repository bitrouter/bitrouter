# BitRouter Origin MCP Server

BitRouter ships a built-in MCP server (`bitrouter mcp serve`) that exposes BitRouter's own routing tools. This is the **origin** server — it is **not** the MCP gateway. The two are entirely separate:

| Surface | Route / binding | Purpose |
|---|---|---|
| MCP gateway | `/mcp` on the daemon (`:4356`) | Proxies upstream MCP servers declared in `bitrouter.yaml` |
| Origin MCP server | stdio *or* `/mcp-control` on a separate port (`:4357`) | Exposes BitRouter's own `complete`/`list_models`/`status` tools |

---

## Skills over MCP (SEP-2640)

BitRouter is a skills **server** and **gateway**. It is not a skills *host*: it
never decides what enters a model's context and never honours a skill's
`allowed-tools`. It is not an *installer* either — BitRouter reads the
installed-skills directory, and something else populates it (`npx skills add`,
or the Claude Code / Codex plugin marketplaces). Handle transport, not
distribution.

**As a server** (`mcp serve --backend skills`) it publishes the skills installed
under the current directory as `skill://<name>/SKILL.md`, answering
`skills/list`, `skills/get`, `resources/list`, and `resources/read`. Each entry
carries every file of the skill with a `sha256:` digest.

**As a gateway** (`POST /mcp`) it merges the skills of every upstream marked
`aggregate: true`. Upstream URIs are namespaced under the member's configured
server name, because two upstreams may legitimately publish the same URI:

```text
upstream   skill://refunds/SKILL.md        (server "acme")
gateway    skill://acme/refunds/SKILL.md
```

Reads of an aggregated skill route back to that member alone — a skill served
by one upstream can never cause a read against another.

Two limits worth knowing before you rely on this:

- **The gateway is not a security boundary.** SEP-2640 digests are unsigned and
  come from the same server as the content, and any intermediary can rewrite
  both together. BitRouter *is* such an intermediary. A digest match proves the
  listing and the bytes agree, nothing more.
- **Remote catalogs are daemon-scoped, not caller-scoped.** An upstream's
  credentials come from `bitrouter.yaml` and are shared by every caller of the
  daemon, so a remote server cannot serve different skills to different users
  through BitRouter. Private per-user catalogs are not supported yet.

Only `skill://` URIs are aggregated. An upstream serving skills under another
scheme is reachable on its direct route (`POST /mcp/{server}`), and the
aggregate reports those entries as skipped under `_bitrouterErrors`.

---

## Commands

### `bitrouter mcp serve`

Starts the origin MCP server.

| Flag | Default | Description |
|---|---|---|
| `--transport` | `stdio` | `stdio` or `http` |
| `--backend` | *(derived)* | `local`, `cloud`, or `skills`. Omit to auto-derive: `stdio`→`local`, `http`→`cloud`. `skills` is the **origin AgentSkills server** over the installed-skills root of the current directory, serving two surfaces: the `skills_search`/`skills_get` **tools** (callable by any MCP client today) and SEP-2640's `skills/list`/`skills/get` **methods** plus `resources/list`/`resources/read` over skill files. It is the `bitrouter_skills` server `bitrouter launch` injects into every harness that can take one. `skills` is **stdio-only** |
| `--local-url` | `http://127.0.0.1:4356` | Root URL of the local BitRouter daemon |
| `--cloud-url` | `https://api.bitrouter.ai` | Root URL of BitRouter Cloud |
| `--token` | *(env fallback)* | Cloud bearer token for **stdio→cloud only**; falls back to `BITROUTER_TOKEN` env var. Ignored when `--transport http` (multi-tenant PerCaller mode). |
| `--bind` | `127.0.0.1:4357` | HTTP bind address (ignored for stdio) |

**Examples:**

```bash
# stdio — local daemon, no auth needed
bitrouter mcp serve

# stdio — cloud backend, single token (--token / BITROUTER_TOKEN required)
bitrouter mcp serve --backend cloud --token brk_...

# HTTP — cloud backend, multi-tenant (clients supply their own Bearer header; no --token on the server)
bitrouter mcp serve --transport http

# HTTP — local backend override (no auth middleware)
bitrouter mcp serve --transport http --backend local --local-url http://127.0.0.1:4356

# stdio — origin AgentSkills server over ./skills installs.
# Serves both the tool surface (skills_search/skills_get) and the SEP-2640
# method surface (skills/list, skills/get, resources/list, resources/read).
bitrouter mcp serve --backend skills
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

## Tools

The `complete` / `list_models` / `status` trio is always served. Over **stdio
against the local daemon** the server also wires `route_preview` (below), which
resolves against *this machine's* config and control socket — that is why it is
never served over the multi-tenant HTTP transport. `--backend skills` serves a
different profile entirely: `skills_search` / `skills_get` plus the SEP-2640
methods, and no completion tools.

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
  { "id": "openai/gpt-4o", "provider": "openai" },
  { "id": "anthropic/claude-3-5-sonnet", "provider": "anthropic" }
]
```

### `status`

Report BitRouter's health and credit state. The shape differs by backend.

**Arguments:** none.

**Local backend response:**

A successful response means the daemon is reachable; an error is returned otherwise.

```json
{
  "listen": "127.0.0.1:4356",
  "models": 14,
  "providers": [
    { "id": "openai" },
    { "id": "anthropic" }
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

### `route_preview`

*(stdio → local only)* Preview how BitRouter would route a model/prompt: the
resolved provider chain, the policy decision, and a cost estimate. Read-only —
nothing is sent upstream. Wired best-effort: when the config can't be read the
tool is simply not advertised, and `mcp serve` still starts.

---

## Transport ↔ Backend pairing

| Transport | Default backend | Auth mode | MCP endpoint |
|---|---|---|---|
| `stdio` | local | none (daemon uses `skip_auth`) | n/a — stdin/stdout |
| `http` | cloud | **PerCaller** — each request carries its own `Bearer` | `http://<bind>/mcp-control` |

You can override the default backend with `--backend local|cloud`. The HTTP transport always mounts at `/mcp-control` (not `/mcp`, which is the gateway route).

---

## Cloud auth

### stdio → cloud (single-tenant)

When running with `--transport stdio --backend cloud`, a single bearer token is used for every request. Supply it via `--token <value>` or the `BITROUTER_TOKEN` environment variable:

- A `brk_*` API key minted on the dashboard, or
- The access token written by `bitrouter cloud login` (RFC 8628 device flow).

```bash
# stdio — cloud backend, single token
bitrouter mcp serve --backend cloud --token brk_...
# or: export BITROUTER_TOKEN=brk_...; bitrouter mcp serve --backend cloud
```

### http → cloud (multi-tenant, PerCaller)

When running `--transport http --backend cloud` (the default for HTTP), the server is **multi-tenant**: each MCP client supplies its own `Authorization: Bearer` header, which is forwarded per-request to `api.bitrouter.ai/v1`. The server itself holds **no** token — `--token`/`BITROUTER_TOKEN` is ignored in this mode.

An edge middleware rejects any request that lacks a `Bearer` Authorization header with `401 Unauthorized` before it reaches the MCP handler. The cloud backend validates the token's validity; the server only checks presence.

```bash
# HTTP multi-tenant — no --token needed on the server side
bitrouter mcp serve --transport http
```

Each client configures its own credential in its remote-server block. Example for Claude Code / Cursor:

```json
{
  "mcpServers": {
    "bitrouter": {
      "url": "http://127.0.0.1:4357/mcp-control",
      "headers": {
        "Authorization": "Bearer brk_..."
      }
    }
  }
}
```

Different clients can use different `brk_*` keys (or their own `bitrouter cloud login` access tokens) against the same running server instance.

### http → local

When the local backend is forced (`--backend local`), no auth middleware is applied — the local daemon's own `skip_auth` setting governs access. Because this exposes the BYOK daemon (running on your own provider keys) with no MCP-layer auth, `serve` **refuses a non-loopback `--bind`** in this mode: bind a loopback address (e.g. `127.0.0.1:4357`), or use `--backend cloud`, which requires `Authorization`.

---

## Deferred / roadmap

The following are explicitly **not** in the current release:

- **Admin / mutating tools** — e.g. key rotation, provider toggle, policy edit. Current release is read/inference only.
- **Tier allowlist filter** — restricting `list_models` output to models within a user's plan tier.
- **Native in-client browser OAuth (Item B)** — MCP-native browser OAuth (RFC 8414 Authorization Server Metadata + RFC 7591 Dynamic Client Registration) is deferred. BitRouter Cloud's authorization server has no DCR endpoint, and its `/oauth/authorize` flow requires a `brk_*` principal. Enabling native browser OAuth requires cross-repo cloud work before the MCP server can participate. In the meantime, clients use pre-minted `brk_*` keys or device-flow tokens in the `Authorization: Bearer` header as shown above.
