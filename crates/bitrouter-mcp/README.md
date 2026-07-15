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

The handler assembles two **profiles** from one set of tool schemas. The
public profile (`--backend local|cloud`) exposes only the completion tools and
is HTTP-safe. The orchestrator profile (`--backend fleet`, stdio-only, injected
by the TUI) is the *union*: the completion tools **plus** the fleet, cost,
routing, skills, and human-bridge tools, injected app-side through the crate's
capability ports (`Fleet`, `CostQuery`, `RoutingQuery`, `SkillsQuery`,
`HumanBridge`) so the crate itself stays substrate-free.

| Tool | Profile | Description |
|------|---------|-------------|
| `complete` | both | Route a completion through BitRouter and return the full result |
| `list_models` | both | List models routable through BitRouter |
| `status` | both | Report BitRouter status (local: liveness/models/providers; cloud: credit balance) |
| `spawn_subagent` | orchestrator | Spawn a worktree-isolated ACP subagent, send the task, block until the turn ends |
| `prompt_subagent` | orchestrator | Send a follow-up prompt to a running subagent |
| `subagent_status` | orchestrator | Fleet snapshot, or one subagent's status |
| `subagent_diff` | orchestrator | The subagent's diff against its spawn base |
| `apply_subagent` | orchestrator | Apply the subagent's diff onto the base tree, uncommitted (human-gated) |
| `merge_subagent` | orchestrator | Merge the subagent's branch into the base repo (human-gated) |
| `close_subagent` | orchestrator | Shut the subagent down (worktree retained) |
| `fleet_cost` | orchestrator | BitRouter spend snapshot (today + all-time totals) |
| `route_preview` | orchestrator | Preview how a model/prompt would route (provider chain, policy decision, cost) |
| `skills_search` | orchestrator | Search installed BitRouter skills by name/description |
| `skills_get` | orchestrator | Fetch a skill's frontmatter + body to hand to a subagent |
| `notify_human` | orchestrator | Send the supervising human a one-line TUI notice |
| `request_attach` | orchestrator | Ask the human to attach to a subagent's pane |
| `request_review` | orchestrator | Flag a subagent's work for the human's review queue |

Only wired capabilities register their tools, so a public client never sees —
or can call — the mutating fleet tools (or the routing/skills/human ones).

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
│   ├── capabilities/   # port traits: Fleet, CostQuery, RoutingQuery, SkillsQuery, HumanBridge + schemas
│   ├── error.rs        # ToolError — the substrate-free error a port returns
│   ├── server.rs       # rmcp handler, named router blocks, Builder, serving
│   └── install.rs      # render / merge client config blocks
└── tests/              # stdio handshake + HTTP integration tests
```

## More

The `/bitrouter` Agent Skill is the source of truth for operating the server:
see [`../skills/bitrouter/references/mcp-server.md`](../skills/bitrouter/references/mcp-server.md)
for the full reference (tools, transports, backends, auth, roadmap).
