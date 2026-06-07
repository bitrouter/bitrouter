# BitRouter MCP — Per-Caller Bearer Forwarding (v1.x, Item A)

- **Status:** Draft for review
- **Date:** 2026-06-07
- **Builds on:** `2026-06-07-bitrouter-mcp-server-design.md` (v1, shipped in PR #530)
- **Issue:** follow-up to #526
- **Author:** Spikel (design discussion w/ Claude)

## 1. Summary

Make the remote (streamable-HTTP) MCP server **multi-tenant**: instead of the
cloud backend using a single token configured at startup, each tool call
forwards the **caller's own `Authorization` bearer** to `api.bitrouter.ai`. A
hosted `bitrouter mcp serve --transport http` can then serve many users at
once — each MCP client sets its own `Authorization: Bearer <brk_… or access
token>` in its remote-server config (Claude Code and Cursor both support
headers), and the server forwards that credential per request.

**No `bitrouter-cloud` changes.** `brk_` keys and OAuth access tokens are
already valid bearers on `api.bitrouter.ai/v1`; the cloud validates them and
returns `401` on bad tokens (surfaced as `BackendError::Upstream { 401 }`).

This is v1.x **Item A**. Native browser OAuth (PRM + Dynamic Client
Registration) is **Item B**, deferred: the cloud AS today has no DCR endpoint
and `/oauth/authorize` requires a `brk_` principal, so B needs cross-repo cloud
work and is not required once token-in-header multi-tenancy (A) works.

## 2. Goals / Non-Goals

### Goals
- The HTTP transport forwards each caller's `Authorization` bearer to the cloud,
  per request (true multi-tenant).
- stdio behaviour is **unchanged** (no HTTP request → no per-caller bearer →
  existing configured-token / no-auth paths still apply).
- The local daemon **never** receives a caller bearer.
- A missing bearer (HTTP, no configured fallback) yields a **clear tool error**,
  not an opaque upstream failure.

### Non-Goals
- Native in-client browser OAuth / PRM / DCR (Item B — needs cloud changes).
- Local token validation/introspection in the MCP server (the cloud validates;
  we forward and surface its verdict).
- TLS termination / hosting concerns (operator's responsibility; we keep the
  default bind at `127.0.0.1` and forward to cloud over HTTPS).

## 3. The mechanism (verified against rmcp 1.7.0)

rmcp's streamable-HTTP tower service injects the inbound `http::request::Parts`
(headers included) into each MCP request's extensions
(`transport/streamable_http_server/tower.rs:1039/1102/1179`), reachable from a
tool's `RequestContext`. Confirmed by rmcp's own docs (`tower.rs:438–495`):

```rust
// inside a #[tool] method that also takes `ctx: RequestContext<RoleServer>`
let bearer = ctx
    .extensions
    .get::<http::request::Parts>()                       // None over stdio
    .and_then(|p| p.headers.get(http::header::AUTHORIZATION))
    .and_then(|h| h.to_str().ok())
    .and_then(|s| s.strip_prefix("Bearer "))
    .map(str::to_owned);
```

The `RequestContext` form is chosen over the `Extension<Parts>` extractor
because `ctx.extensions.get::<Parts>()` returns `Option` — `None` over stdio,
where no HTTP parts exist — so the **same tool definition works on both
transports**.

## 4. Design

### 4.1 Separate "what to call" from "who's calling"

A small per-call credential carrier, defined in `mcp/src/backend/mod.rs`:

```rust
/// Per-call caller identity. Empty for stdio / unauthenticated local use.
#[derive(Debug, Default, Clone)]
pub struct Caller {
    /// The caller's bearer token, if the inbound request carried one.
    pub bearer: Option<String>,
}
```

The `Backend` trait threads it through every method:

```rust
#[async_trait]
pub trait Backend: Send + Sync {
    async fn complete(&self, caller: &Caller, req: CompleteRequest) -> Result<CompleteResponse, BackendError>;
    async fn list_models(&self, caller: &Caller) -> Result<Vec<ModelInfo>, BackendError>;
    async fn status(&self, caller: &Caller) -> Result<StatusInfo, BackendError>;
}
```

### 4.2 Backend behaviour

- **`LocalBackend`** ignores `caller` entirely (the BYOK daemon is single-tenant
  and must never receive a third party's cloud bearer). Signatures change; bodies
  do not.
- **`CloudBackend.token`** becomes `Option<String>` — a **fallback** for the
  single-tenant stdio→cloud case. Each request authenticates with:
  ```rust
  let bearer = caller.bearer.as_deref().or(self.token.as_deref());
  ```
  - `Some(b)` → send `Authorization: Bearer b`.
  - `None` → return `BackendError::MissingCredential` (new variant) with a clear
    message: *"no bearer token — set Authorization on the MCP client or pass
    --token"*. (Today `CloudBackend::new` requires a token; this relaxes it so a
    hosted multi-tenant server can run with no default token and rely on
    per-caller bearers.)

### 4.3 Tool handlers

Each of the three `#[tool]` methods in `mcp/src/server.rs` gains a
`ctx: RequestContext<RoleServer>` parameter, extracts the bearer per §3, builds
a `Caller`, and passes it to the backend:

```rust
#[tool(description = "…")]
async fn complete(
    &self,
    Parameters(args): Parameters<CompleteArgs>,
    ctx: RequestContext<RoleServer>,
) -> Result<CallToolResult, McpError> {
    let caller = caller_from(&ctx);          // helper: extracts bearer (§3)
    match self.backend.complete(&caller, args.into()).await { … }
}
```

`list_models` and `status` likewise gain `ctx` and pass `&caller`. A single
private `caller_from(ctx: &RequestContext<RoleServer>) -> Caller` helper holds
the extraction logic (one place, unit-tested).

> Verify at implementation: the rmcp `#[tool]` macro accepts a method taking
> both `Parameters<T>` and `RequestContext<RoleServer>`. rmcp's extractor model
> supports multiple params; confirm by compile. If the combination is rejected,
> fall back to the `Extension<http::request::Parts>` extractor guarded for the
> stdio (no-parts) case.

### 4.4 CLI / serve wiring

- `CloudBackend::new(base_url, token: Option<String>)` — token now optional.
- `build_backend` (in `server.rs`) for the cloud kind: pass the configured
  `--token`/`BITROUTER_TOKEN` as the **fallback** token (may be `None`). The
  per-caller bearer (when present) overrides it inside the backend.
- No new CLI flags. `--token` keeps its meaning (now: the *fallback* used when a
  request carries no `Authorization`). For a pure multi-tenant host, omit it.

## 5. Behaviour matrix

| Transport → backend | Credential used per call |
|---|---|
| stdio → local | none (caller ignored) — unchanged |
| stdio → cloud | configured `--token`/`BITROUTER_TOKEN` (fallback; no HTTP parts) — unchanged |
| http → local | caller bearer ignored (local never gets it); local stays unauthenticated/master_key |
| **http → cloud** | **caller's `Authorization` bearer**; else fallback token; else `MissingCredential` error |

## 6. Security

- **Never log tokens** (no `tracing` of `caller.bearer` or the Authorization
  header).
- The **local backend never** attaches a caller bearer — a caller's cloud
  credential cannot leak to the BYOK daemon.
- Forwarding to `api.bitrouter.ai` stays over **HTTPS**; the cloud is the sole
  validator (bad token → `401` → `Upstream { 401 }`).
- HTTP bind default stays `127.0.0.1:4357`. Real multi-tenant hosting (operator
  binds `0.0.0.0` behind TLS) is out of scope here but unblocked by this change.
- `MissingCredential` returns a tool error, never panics.

## 7. Testing

- **`caller_from` unit test** — given a `RequestContext` whose extensions carry
  an `http::request::Parts` with `Authorization: Bearer xyz`, returns
  `Caller { bearer: Some("xyz") }`; with no parts (stdio) returns
  `Caller::default()`; with a malformed header returns `bearer: None`.
- **CloudBackend precedence tests** — `caller.bearer` overrides the fallback
  token (wiremock asserts the forwarded `Authorization` header equals the caller
  bearer, not the fallback); with neither, `complete/list_models/status` return
  `BackendError::MissingCredential`.
- **Multi-tenant HTTP integration test** — stand up the streamable-HTTP server
  pointed at a wiremock "cloud"; issue two `tools/call list_models` requests
  with **different** `Authorization` bearers; assert the wiremock saw each bearer
  forwarded verbatim on its respective call. This is the proof of multi-tenancy.
- **Regression** — existing v1 backend tests update to the new signatures
  (`complete(&Caller::default(), req)` etc.); all must stay green. No `#[allow]`,
  no `.unwrap`/`.expect`/`panic!` in non-test code (CLAUDE.md).

## 8. Skill update (CLAUDE.md requirement)

`skills/bitrouter/references/mcp-server.md`: document that the remote HTTP
server is now **multi-tenant** — each MCP client supplies its own
`Authorization: Bearer <brk_…>` header in its remote-server config, forwarded
per request; `--token` is the optional single-tenant fallback. Note native
browser OAuth (Item B) remains deferred and why (cloud-side DCR).

## 9. Open questions (carry into the plan)

1. Confirm the rmcp `#[tool]` macro accepts `Parameters<T>` + `RequestContext<…>`
   together (§4.3) — else use the `Extension<Parts>` fallback.
2. Whether to also add a lightweight axum middleware that rejects HTTP requests
   with *no* `Authorization` header up front (defense-in-depth). Leaning **no**
   for this increment — the per-tool `MissingCredential` error is sufficient and
   keeps stdio/HTTP symmetric; revisit if pre-auth rejection is wanted for hosting.
3. `Caller` is intentionally minimal (just `bearer`). If Item B later needs a
   resolved principal/claims, extend `Caller` then — not now (YAGNI).
