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

The handler assembles its tool set from one set of schemas. `complete` is
HTTP-safe and always present. Every other tool comes through one of the crate's
ports — `StatusQuery`, `ModelsQuery` and `RouteQuery` (in `actions/`),
`SkillsQuery` (in `capabilities/`) — so the crate itself stays substrate-free.
The host-bound ones are wired on **stdio only**: they read the serving machine's
own routing table and skill library, which has no meaning on a multi-tenant HTTP
transport. `status` and `list_models` are the exception to *that*: a backend
that genuinely knows the answer for its own deployment hands its port over
(`Backend::status_port` / `Backend::models_port`), which is how the HTTP profile
keeps them.

`status` and `list_models` are **actions**: one report type each
(`actions::status::StatusReport`, `actions::models::ModelsReport`) shared by the
MCP tool and the CLI leaf, listed in `actions::ACTIONS` and held there by a
guard test in `apps/bitrouter`. See
[`docs/ACTIONS_SPEC.md`](../../docs/ACTIONS_SPEC.md).

| Tool | Wired on | Description |
|------|----------|-------------|
| `complete` | all | Route a completion through BitRouter and return the full result |
| `list_models` | all | Every routable model with **all** the providers that can serve it — the fallback chain, not just the first hop. Optional `provider` filter. Returns the shared `actions::models::ModelsReport`, so the tool advertises an `output_schema` and `bitrouter models --json` is the same bytes. `resolved_via` distinguishes a running router's catalog (`live`) from a static-config projection (`config`); on stdio + local the app-injected port falls back to the latter, so the tool answers with no daemon running |
| `status` | stdio + local, any cloud | Report BitRouter status: liveness (pid, listen, models, providers, control socket) plus the spend position — `spend.spent` (money gone, a locally metered estimate whose `unpriced` count says how partial it is) on any deployment, `spend.limit` (money left) where a cap exists. Returns the shared `actions::status::StatusReport`, so the tool advertises an `output_schema` and a stopped daemon is `running: false` rather than a tool error |
| `route_preview` | stdio + local | Preview how a model/prompt would route: the `effective_model` the policy table selects (which can differ from `requested_model`), the provider chain, the decision behind it, and the first hop's per-token rates. Returns the shared `actions::route::RouteReport`, so the tool advertises an `output_schema` and its structured content is byte-identical to `bitrouter route --json`. Config is read per call, so an edited `bitrouter.yaml` needs no restart |
| `skills_search` | every stdio profile | Every skill on this machine — the project *and* user-global roots, all three conventional layouts — optionally narrowed by `query`. Returns the shared `actions::skills::SkillsReport`, so the tool advertises an `output_schema` and `bitrouter skills list --json` is the same bytes. A skill that cannot be loaded is listed with `valid: false` and a `problem`, rather than silently missing |
| `skills_get` | every stdio profile | One skill's frontmatter metadata and `SKILL.md` body, as the shared `actions::skills::SkillDetail` |

Only wired capabilities register their tools, so an HTTP client never sees —
or can call — the host-bound routing and skills tools. The skills tools ride the
transport rather than the backend: a stdio server is a subprocess of the caller
whose machine it is, so a `bitrouter mcp install`-ed client sees them.
`--backend skills` remains the narrow gateway-subprocess profile that carries
nothing else.

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
│   ├── actions/        # shared report types + their ports + the ACTIONS inventory
│   ├── backend/        # Backend trait + LocalBackend / CloudBackend (thin reqwest)
│   ├── capabilities/   # port traits: SkillsQuery, SkillCatalog + schemas
│   ├── error.rs        # ToolError — the substrate-free error a port returns
│   ├── server.rs       # rmcp handler, named router blocks, Builder, serving
│   └── install.rs      # render / merge client config blocks
└── tests/              # stdio handshake + HTTP integration tests
```

## More

The CLI reference carries the full flag/transport/backend/tool reference:
see [`docs/CLI.md` → *Origin MCP server*](../../docs/CLI.md#origin-mcp-server).
The `/bitrouter` Agent Skill (`skills/bitrouter/references/cli.md`) is the
agent-facing summary of the same surface.
