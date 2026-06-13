# Walrus Memory via the MCP gateway (per-agent namespace scoping)

BitRouter fronts a single Walrus Memory MCP server and enforces per-agent
namespace isolation at the gateway (Strategy A).

## 1. Configure the upstream (one shared delegate credential)

Provision once on any machine:

```bash
npx -y @mysten-incubation/memwal-mcp login --prod
```

Lift `delegatePrivateKey` and `accountId` from `~/.memwal/credentials.json`
into `mcp_servers` in `bitrouter.yaml`:

```yaml
mcp_servers:
  memory:
    transport:
      type: http
      url: https://relayer.memory.walrus.xyz/api/mcp
      headers:
        Authorization: "Bearer <delegatePrivateKey>"   # secret — keep out of VCS
        x-memwal-account-id: "<accountId>"
    aggregate: true
    tool_prefix: "memory__"
```

The bearer token is API-key-equivalent. Do not commit it.

## 2. Configure per-agent scopes

```yaml
plugins:
  bitrouter-memory:
    server: memory               # must match the mcp_servers key above
    default_namespace: shared    # injected when a scoped agent omits one
    agents:
      orchestrator:
        namespaces: ["*"]        # unrestricted — full access, never clamped
      researcher:
        namespaces: ["research"]
        default: research
      writer:
        namespaces: ["drafts", "shared"]
        default: drafts
```

When `plugins.bitrouter-memory` is absent, scoping is disabled (passthrough).

## 3. How agents are identified

Each request's agent identity comes from the `x-bitrouter-agent` header. The
orchestrator sets it when spawning a subagent. Rules applied to memory
`tools/call`s (`memwal_remember`, `memwal_recall`, `memwal_analyze`,
`memwal_restore`):

- Unrestricted agent (`namespaces: ["*"]`): never modified.
- Scoped agent naming an **allowed** namespace: passes through.
- Scoped agent naming a **disallowed** namespace: rejected (401).
- Scoped agent **omitting** namespace: its `default` (or `default_namespace`)
  is injected.
- **Unknown** agent (missing/empty header): may not name a namespace
  (rejected); an omitted namespace gets `default_namespace`.

## Trust boundary

The `x-bitrouter-agent` header is client-supplied: a subagent that can forge it
can claim another agent's scope. Strategy A assumes the orchestrator controls
the header each subagent presents. For relayer-enforced, bypass-proof isolation
see the Strategy B note in
`docs/superpowers/plans/2026-06-13-mcp-memory-scoping.md`.
