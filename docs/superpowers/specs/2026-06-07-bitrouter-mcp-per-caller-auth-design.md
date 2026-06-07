# BitRouter MCP — Per-Caller Bearer Forwarding (v1.x, Item A)

- **Status:** Draft for review (rev 2 — incorporates review feedback)
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
- HTTP requests with no/!Bearer `Authorization` are rejected **at the edge**
  (401) before reaching a tool, when serving the cloud backend.
- stdio behaviour is **unchanged** (no HTTP request → no per-caller bearer →
  the configured construction token applies).
- The local daemon **never** receives a caller bearer.

### Non-Goals
- Native in-client browser OAuth / PRM / DCR (Item B — needs cloud changes).
- Local token *validation*/introspection (the cloud validates; we forward and
  surface its verdict). The edge middleware only checks **presence**, not validity.
- TLS termination / hosting concerns (operator's responsibility; default bind
  stays `127.0.0.1`, forwarding to cloud over HTTPS).

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

The `RequestContext` form is chosen over the bare `Extension<Parts>` extractor
because `ctx.extensions.get::<Parts>()` returns `Option` — `None` over stdio,
where no HTTP parts exist — so the **same tool definition works on both
transports**.

**A `#[tool]` method may take both `Parameters<T>` and `RequestContext`.**
Verified: both implement rmcp's `FromContextPart` extractor
(`handler/server/tool.rs:181`, `handler/server/common.rs:114`), so the macro
extracts each independently — the same axum-style multi-extractor model. (This
was the one open implementation risk; it is resolved.)

## 4. Design

### 4.1 A minimal forwarded-credential type (and why not reuse `CallerContext`)

A small per-call carrier in `mcp/src/backend/mod.rs`:

```rust
/// The caller's bearer to forward upstream, if the inbound request carried one.
/// Empty for stdio (the backend's construction token applies instead).
#[derive(Debug, Default, Clone)]
pub struct CallerAuth {
    pub bearer: Option<String>,
}
```

**Why a new type rather than `bitrouter-sdk::CallerContext`:** `CallerContext`
is a *resolved-identity* type (`api_key_id`, `user_id`, `local`) — the **output**
of authentication, deliberately storing "opaque identity," with **no raw
credential**. We need the opposite: the raw bearer to **relay** to the cloud,
which validates it. `CallerContext` also lives in `bitrouter-sdk`, which the
thin mcp crate intentionally does not depend on (it would pull the whole SDK in
for one struct that doesn't even carry a token). The distinct name `CallerAuth`
avoids implying equivalence with the SDK's `CallerContext`.

### 4.2 `Backend` trait threads the caller credential

```rust
#[async_trait]
pub trait Backend: Send + Sync {
    async fn complete(&self, caller: &CallerAuth, req: CompleteRequest) -> Result<CompleteResponse, BackendError>;
    async fn list_models(&self, caller: &CallerAuth) -> Result<Vec<ModelInfo>, BackendError>;
    async fn status(&self, caller: &CallerAuth) -> Result<StatusInfo, BackendError>;
}
```

- **`LocalBackend`** ignores `caller` entirely (the BYOK daemon is single-tenant
  and must never receive a third party's cloud bearer). Signatures change; bodies
  do not.
- **`CloudBackend.token` stays a required `String`** set at construction
  (`CloudBackend::new(base_url, token)` — unchanged signature). Per the design
  decision, a cloud backend always has a token. Each request authenticates with
  the per-caller bearer when present, else the construction token:
  ```rust
  let bearer = caller.bearer.as_deref().unwrap_or(&self.token);
  ```
  No new error variant: there is always a credential to send (caller's or the
  construction fallback). Invalid tokens are the cloud's `401` →
  `BackendError::Upstream { 401 }` (already handled).

> For a pure multi-tenant HTTP host, the construction token is required but
> effectively a backstop — the §4.5 edge middleware guarantees every HTTP
> request that reaches a tool already carries a caller bearer, which overrides
> it. The operator still supplies `--token`/`BITROUTER_TOKEN` (any valid bearer)
> to build the backend.

### 4.3 Tool handlers

Each of the three `#[tool]` methods in `mcp/src/server.rs` gains a
`ctx: RequestContext<RoleServer>` parameter alongside its existing params,
extracts the bearer per §3 into a `CallerAuth`, and passes it to the backend:

```rust
#[tool(description = "…")]
async fn complete(
    &self,
    Parameters(args): Parameters<CompleteArgs>,
    ctx: RequestContext<RoleServer>,
) -> Result<CallToolResult, McpError> {
    let caller = caller_from(&ctx);          // private helper, §3 extraction
    match self.backend.complete(&caller, args.into()).await { … }
}
```

`list_models` and `status` likewise gain `ctx` and pass `&caller`. A single
private `caller_from(ctx: &RequestContext<RoleServer>) -> CallerAuth` holds the
extraction logic (one place, unit-tested).

### 4.4 CLI / serve wiring

- `CloudBackend::new(base_url, token)` — **unchanged** (token required).
- `build_backend` cloud arm — unchanged: the configured `--token`/
  `BITROUTER_TOKEN` becomes the construction token.
- No new CLI flags. `--token` keeps its meaning (now also the stdio→cloud
  credential and the HTTP backstop).

### 4.5 Pre-auth edge middleware (HTTP + cloud backend only)

`serve_http` installs a small axum middleware on the `/mcp-control` route **when
the backend is `Cloud`**, rejecting requests whose `Authorization` is missing or
not a `Bearer` token with `401 Unauthorized` before rmcp sees them. Modelled on
rmcp's `simple_auth_streamhttp.rs` (`middleware::from_fn`). It checks
**presence only** — token *validity* is the cloud's job.

```rust
// pseudo
async fn require_bearer(headers: HeaderMap, req: Request, next: Next) -> Result<Response, StatusCode> {
    match headers.get(AUTHORIZATION).and_then(|h| h.to_str().ok()) {
        Some(v) if v.starts_with("Bearer ") => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}
```

Not installed for `--backend local` over HTTP (the local daemon is
unauthenticated, so requiring a bearer there would be wrong) — that unusual
combo stays open, with the caller bearer ignored as in §4.2.

## 5. Behaviour matrix

| Transport → backend | Credential used per call |
|---|---|
| stdio → local | none (caller ignored) — unchanged |
| stdio → cloud | construction `--token`/`BITROUTER_TOKEN` (no HTTP parts) — unchanged |
| http → local | caller bearer ignored; no edge middleware; local unauthenticated |
| **http → cloud** | edge middleware requires a `Bearer`; the tool forwards that **caller bearer** to `api.bitrouter.ai` (overriding the construction token) |

## 6. Security

- **Never log tokens** (no `tracing` of `caller.bearer` or the Authorization
  header).
- The **local backend never** attaches a caller bearer — a caller's cloud
  credential cannot leak to the BYOK daemon.
- **Edge middleware** rejects unauthenticated HTTP→cloud requests at the door
  (401), shrinking what reaches rmcp.
- Forwarding to `api.bitrouter.ai` stays over **HTTPS**; the cloud is the sole
  validator of token *validity* (bad token → `401` → `Upstream { 401 }`).
- HTTP bind default stays `127.0.0.1:4357`. Real multi-tenant hosting (operator
  binds `0.0.0.0` behind TLS) is out of scope here but unblocked.

## 7. Testing

- **`caller_from` unit test** — a `RequestContext` whose extensions carry an
  `http::request::Parts` with `Authorization: Bearer xyz` →
  `CallerAuth { bearer: Some("xyz") }`; no parts (stdio) → `CallerAuth::default()`;
  malformed/non-Bearer header → `bearer: None`.
- **CloudBackend precedence tests** — `caller.bearer` overrides the construction
  token (wiremock asserts the forwarded `Authorization` equals the caller bearer,
  not the construction token); with `CallerAuth::default()`, the construction
  token is sent.
- **Edge-middleware tests** — HTTP→cloud request with no `Authorization` → `401`;
  with `Authorization: Bearer x` → passes through (reaches a tool). Confirm the
  middleware is NOT applied for `--backend local`.
- **Multi-tenant HTTP integration test** — streamable-HTTP server pointed at a
  wiremock "cloud"; two `tools/call list_models` requests with **different**
  `Authorization` bearers; assert wiremock saw each bearer forwarded verbatim on
  its respective call. The proof of multi-tenancy.
- **Regression** — existing v1 backend tests update to the new signatures
  (`complete(&CallerAuth::default(), req)` etc.); all stay green. No `#[allow]`,
  no `.unwrap`/`.expect`/`panic!` in non-test code (CLAUDE.md).

## 8. Skill update (CLAUDE.md requirement)

`skills/bitrouter/references/mcp-server.md`: the remote HTTP server is now
**multi-tenant** — each MCP client supplies its own `Authorization: Bearer
<brk_…>` header (HTTP→cloud requires it; 401 otherwise), forwarded per request;
`--token` is the stdio→cloud credential and HTTP construction backstop. Note
native browser OAuth (Item B) remains deferred and why (cloud-side DCR).

## 9. Resolved during design / remaining notes

1. **(resolved)** rmcp `#[tool]` accepts `Parameters<T>` + `RequestContext`
   together — both are `FromContextPart` extractors (§3).
2. **(resolved)** Pre-auth middleware: **included** (§4.5), HTTP+cloud only.
3. **(resolved)** No existing bitrouter mechanism fits; `CallerContext` is a
   resolved-identity type without a raw bearer and lives in `bitrouter-sdk`
   (§4.1). New minimal `CallerAuth` justified.
4. **(decision)** Construction token stays **required** even for multi-tenant
   HTTP (operator passes any valid bearer as the backstop; the edge middleware
   makes per-call use of it moot on HTTP→cloud).
5. **(YAGNI)** `CallerAuth` carries only `bearer`. If Item B later needs resolved
   claims/principal, extend then — not now.
