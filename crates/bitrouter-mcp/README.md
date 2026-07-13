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

| Tool | Description |
|------|-------------|
| `complete` | Route a completion through BitRouter and return the full result |
| `list_models` | List models routable through BitRouter |
| `status` | Report BitRouter status (local: liveness/models/providers; cloud: credit balance) |

## Transports & backends

One tool definition is served over two transports (built on
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)):

- **stdio** → the local BYOK daemon at `http://127.0.0.1:4356` (BYOK).
- **streamable HTTP** (mounted at `/mcp-control`) → BitRouter Cloud at
  `https://api.bitrouter.ai`.

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
│   ├── server.rs       # rmcp handler (the 3 tools) + stdio / HTTP serving
│   └── install.rs      # render / merge client config blocks
└── tests/              # stdio handshake + HTTP integration tests
```

## More

The `/bitrouter` Agent Skill is the source of truth for operating the server:
see [`../skills/bitrouter/references/mcp-server.md`](../skills/bitrouter/references/mcp-server.md)
for the full reference (tools, transports, backends, auth, roadmap).
