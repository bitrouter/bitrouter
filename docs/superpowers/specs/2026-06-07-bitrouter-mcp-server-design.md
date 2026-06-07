# BitRouter MCP Server — v1 Design

- **Status:** Draft for review
- **Date:** 2026-06-07
- **Issue:** [#526 — feat: MCP server for BitRouter under /mcp](https://github.com/bitrouter/bitrouter/issues/526)
- **Author:** Spikel (design discussion w/ Claude)

## 1. Summary

Add an **origin** Model Context Protocol (MCP) server for BitRouter so any
MCP-capable client (Claude Code, Claude Desktop, Cursor, …) can drive the proxy
as a first-class tool. It ships in two transports from **one tool definition**:

- **Local, over stdio** — talks to the user's BYOK daemon at `127.0.0.1:4356`.
- **Remote, over streamable HTTP** — a hostable endpoint (e.g.
  `mcp.bitrouter.ai`, also self-hostable) that talks to BitRouter Cloud at
  `api.bitrouter.ai/v1`.

v1 targets an agent that **uses** BitRouter (route completions, discover
models, check status). The architecture is built so an **administer** agent
tier can be added later without rework.

### Relationship to the existing MCP *gateway* (important)

BitRouter already ships MCP infrastructure pointing the **other direction** —
it is an MCP *gateway/aggregator*:

```
EXISTING (gateway):  client ──▶ BitRouter /mcp ──▶ upstream MCP servers
THIS ISSUE (origin): client ──▶ BitRouter MCP server exposing complete/list_models/status
```

- `crates/bitrouter-sdk/src/mcp/` — pipeline, `Executor` trait, `RmcpExecutor`
  (dials upstreams), `AggregatingExecutor`, `CachingExecutor`, `config_routing`,
  `transport`.
- Config already has `mcp: McpConfig` **and** `mcp_servers: HashMap<…>`.
- The daemon already serves streamable-HTTP MCP at `/mcp` (aggregate) and
  `/mcp/{id}` (per-server) via `server.rs::mcp_invoke`, spec `2025-11-25`.
- `rmcp` is already a workspace dependency, **pinned at v1.7**.

This design adds a **separate origin server**, not a built-in executor inside
that gateway. Rationale in §3.1. Three collisions are explicitly avoided:
the `/mcp` HTTP route (§4), the `mcp:` config key (§7), and the meaning of
"mcp" in the product (origin vs gateway — kept distinct in docs).

## 2. Goals / Non-Goals

### Goals
- One `rmcp` tool definition served over **both** stdio and streamable HTTP.
- A small, safe default tool surface identical across local and cloud backends.
- A tool registry whose tiers make the future admin surface purely additive.
- Reuse existing BitRouter auth end to end — no new auth-server code.
- No collision with the existing MCP gateway (route, config key, naming).

### Non-Goals (v1)
- Token-level streaming of completions (the *transport* is streamable; the
  `complete` tool returns a full result — see §8).
- Admin/mutating tools enabled by default (`reload`, `key_sign`,
  `policy_create` are defined but off; opt-in via `--enable-tool`).
- Interactive credential flows as MCP tools (`login`/`logout` stay CLI-only —
  browser PKCE / device code / stdin paste cannot be one tool call).
- A new `bitrouter.yaml` block (v1 is CLI-flag-driven; reserve `mcp_serve:`
  for later — see §7).

## 3. Architecture: two orthogonal axes

The design separates **transport** (how the MCP client connects) from
**backend** (where routing requests go). They combine freely.

```
Transport (rmcp 1.7):  stdio   |   streamable-http        ← `--transport`
Backend (trait):       Local(:4356)   |   Cloud(api.bitrouter.ai)
Tool registry:         tool = { name, tier, backends, handler }
                       tier ∈ { Core, ReadOnly, Admin }
                       enabled = Core + ReadOnly  (Admin opt-in via --enable-tool)
serve registers:       enabled ∩ supported-by-backend
```

Maps onto BitRouter's local/cloud duality:

```
  stdio            ────▶  LocalBackend
  (bitrouter mcp           → HTTP to 127.0.0.1:4356/v1/*
   serve --transport       → daemon control socket (read-only status)
   stdio)                  → BYOK; master_key passthrough if skip_auth=false

  streamable-http  ────▶  CloudBackend
  (bitrouter mcp           → HTTP to api.bitrouter.ai/v1/*
   serve --transport       → forwards caller's Authorization bearer (OAuth/brk_)
   http, self-hostable)    → NO lifecycle tools
```

### 3.1 Why a separate origin server (not a built-in gateway executor)

The gateway's `Executor` trait *could* host an in-process `BuiltinExecutor`
serving `tools/list`/`tools/call` from native Rust, mounted at `/mcp/bitrouter`.
That reuse path was considered and rejected:

1. **stdio is a hard requirement and the gateway pipeline is HTTP-only.** Its
   `server.rs::mcp_invoke` plumbing (session, Origin, SSE) exists only for the
   HTTP transport. The local case needs **stdio**, which the gateway does not
   serve — so reuse would force implementing the three tools **twice** (a
   `BuiltinExecutor` for HTTP *and* rmcp `#[tool]` fns for stdio).
2. **rmcp defines tools once and serves both transports.** One `#[tool_router]`
   → stdio *and* streamable-HTTP from a single source of truth.
3. **Different products.** The gateway proxies *other people's* tools; this
   server exposes *BitRouter's own* capabilities. Folding control tools into
   the `/mcp` aggregate would surprise clients aggregating upstreams.

What we **do** reuse: `rmcp` itself (already a dep) and the daemon
binary/runtime (no second process). The HTTP variant mounts on a **distinct
route** (§4) so it never collides with the gateway's `/mcp`.

### 3.2 Why a thin client backend (not an embedded SDK)

Embedding `bitrouter-sdk` only helps the local case; the remote case must
forward to Cloud regardless. So **both backends are thin reqwest clients over
`/v1/*`** — zero routing logic duplicated, no risk of two routers diverging.

## 4. Crate layout & CLI wiring

Top-level `/mcp` workspace member (per issue #526) as a **library**, plus a
`Mcp` subcommand in `apps/bitrouter` so the user-facing entry is
`bitrouter mcp …` (not a standalone `bitrouter-mcp` binary).

```
bitrouter/
├── apps/bitrouter/
│   └── src/main.rs           ← clap: add `Mcp { Serve, Install }` subcommand
│                               (delegates into the bitrouter-mcp crate)
├── crates/
├── plugins/
├── skills/
└── mcp/                      ← new workspace member (LIBRARY, not a binary)
    ├── Cargo.toml            ← [lib]; depends on rmcp (workspace, 1.7),
    │                           bitrouter-sdk (Config), bitrouter-cloud-sdk, reqwest
    │                           ⚠ must NOT depend on the apps/bitrouter package
    │                             — the Mcp subcommand makes that a cycle (§4.1)
    └── src/
        ├── lib.rs            ← pub fn serve(opts), pub fn install(opts)
        ├── backend/
        │   ├── mod.rs        ← Backend trait
        │   ├── local.rs      ← LocalBackend  (127.0.0.1:4356 + control socket)
        │   └── cloud.rs      ← CloudBackend  (api.bitrouter.ai)
        ├── tools/
        │   ├── mod.rs        ← #[tool_router] + tier filtering
        │   ├── complete.rs
        │   ├── list_models.rs
        │   └── status.rs
        ├── server.rs         ← rmcp stdio + streamable-HTTP wiring
        └── install.rs        ← writes client config blocks
```

Root `Cargo.toml`: add `"mcp"` to `members` (the glob is
`crates/*`/`plugins/*`/`apps/*`, so top-level `/mcp` needs an explicit entry).

`apps/bitrouter` gains a `Mcp` subcommand that calls `bitrouter_mcp::serve`/
`install`. The stdio MCP process the client launches is therefore
`bitrouter mcp serve`.

### 4.1 Dependency direction (avoid a cycle)

Because `apps/bitrouter` depends on `bitrouter-mcp` (for the subcommand),
`bitrouter-mcp` **must not** depend back on the `apps/bitrouter` package — that
is a Cargo cycle. Consequence: the crate reaches the daemon only via its
**public surfaces**, not by calling app-internal helpers:

- **HTTP** — `GET :4356/v1/models` (list_models + liveness), `POST /v1/*`
  (complete).
- **Control socket** — the `DaemonCommand`/`DaemonResponse` wire protocol for
  `status` (`{ pid, listen, models }`).

The control-socket types and `commands::list_providers` currently live **inside
`apps/bitrouter`**. To use them without a cycle, either (a) keep v1 local
`status` to the control-socket fields only and derive provider names directly
from `bitrouter-sdk::Config` (no catalog enrichment), or (b) extract the
control-socket protocol + `list_providers` into a small shared crate. v1
prefers (a) to stay in scope; (b) is the follow-up if richer status is wanted.
See §13.

### CLI surface
```bash
# stdio (local daemon backend) — default
bitrouter mcp serve

# explicit transport
bitrouter mcp serve --transport stdio                       # local daemon
bitrouter mcp serve --transport http --bind 0.0.0.0:4357    # remote, cloud backend
bitrouter mcp serve --backend cloud                         # force cloud over stdio (rare)

# opt-in admin tool (repeatable)
bitrouter mcp serve --enable-tool reload

# wiring helpers
bitrouter mcp install --print              # emit the client config block
bitrouter mcp install --client claude      # write it (Claude Code / Desktop)
bitrouter mcp install --client cursor
```

**Route mounting (HTTP).** The streamable-HTTP origin server mounts at a
**distinct path from the gateway's `/mcp`** — default `/mcp-control` (final name
TBD in implementation; must not be a prefix collision with `/mcp` or
`/mcp/{id}`). For a hosted deployment this is the path `mcp.bitrouter.ai`
fronts.

`rmcp` features required: `server`, `macros`, `transport-io` (stdio),
`transport-streamable-http-server`, and `auth` (§7). Confirm exact feature
names exist under the workspace-pinned **1.7** at implementation time.

## 5. Tool surface (v1)

| Tool | Tier | Local backend | Cloud backend |
|------|------|---------------|---------------|
| `complete` | Core | → `:4356/v1/*` | → `api.bitrouter.ai/v1/*` |
| `list_models` | Core | daemon routes | entitled models |
| `status` | ReadOnly | running/pid/listen/models/providers | credit balance |

Defined in the registry but **default-off** (Admin tier, opt-in via
`--enable-tool`), proving the tier mechanism without shipping risk:
`reload`, `key_sign`, `policy_create`.

**Explicitly cut from v1 / never MCP tools:**
- `daemon_start` / `daemon_stop` — handing an LLM a kill switch on shared
  infrastructure; high blast radius, near-zero value. If the local daemon is
  unreachable, the backend returns a clear *"daemon not running — run
  `bitrouter start`"* error (optionally auto-start in a later version).
- separate `chat_completion` + `send_message` — collapsed into one `complete`
  (§5.1).
- `login` / `logout` — interactive; stay CLI-only.
- `serve` — nonsensical as a tool (the server is already serving).

### 5.1 `complete` — unified completion

MCP is a new interface, so we define its schema rather than mirror BitRouter's
OpenAI-vs-Anthropic wire duality. The caller hands us `{ model, messages, … }`;
the backend picks the upstream shape from the route. This avoids leaking
internal API-shape details to a caller that shouldn't care.

```jsonc
// input
{
  "model": "openai/gpt-4o",              // routable name (from list_models)
  "messages": [{ "role": "user", "content": "…" }],
  "max_tokens": 1024,                    // optional
  "temperature": 0.7,                    // optional
  "system": "…"                          // optional
}
// output: full (non-streaming) assistant message + usage
{
  "content": "…",
  "model": "openai/gpt-4o",
  "usage": { "input_tokens": 12, "output_tokens": 240 },
  "finish_reason": "stop"
}
```

Real v1 use cases: multi-model fan-out, cost-routing a sub-task to a cheap
model, second opinions, eval. Note MCP does **not** replace pointing the main
agent loop's `base_url` at `:4356`; it adds *sidecar* completions.

### 5.2 `list_models`

Read-only discovery; required to use `complete` well. Local: the daemon's
routable models. Cloud: the caller's entitled models. Returns
`[{ id, provider, active }]`.

### 5.3 `status`

Read-only health/identity, structured JSON (never the CLI's pretty table). No
daemon-protocol change required: the local backend hits the control socket for
liveness/pid/listen/model-count (the existing `DaemonResponse::Status` wire
shape `{ pid, listen, models }`). Provider detail is derived from
`bitrouter-sdk::Config` directly (the richer `commands::list_providers`
enrichment lives in `apps/bitrouter` and is off-limits without a cycle — §4.1),
so the `providers` field carries declared id/active only.

```jsonc
// local backend
{ "running": true, "pid": 4821, "listen": "127.0.0.1:4356",
  "models": 37, "providers": [ { "id": "openai", "active": true }, … ] }

// cloud backend — from bitrouter-cloud-sdk billing_balance()
{ "available_micro_usd": 4231000,
  "balance_micro_usd": 5000000,
  "pending_micro_usd": 769000 }
```

## 6. Backend trait

```rust
#[async_trait]
trait Backend: Send + Sync {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse>;
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    async fn status(&self) -> Result<StatusInfo>;
}
```

- `LocalBackend` — reqwest to `http://127.0.0.1:4356/v1/*`; `status` also reads
  the daemon control socket (wire protocol) + `bitrouter-sdk::Config` providers,
  not app-internal helpers (§4.1). Auth: none when the daemon runs
  `skip_auth: true` (starter-config default); otherwise forward the configured
  `master_key` / virtual key.
- `CloudBackend` — reqwest to `https://api.bitrouter.ai/v1/*`; forwards the
  caller's `Authorization` bearer (§7); `status` calls
  `bitrouter-cloud-sdk` `billing_balance()`.

Tool handlers depend only on `dyn Backend`, so a tool behaves identically
across local and cloud — the backend supplies the URL and credential.

## 7. Authentication

### Local (stdio)
BYOK. If the daemon runs `skip_auth: true` (default), no credential. If
`skip_auth: false`, the MCP server reads the configured `master_key` / virtual
key (config or env) and forwards it.

### Remote (streamable HTTP) — resource server delegating to the existing Cloud AS

BitRouter Cloud is **already an OAuth 2.0 authorization server**:
`bitrouter-cloud-sdk` implements AS metadata discovery
(`.well-known/oauth-authorization-server`), token endpoint + refresh (RFC
6749/6750), credential persistence with auto-refresh, **and the
authorization-code flow** (confirmed available — not just device flow).

So the remote MCP server implements **no auth of its own** — it is a resource
server that forwards the caller's `Authorization` bearer to
`api.bitrouter.ai`, which validates it:

```
MCP client ──Bearer──▶ origin server (/mcp-control) ──forward Authorization──▶ api.bitrouter.ai/v1
     │                                                                              │
  token from BitRouter Cloud AS (same credential `bitrouter auth login` mints)   validates
```

Because the Cloud AS supports authorization-code, native in-client browser
consent is feasible **in v1**:

- **v1 baseline:** forward the bearer. The token may come from
  `bitrouter auth login`, a pasted `brk_` key, or a header set in the client's
  config. Reuses 100% of existing cloud auth; **zero new auth-server code.**
- **v1 (if scope allows):** wire `rmcp`'s `auth` feature + OAuth Protected
  Resource Metadata (RFC 9728) pointing at the existing Cloud AS, so the MCP
  client runs authorization-code + PKCE in the browser. **Verify at
  implementation:** whether the AS also offers dynamic client registration
  (RFC 7591) that some MCP clients expect; if not, pre-registered client ids
  cover it. The baseline works regardless.

The config key for any of this is **not** `mcp:` (taken by the gateway) — see
§9. v1 needs none.

## 8. Streaming

The **streamable-HTTP transport** (MCP session framing) is independent of
**token streaming**. v1 supports the transport but `complete` returns a full
result. Streaming tokens through an MCP tool result is awkward and defers
cleanly to a later version.

## 9. Configuration

**v1 is CLI-flag-driven; no new `bitrouter.yaml` block.** The only thing worth
configuring is the admin-tool allowlist, and admin tools are deferred/default
-off, so `--enable-tool <name>` (repeatable) on `bitrouter mcp serve` suffices.
This deliberately sidesteps the `mcp:` key, which the gateway owns
(`McpConfig`: aggregate route, cache TTLs).

If file-based control is later needed (hosted remote deployments), introduce a
**new top-level `mcp_serve:`** block then — never overload `mcp:`.

## 10. `install` command

Writes the client wiring block (non-destructively: merge into the existing
`mcpServers` map; never clobber unrelated entries; refuse + warn on parse
failure). v1 clients: `claude` (Claude Code / Desktop) and `cursor`.

- **stdio block** points at `bitrouter mcp serve`.
- **remote block** points at the hosted URL (`/mcp-control` path) with an
  `Authorization` header placeholder for the user's token.
- `--print` emits the block to stdout instead of editing files.

## 11. Testing

- **Backend unit tests** — mock the `/v1/*` upstreams (wiremock-style); assert
  request shaping (`complete` → correct upstream body) and auth-header
  forwarding for both backends.
- **Registry/tier tests** — default enables Core+ReadOnly only; Admin appears
  only when `--enable-tool` opts in; cloud backend never exposes lifecycle
  tools.
- **Tool handler tests** — `complete`/`list_models`/`status` against a mock
  backend, independent of transport.
- **Transport smoke tests** — spin up stdio and streamable-HTTP servers, list
  tools, call `list_models` end to end against a mock backend; assert the HTTP
  variant mounts off `/mcp-control` and does not shadow the gateway `/mcp`.
- **`install` tests** — merge into a fixture client config without clobbering
  existing entries; `--print` output is valid.
- No `.unwrap`/`.expect`/`panic!`, no `#[allow]`, no dead code (CLAUDE.md).

## 12. Skill update (required by CLAUDE.md)

`skills/bitrouter/` must document the origin MCP server in the same change: the
`bitrouter mcp serve`/`install` commands, the two transports, the `:4356` local
backend, the `api.bitrouter.ai` cloud backend, the v1 tool list, and the
distinction from the existing MCP *gateway* at `/mcp`. Deep detail (config
block examples, tier/allowlist, route) goes in `skills/bitrouter/references/`.

## 13. Open questions (carry into the plan)

1. Final HTTP mount path for the origin server (`/mcp-control` placeholder) —
   confirm no prefix collision with the gateway's `/mcp` / `/mcp/{id}`.
2. Whether to pull native browser OAuth (rmcp `auth` + PRM) fully into v1 or
   ship bearer-forward first; depends on AS dynamic-client-registration support
   (§7).
3. Exact `rmcp` 1.7 feature flag names for streamable-HTTP server + stdio +
   auth (verify against the pinned version).
4. `complete` parameter coverage beyond the v1 minimum (tools/function-calling,
   stop sequences) — keep minimal for v1, revisit per demand.
5. Control-socket `DaemonCommand`/`DaemonResponse` types + `list_providers`
   live in `apps/bitrouter`; using them without a dependency cycle (§4.1) means
   either reimplementing the small wire protocol in `bitrouter-mcp` or
   extracting a shared crate. Decide in the plan; v1 leans on the wire protocol
   + `bitrouter-sdk::Config`.
