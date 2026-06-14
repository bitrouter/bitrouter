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
    name: memory                   # required field (may differ from the map key)
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
`tools/call`s for the namespaced tools (`memwal_remember`,
`memwal_remember_bulk`, `memwal_recall`, `memwal_analyze`, `memwal_restore` —
matched against the live relayer's tool set, not just the docs):

- Unrestricted agent (`namespaces: ["*"]`): never modified.
- Scoped agent naming an **allowed** namespace: passes through.
- Scoped agent naming a **disallowed** namespace: rejected (401).
- Scoped agent **omitting** namespace: its `default` (or `default_namespace`)
  is injected.
- **Unknown** agent (missing/empty header): may not name a namespace
  (rejected); an omitted namespace gets `default_namespace`.

## 4. Always-on memory (forced recall + instructed remember)

By default memory is opt-in: the model decides whether to call the memory tools.
To make **every** agent/subagent turn use memory, add an `always` block under
`plugins.bitrouter-memory`:

```yaml
plugins:
  bitrouter-memory:
    server: memory
    default_namespace: shared
    always:
      enabled: true
      # recall_tool: memwal_recall          # optional override (unprefixed)
      # remember_instruction: "...{remember}..."   # optional; {remember} is
      #                                            # replaced with the prefixed
      #                                            # remember tool name
    agents:
      researcher:
        namespaces: ["research"]
        default: research
```

When `enabled: true`:

- The memory server is **auto-wired into the server-side tool loop** — you do
  *not* need to also list it under `server_tools.mcp_servers`. Its tools are
  injected into every LLM request.
- **Recall is forced** as the first tool call of each turn (`tool_choice` pins
  the prefixed `memwal_recall`); the loop then reverts to the caller's normal
  tool choice so the model can answer and persist.
- **Remember is instructed**: a system-prompt line (mandating a call to the
  prefixed `memwal_remember` before the turn ends) is prepended to every request.
- Per-agent **namespace scoping still applies** on this `/v1` path: the
  `x-bitrouter-agent` header is now propagated from the inbound request onto the
  router-executed memory `tools/call`, so the same Strategy A rules above scope
  it. Send `x-bitrouter-agent: <agent-id>` on the inbound request.

Absent / `enabled: false` ⇒ behaves exactly as before (memory stays opt-in).

### Agents reached over ACP (`agent-proxy`)

The ACP pipeline is pure JSON-RPC routing of an opaque external agent —
BitRouter cannot force `tool_choice` or inject a system prompt into another
agent's own model loop. To get always-on memory for an ACP agent, configure
that agent to use BitRouter's `/v1` endpoint as its **model provider** and to
send `x-bitrouter-agent: <agent-id>`. Every model turn it takes is then subject
to the forced recall + instructed remember + per-agent scoping above. There is
no BitRouter-side enforcement on the ACP frames themselves.

## Trust boundary

The `x-bitrouter-agent` header is client-supplied: a subagent that can forge it
can claim another agent's scope. Strategy A assumes the orchestrator controls
the header each subagent presents. For relayer-enforced, bypass-proof isolation
see the Strategy B note in
`docs/superpowers/plans/2026-06-13-mcp-memory-scoping.md`.
