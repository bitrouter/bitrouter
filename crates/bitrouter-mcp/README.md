# bitrouter-mcp (experimental)

> ⚠️ **Not stable. Use at your own risk.**
>
> This crate is experimental. Its CLI flags, tool schemas, transports, and
> public API may change — or break — without notice between releases. Do not
> depend on it in production. Feedback and issues welcome.

An **origin** Model Context Protocol (MCP) server for BitRouter: it exposes
BitRouter's *own* capabilities as MCP tools to any MCP-capable client (Claude
Code, Claude Desktop, Cursor, …).

This is **not** the same as BitRouter's MCP *gateway* (the `bitrouter tools`
subcommand and the `mcp_servers` config), which *proxies* upstream MCP servers.
This crate makes BitRouter itself the server.

## Tools

The handler assembles its tool set from one set of schemas. The completion
tools are HTTP-safe and always present. Host-bound tools — routing
introspection and skills — are injected app-side through the crate's
capability ports (`RoutingQuery`, `SkillsQuery`) so the crate itself stays
substrate-free, and they are wired on **stdio only**: they read the serving
machine's own routing table and skill library, which has no meaning on a
multi-tenant HTTP transport.

| Tool | Wired on | Description |
|------|----------|-------------|
| `complete` | all | Route a completion through BitRouter and return the full result |
| `list_models` | all | List models routable through BitRouter |
| `status` | all | Report BitRouter status (local: liveness/models/providers; cloud: credit balance) |
| `route_preview` | stdio + local | Preview how a model/prompt would route (provider chain, policy decision, cost) |
| `skills_search` | stdio + skills | Search installed BitRouter skills by name/description |
| `skills_get` | stdio + skills | Fetch a skill's frontmatter + body |

Only wired capabilities register their tools, so an HTTP client never sees —
or can call — the host-bound routing and skills tools.

## Transports & backends

One tool definition is served over two transports (built on
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)):

- **stdio** → the local BYOK daemon at `http://127.0.0.1:4356` (BYOK).
- **streamable HTTP** (mounted at `/mcp-control`) → BitRouter Cloud at
  `https://api.bitrouter.ai`.

`--backend` selects `local`, `cloud`, or `skills` (defaults: stdio→local,
http→cloud). The host-bound tools in the table above ride stdio only.

## Usage

```bash
# stdio (local daemon backend) — what an MCP client launches
bitrouter mcp serve

# streamable HTTP (cloud backend)
bitrouter mcp serve --transport http --bind 127.0.0.1:4357

# write the client config block (or omit --config to print it)
bitrouter mcp install --client claude
bitrouter mcp install --client cursor
```

`bitrouter mcp serve --help` lists every flag (`--transport`, `--backend`,
`--local-url`, `--cloud-url`, `--token`, `--bind`).

A typical MCP client config entry (stdio):

```json
{
  "mcpServers": {
    "bitrouter": { "command": "bitrouter", "args": ["mcp", "serve"] }
  }
}
```

## Layout

```
mcp/
├── src/
│   ├── lib.rs          # serve() / install() entry points, Transport / BackendKind
│   ├── backend/        # Backend trait + LocalBackend / CloudBackend (thin reqwest)
│   ├── capabilities/   # port traits: RoutingQuery, SkillsQuery, SkillCatalog + schemas
│   ├── error.rs        # ToolError — the substrate-free error a port returns
│   ├── server.rs       # rmcp handler, named router blocks, Builder, serving
│   └── install.rs      # render / merge client config blocks
└── tests/              # stdio handshake + HTTP integration tests
```

## More

The `/bitrouter` Agent Skill is the source of truth for operating the server:
see [`../skills/bitrouter/references/mcp-server.md`](../skills/bitrouter/references/mcp-server.md)
for the full reference (tools, transports, backends, auth, roadmap).
