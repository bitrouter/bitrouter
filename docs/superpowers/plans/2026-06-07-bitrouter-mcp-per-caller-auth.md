# BitRouter MCP Per-Caller Bearer Forwarding — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the remote (streamable-HTTP) MCP server multi-tenant — forward each caller's `Authorization` bearer to `api.bitrouter.ai`, with an explicit `CloudAuth` credential mode and a pre-auth edge middleware.

**Architecture:** A per-call `CallerAuth { bearer }` is threaded through the `Backend` trait. `CloudBackend` holds an explicit `CloudAuth` mode (`Static(token)` for stdio→cloud, `PerCaller` for http→cloud). Tool handlers read the caller's bearer from `RequestContext`'s injected `http::request::Parts`. An axum middleware rejects bearer-less HTTP→cloud requests at the edge.

**Tech Stack:** Rust 2024, `rmcp` 1.7, `axum` 0.8, `reqwest`, `wiremock` (tests), `cargo-nextest`. Branch: `feat/mcp-per-caller-auth` (already checked out, stacked on the v1 work).

---

## Context for the implementer

- The `bitrouter-mcp` crate (`/mcp`) already exists (v1, PR #530). Current state:
  - `mcp/src/backend/mod.rs` defines `Backend` trait + types. Current trait:
    ```rust
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, BackendError>;
    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError>;
    async fn status(&self) -> Result<StatusInfo, BackendError>;
    ```
    `BackendError` has `DaemonUnreachable`, `Upstream`, `Transport`, `Decode`.
  - `mcp/src/backend/local.rs` — `LocalBackend { base_url, http }`, pure reqwest.
  - `mcp/src/backend/cloud.rs` — `CloudBackend { base_url, token: String, http }`, `new(base_url, token)`, private `bearer(&self, rb)` = `rb.bearer_auth(&self.token)`.
  - `mcp/src/server.rs` — `BitrouterMcp` rmcp handler with 3 `#[tool]`s; `serve_stdio`, `serve_http(backend, bind)`, `build_backend(kind, local_url, cloud_url, cloud_token)`.
  - `mcp/src/lib.rs` — `Transport`, `BackendKind`, `ServeOptions`, `serve()`.
- Project rules (CLAUDE.md): no `#[allow]`; no `.unwrap()`/`.expect()`/`panic!` in non-test code (Option/Result combinators like `.unwrap_or_default()` OK; tests may use `.expect()`/`panic!`); no dead code. Run `cargo fmt` before every commit. Conventional commit titles < 60 chars.
- Spec: `docs/superpowers/specs/2026-06-07-bitrouter-mcp-per-caller-auth-design.md`.

## File structure (what changes)

| File | Change |
|------|--------|
| `mcp/src/backend/mod.rs` | add `CallerAuth`, `MissingCredential`; change `Backend` trait sigs |
| `mcp/src/backend/local.rs` | impl sigs gain `_caller: &CallerAuth` (ignored); tests updated |
| `mcp/src/backend/cloud.rs` | `CloudAuth` enum; `auth` field; per-call credential resolution; tests |
| `mcp/src/server.rs` | `caller_from` helper; tools take `RequestContext`; `serve_http` middleware; `build_backend` takes `Transport` |
| `mcp/src/lib.rs` | `serve()` passes `Transport` + cloud flag |
| `mcp/tests/multitenant_http.rs` | new integration test (two bearers forwarded) |
| `skills/bitrouter/references/mcp-server.md` | document multi-tenant remote |

---

## Task 1: Thread `CallerAuth` through the `Backend` trait

The trait change is atomic — every impl, call site, and test updates together so the crate compiles. `CloudBackend` keeps its `token: String` for now (CloudAuth enum is Task 2) but starts honoring a caller bearer.

**Files:** `mcp/src/backend/mod.rs`, `mcp/src/backend/local.rs`, `mcp/src/backend/cloud.rs`, `mcp/src/server.rs`

- [ ] **Step 1: Add `CallerAuth` + `MissingCredential` and change the trait in `mcp/src/backend/mod.rs`**

Add near the other types:
```rust
/// The caller's bearer to forward upstream, if the inbound request carried one.
/// Empty for stdio (the cloud backend's configured credential applies instead).
#[derive(Debug, Default, Clone)]
pub struct CallerAuth {
    pub bearer: Option<String>,
}
```
Add a variant to `BackendError`:
```rust
    #[error("no bearer token: set Authorization on the MCP client")]
    MissingCredential,
```
Change the three `Backend` methods to:
```rust
    async fn complete(&self, caller: &CallerAuth, req: CompleteRequest) -> Result<CompleteResponse, BackendError>;
    async fn list_models(&self, caller: &CallerAuth) -> Result<Vec<ModelInfo>, BackendError>;
    async fn status(&self, caller: &CallerAuth) -> Result<StatusInfo, BackendError>;
```

- [ ] **Step 2: Update `LocalBackend` (ignores caller) in `mcp/src/backend/local.rs`**

Add `CallerAuth` to the `use super::{…}` import. Change the three impl method signatures to take `_caller: &CallerAuth` as the first argument (bodies unchanged):
```rust
    async fn list_models(&self, _caller: &CallerAuth) -> Result<Vec<ModelInfo>, BackendError> { /* unchanged */ }
    async fn complete(&self, _caller: &CallerAuth, req: CompleteRequest) -> Result<CompleteResponse, BackendError> { /* unchanged */ }
    async fn status(&self, _caller: &CallerAuth) -> Result<StatusInfo, BackendError> { /* unchanged */ }
```
In the `#[cfg(test)] mod tests`, update the three call sites to pass `&CallerAuth::default()`:
- `backend.list_models()` → `backend.list_models(&CallerAuth::default())`
- `backend.status()` → `backend.status(&CallerAuth::default())` (both occurrences)
Add `use super::CallerAuth;` to the test module if not already in scope via `use super::*`.

- [ ] **Step 3: Update `CloudBackend` to honor a caller bearer in `mcp/src/backend/cloud.rs`**

Add `CallerAuth` to the `use super::{…}` import. Change the private `bearer` helper to resolve the caller's bearer with the configured token as fallback:
```rust
    fn bearer(&self, caller: &CallerAuth, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let token = caller.bearer.as_deref().unwrap_or(&self.token);
        rb.bearer_auth(token)
    }
```
Change the three impl method signatures to take `caller: &CallerAuth` first, and pass `caller` to every `self.bearer(...)` call:
```rust
    async fn list_models(&self, caller: &CallerAuth) -> Result<Vec<ModelInfo>, BackendError> {
        // … self.bearer(self.http.get(&url)) → self.bearer(caller, self.http.get(&url)) …
    }
    async fn complete(&self, caller: &CallerAuth, req: CompleteRequest) -> Result<CompleteResponse, BackendError> {
        // … self.bearer(caller, self.http.post(&url).json(&body)) …
    }
    async fn status(&self, caller: &CallerAuth) -> Result<StatusInfo, BackendError> {
        // … self.bearer(caller, self.http.get(&url)) …
    }
```
Update existing cloud tests' call sites to pass `&CallerAuth::default()` (e.g. `backend.status(&CallerAuth::default())`, `backend.list_models(&CallerAuth::default())`).

- [ ] **Step 4: Update the tool handlers + StubBackend in `mcp/src/server.rs`**

In the three `#[tool]` methods, pass a default caller for now (real extraction is Task 3). Add `use crate::backend::CallerAuth;` to the imports. Change:
- `self.backend.complete(req)` → `self.backend.complete(&CallerAuth::default(), req)`
- `self.backend.list_models()` → `self.backend.list_models(&CallerAuth::default())`
- `self.backend.status()` → `self.backend.status(&CallerAuth::default())`

In `#[cfg(test)] mod tests`, the `StubBackend impl Backend` must match the new signatures:
```rust
        async fn complete(&self, _: &CallerAuth, _: CompleteRequest) -> Result<CompleteResponse, BackendError> { /* unchanged body */ }
        async fn list_models(&self, _: &CallerAuth) -> Result<Vec<ModelInfo>, BackendError> { Ok(vec![]) }
        async fn status(&self, _: &CallerAuth) -> Result<StatusInfo, BackendError> { /* unchanged body */ }
```
Add `CallerAuth` to the test module's `use` (e.g. `use crate::backend::{BackendError, CallerAuth, CompleteResponse, ModelInfo, StatusInfo, Usage};`).

- [ ] **Step 5: Add a CloudBackend precedence test (caller bearer overrides token)**

Add to `mcp/src/backend/cloud.rs` `tests`:
```rust
#[tokio::test]
async fn caller_bearer_overrides_configured_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer caller-tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list", "data": []
        })))
        .mount(&server)
        .await;
    let backend = CloudBackend::new(server.uri(), "configured-tok");
    let caller = CallerAuth { bearer: Some("caller-tok".into()) };
    backend.list_models(&caller).await.expect("list_models");
    // wiremock asserts the request matched `Bearer caller-tok`, not the configured token.
}
```

- [ ] **Step 6: Verify build + tests + clippy + fmt**

Run: `cargo nextest run -p bitrouter-mcp`
Expected: all pass (11 prior + 1 new = 12).
Run: `cargo clippy -p bitrouter-mcp --all-targets` → clean. `cargo fmt` then `cargo fmt -- --check` → clean.

- [ ] **Step 7: Commit**

```bash
git add mcp/src/backend/mod.rs mcp/src/backend/local.rs mcp/src/backend/cloud.rs mcp/src/server.rs
git commit -m "feat(mcp): thread CallerAuth through Backend trait"
```

---

## Task 2: Replace `CloudBackend.token` with the `CloudAuth` mode enum

**Files:** `mcp/src/backend/cloud.rs`, `mcp/src/server.rs` (build_backend cloud arm)

- [ ] **Step 1: Write the failing test for `PerCaller` with no bearer**

Add to `mcp/src/backend/cloud.rs` `tests`:
```rust
#[tokio::test]
async fn per_caller_without_bearer_errors() {
    let backend = CloudBackend::new("https://api.bitrouter.ai", CloudAuth::PerCaller);
    let err = backend
        .list_models(&CallerAuth::default())
        .await
        .expect_err("should error");
    assert!(matches!(err, BackendError::MissingCredential));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p bitrouter-mcp per_caller_without_bearer_errors`
Expected: FAIL (compile error — `CloudAuth` doesn't exist; `new` takes a token string).

- [ ] **Step 3: Introduce `CloudAuth` and rework `CloudBackend`**

In `mcp/src/backend/cloud.rs`, add the enum and change the struct + constructor + bearer resolution:
```rust
/// How a [`CloudBackend`] authenticates upstream.
pub enum CloudAuth {
    /// One configured token used for every call (stdio → cloud, single-tenant).
    Static(String),
    /// Every call must carry the caller's own bearer; no fallback (http multi-tenant).
    PerCaller,
}

pub struct CloudBackend {
    base_url: String,
    auth: CloudAuth,
    http: reqwest::Client,
}

impl CloudBackend {
    pub fn new(base_url: impl Into<String>, auth: CloudAuth) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            auth,
            http: reqwest::Client::new(),
        }
    }

    /// Resolve the bearer to send: the caller's wins; else the `Static` token;
    /// `PerCaller` with no caller bearer is a (middleware-prevented) error.
    fn resolve_bearer<'a>(&'a self, caller: &'a CallerAuth) -> Result<&'a str, BackendError> {
        match (&self.auth, caller.bearer.as_deref()) {
            (_, Some(b)) => Ok(b),
            (CloudAuth::Static(t), None) => Ok(t),
            (CloudAuth::PerCaller, None) => Err(BackendError::MissingCredential),
        }
    }
}
```
Replace the private `bearer` helper usage: each method now resolves the bearer first and applies it. Change the `bearer` helper to:
```rust
    fn authed(&self, bearer: &str, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(bearer)
    }
```
And in each of the three methods, at the top:
```rust
        let bearer = self.resolve_bearer(caller)?;
```
then replace `self.bearer(caller, self.http.get(&url))` with `self.authed(bearer, self.http.get(&url))` (and likewise for the POST in `complete`).

- [ ] **Step 4: Update the other cloud tests to the new constructor**

Existing cloud tests call `CloudBackend::new(server.uri(), "brk_test")` / `"configured-tok"`. Change each to `CloudBackend::new(server.uri(), CloudAuth::Static("brk_test".into()))` (and the precedence test from Task 1 to `CloudAuth::Static("configured-tok".into())`). Add `CloudAuth` to the test `use super::*` scope (it's in `super`).

- [ ] **Step 5: Update `build_backend` cloud arm in `mcp/src/server.rs`**

For now keep `build_backend`'s signature; just construct `Static`:
```rust
        crate::BackendKind::Cloud => {
            let token = cloud_token.ok_or_else(|| {
                anyhow::anyhow!("cloud backend needs a bearer token (--token or BITROUTER_TOKEN)")
            })?;
            Ok(Arc::new(CloudBackend::new(cloud_url, crate::backend::cloud::CloudAuth::Static(token.to_owned()))))
        }
```
(Transport-aware mode selection is Task 4.)

- [ ] **Step 6: Verify**

Run: `cargo nextest run -p bitrouter-mcp` → all pass (now 13). `cargo clippy -p bitrouter-mcp --all-targets` clean; `cargo fmt` + `--check` clean.

- [ ] **Step 7: Commit**

```bash
git add mcp/src/backend/cloud.rs mcp/src/server.rs
git commit -m "feat(mcp): CloudAuth mode enum (Static | PerCaller)"
```

---

## Task 3: Extract the caller bearer from `RequestContext` in tools

**Files:** `mcp/src/server.rs`

- [ ] **Step 1: Write the failing test for `caller_from`**

Add to `mcp/src/server.rs` `tests` (build a `RequestContext` is heavy; instead unit-test a pure helper that takes the extensions). Refactor extraction into a pure function over `&rmcp::model::Extensions` so it is testable:
```rust
#[test]
fn caller_from_extensions_reads_bearer() {
    use rmcp::model::Extensions;
    let mut ext = Extensions::new();
    let req = http::Request::builder()
        .header(http::header::AUTHORIZATION, "Bearer xyz")
        .body(())
        .expect("req");
    let (parts, _) = req.into_parts();
    ext.insert(parts);
    assert_eq!(caller_from_extensions(&ext).bearer.as_deref(), Some("xyz"));

    let empty = Extensions::new();
    assert_eq!(caller_from_extensions(&empty).bearer, None);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p bitrouter-mcp caller_from_extensions_reads_bearer`
Expected: FAIL (`caller_from_extensions` undefined). (`http` is already a transitive dep via rmcp/axum; if the test needs it directly, add `http = { workspace = true }` to `[dev-dependencies]` or `[dependencies]` of `mcp/Cargo.toml` — it is in the workspace deps.)

- [ ] **Step 3: Implement `caller_from_extensions` + wire it into the tools**

Add to `mcp/src/server.rs`:
```rust
use crate::backend::CallerAuth;

/// Extract the caller's bearer from MCP request extensions (the streamable-HTTP
/// transport injects `http::request::Parts`). Returns an empty `CallerAuth`
/// over stdio (no parts) or when no/!Bearer `Authorization` is present.
fn caller_from_extensions(ext: &rmcp::model::Extensions) -> CallerAuth {
    let bearer = ext
        .get::<http::request::Parts>()
        .and_then(|p| p.headers.get(http::header::AUTHORIZATION))
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_owned);
    CallerAuth { bearer }
}
```
Change the three tool methods to take `ctx: rmcp::service::RequestContext<rmcp::model::RoleServer>` as a parameter and use the real caller:
```rust
    #[tool(description = "Route a completion through BitRouter and return the full result.")]
    async fn complete(
        &self,
        Parameters(args): Parameters<CompleteArgs>,
        ctx: rmcp::service::RequestContext<rmcp::model::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let caller = caller_from_extensions(&ctx.extensions);
        // … self.backend.complete(&caller, req) …
    }
```
`list_models` and `status` likewise gain `ctx: RequestContext<RoleServer>` and call `caller_from_extensions(&ctx.extensions)`, replacing the `&CallerAuth::default()` placeholders from Task 1. Import `RoleServer` / `RequestContext` at the top (e.g. `use rmcp::model::RoleServer; use rmcp::service::RequestContext;`) to keep signatures tidy.

> If the `#[tool]` macro rejects the combined `Parameters<…>` + `RequestContext<…>` signature (it should not — both are `FromContextPart` extractors), report the exact error; do not work around it silently.

- [ ] **Step 4: Verify**

Run: `cargo nextest run -p bitrouter-mcp` → all pass (incl. `caller_from_extensions_reads_bearer`, and the existing `handler_constructs_with_three_tools` and `stdio_lists_three_tools` still green — proving stdio still works with the new tool signatures). `cargo clippy` clean; `cargo fmt`/`--check` clean.

- [ ] **Step 5: Commit**

```bash
git add mcp/src/server.rs mcp/Cargo.toml
git commit -m "feat(mcp): forward caller bearer from request context"
```

---

## Task 4: Make `build_backend` transport-aware (mode selection)

**Files:** `mcp/src/server.rs`, `mcp/src/lib.rs`

- [ ] **Step 1: Change `build_backend` to take `Transport` and pick the mode**

Replace `build_backend` in `mcp/src/server.rs`:
```rust
/// Build the backend. The cloud auth mode depends on transport:
/// stdio→cloud uses the configured token (Static); http→cloud is multi-tenant
/// (PerCaller — each request must carry its own bearer).
pub fn build_backend(
    kind: crate::BackendKind,
    transport: crate::Transport,
    local_url: &str,
    cloud_url: &str,
    cloud_token: Option<&str>,
) -> anyhow::Result<Arc<dyn Backend>> {
    use crate::backend::cloud::CloudAuth;
    match kind {
        crate::BackendKind::Local => Ok(Arc::new(LocalBackend::new(local_url))),
        crate::BackendKind::Cloud => {
            let auth = match transport {
                crate::Transport::Http => CloudAuth::PerCaller,
                crate::Transport::Stdio => {
                    let token = cloud_token.ok_or_else(|| {
                        anyhow::anyhow!("stdio cloud backend needs a token (--token or BITROUTER_TOKEN)")
                    })?;
                    CloudAuth::Static(token.to_owned())
                }
            };
            Ok(Arc::new(CloudBackend::new(cloud_url, auth)))
        }
    }
}
```

- [ ] **Step 2: Update the `serve()` call site in `mcp/src/lib.rs`**

In `serve()`, pass `opts.transport` into `build_backend`:
```rust
    let backend = server::build_backend(
        opts.backend,
        opts.transport,
        &opts.local_url,
        &opts.cloud_url,
        opts.cloud_token.as_deref(),
    )?;
```

- [ ] **Step 3: Verify**

Run: `cargo build -p bitrouter-mcp` and `cargo nextest run -p bitrouter-mcp` → green. `cargo clippy` clean; fmt clean.

- [ ] **Step 4: Commit**

```bash
git add mcp/src/server.rs mcp/src/lib.rs
git commit -m "feat(mcp): select CloudAuth mode by transport"
```

---

## Task 5: Pre-auth edge middleware on the HTTP cloud route

**Files:** `mcp/src/server.rs`, `mcp/src/lib.rs`

- [ ] **Step 1: Write the failing middleware test**

Add a test module to `mcp/src/server.rs` (or a new `mcp/tests/edge_auth.rs`). Test the pure middleware predicate by extracting it:
```rust
#[test]
fn require_bearer_predicate() {
    assert!(has_bearer(Some("Bearer abc")));
    assert!(!has_bearer(Some("Basic abc")));
    assert!(!has_bearer(None));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p bitrouter-mcp require_bearer_predicate`
Expected: FAIL (`has_bearer` undefined).

- [ ] **Step 3: Implement the predicate + middleware, install it in `serve_http` for cloud**

Add to `mcp/src/server.rs`:
```rust
/// Whether an `Authorization` header value is a Bearer token.
fn has_bearer(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.starts_with("Bearer "))
}

async fn require_bearer(
    headers: axum::http::HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let present = has_bearer(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok()),
    );
    if present {
        next.run(request).await
    } else {
        axum::http::StatusCode::UNAUTHORIZED.into_response()
    }
}
```
Add `use axum::response::IntoResponse;` for `.into_response()`. Change `serve_http` to accept a `require_auth: bool` and conditionally layer the middleware:
```rust
pub async fn serve_http(backend: Arc<dyn Backend>, bind: &str, require_auth: bool) -> anyhow::Result<()> {
    // … build `service` as today …
    let mut router = axum::Router::new().nest_service("/mcp-control", service);
    if require_auth {
        router = router.layer(axum::middleware::from_fn(require_bearer));
    }
    // … bind + serve as today …
}
```

- [ ] **Step 4: Update the `serve()` call site in `mcp/src/lib.rs`**

```rust
        Transport::Http => {
            let require_auth = matches!(opts.backend, BackendKind::Cloud);
            server::serve_http(backend, &opts.bind, require_auth).await
        }
```

- [ ] **Step 5: Verify**

Run: `cargo nextest run -p bitrouter-mcp` → green (incl. `require_bearer_predicate`). `cargo clippy` clean; fmt clean.

- [ ] **Step 6: Commit**

```bash
git add mcp/src/server.rs mcp/src/lib.rs
git commit -m "feat(mcp): pre-auth edge middleware for http cloud"
```

---

## Task 6: Multi-tenant HTTP integration test

**Files:** `mcp/tests/multitenant_http.rs` (new)

Proves two different callers' bearers are each forwarded verbatim to the (mock) cloud.

- [ ] **Step 1: Write the integration test**

Create `mcp/tests/multitenant_http.rs`:
```rust
//! End-to-end: two MCP clients with different bearers each get their own bearer
//! forwarded to the (mock) cloud. Proof of multi-tenancy.
use std::sync::Arc;
use std::time::Duration;

use bitrouter_mcp::backend::cloud::{CloudAuth, CloudBackend};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn call_list_models(base: &str, bearer: &str) {
    // Drive the streamable-HTTP MCP endpoint with initialize + tools/call.
    let http = reqwest::Client::new();
    // initialize
    let init = http.post(format!("{base}/mcp-control"))
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#)
        .send().await.expect("init send");
    let session = init.headers().get("mcp-session-id").and_then(|h| h.to_str().ok()).map(str::to_owned);
    // tools/call list_models
    let mut req = http.post(format!("{base}/mcp-control"))
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_models","arguments":{}}}"#);
    if let Some(s) = session { req = req.header("mcp-session-id", s); }
    let _ = req.send().await.expect("call send");
}

#[tokio::test]
async fn two_callers_forward_distinct_bearers() {
    // Mock cloud: assert each bearer is seen on /v1/models.
    let cloud = MockServer::start().await;
    for tok in ["aaa", "bbb"] {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", format!("Bearer {tok}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"object":"list","data":[]})))
            .expect(1)
            .mount(&cloud)
            .await;
    }

    // Serve the MCP HTTP server (PerCaller) pointed at the mock cloud, on an ephemeral port.
    let backend: Arc<dyn bitrouter_mcp::backend::Backend> =
        Arc::new(CloudBackend::new(cloud.uri(), CloudAuth::PerCaller));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        bitrouter_mcp::server::serve_http_on(backend, listener, true).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let base = format!("http://{addr}");
    call_list_models(&base, "aaa").await;
    call_list_models(&base, "bbb").await;

    // wiremock `.expect(1)` per bearer verifies on drop that each was forwarded exactly once.
    drop(cloud);
    server.abort();
}
```

- [ ] **Step 2: Add a `serve_http_on` variant that accepts a pre-bound listener**

The test needs an ephemeral port and to start serving without `ctrl_c`-only shutdown. In `mcp/src/server.rs`, factor the router-building out and add:
```rust
/// Like `serve_http`, but serves on an already-bound listener and shuts down
/// when the listener's task is aborted (used by tests). No ctrl_c handler.
pub async fn serve_http_on(
    backend: Arc<dyn Backend>,
    listener: tokio::net::TcpListener,
    require_auth: bool,
) -> anyhow::Result<()> {
    let router = build_http_router(backend, require_auth);
    axum::serve(listener, router).await?;
    Ok(())
}
```
Refactor the shared router construction into `fn build_http_router(backend: Arc<dyn Backend>, require_auth: bool) -> axum::Router` and have `serve_http` call it (so `serve_http` keeps its ctrl_c graceful shutdown, and `serve_http_on` reuses the router). Keep the `StreamableHttpServerConfig` cancellation wiring inside `build_http_router` or `serve_http` as today.

> Note: this exposes `serve_http_on` as `pub` solely for integration testing of real multi-tenant forwarding. It is a thin, documented wrapper — acceptable. If the team prefers not to widen the public surface, gate it behind `#[doc(hidden)]`.

- [ ] **Step 3: Run the test**

Run: `cargo nextest run -p bitrouter-mcp two_callers_forward_distinct_bearers`
Expected: PASS. If the streamable-HTTP request framing needs an `mcp-session-id` flow the test doesn't satisfy, adjust the request sequence per the server's responses (read the init response body/headers). The assertion that matters: each `Bearer` reached `/v1/models` exactly once.

- [ ] **Step 4: Verify full crate + commit**

Run: `cargo nextest run -p bitrouter-mcp` (all green), `cargo clippy -p bitrouter-mcp --all-targets` (clean), `cargo fmt`/`--check` (clean).
```bash
git add mcp/src/server.rs mcp/tests/multitenant_http.rs
git commit -m "test(mcp): multi-tenant bearer forwarding integration test"
```

---

## Task 7: Update the `/bitrouter` skill reference

**Files:** `skills/bitrouter/references/mcp-server.md`

- [ ] **Step 1: Document multi-tenant remote auth**

Edit `skills/bitrouter/references/mcp-server.md` to state:
- The remote HTTP server (`bitrouter mcp serve --transport http`) is **multi-tenant**: each MCP client sets its own `Authorization: Bearer <brk_… or access token>` in its remote-server config; the server forwards it per request to `api.bitrouter.ai`.
- HTTP→cloud requires a `Bearer` (edge middleware returns `401` otherwise).
- `--token`/`BITROUTER_TOKEN` is the **stdio→cloud** credential only; a multi-tenant HTTP host needs no token.
- Native browser OAuth (Item B) remains deferred (cloud lacks Dynamic Client Registration; `/oauth/authorize` requires a `brk_` principal).

Keep `skills/bitrouter/SKILL.md` unchanged unless a one-line tweak to the existing MCP section is warranted (it should already say "see references/mcp-server.md").

- [ ] **Step 2: Commit**

```bash
git add skills/bitrouter/references/mcp-server.md
git commit -m "docs(skill): document multi-tenant remote MCP auth"
```

---

## Task 8: Full verification

- [ ] **Step 1: Whole-crate + workspace checks**

Run: `cargo nextest run --all-features` → all pass (workspace; new crate ~16 tests).
Run: `cargo clippy --all-features` → clean (no `#[allow]`).
Run: `cargo fmt -- --check` → clean.

- [ ] **Step 2: Manual smoke (help only — no live daemon/cloud)**

Run: `cargo run -p bitrouter -- mcp serve --help` → still lists flags (no flag changes expected).

- [ ] **Step 3: Final commit if fmt/clippy adjusted anything**

```bash
git add -A && git commit -m "chore(mcp): clippy + fmt for per-caller auth"
```

---

## Self-review notes (addressed)

- **Spec coverage:** `CallerAuth` + trait threading (Task 1); `CloudAuth` enum + `MissingCredential` (Task 2); `caller_from` extraction + `RequestContext` tools (Task 3); transport→mode in `build_backend` (Task 4); edge middleware HTTP+cloud-only (Task 5); multi-tenant proof (Task 6); precedence tests (Tasks 1–2); skill docs (Task 7); verification (Task 8). Behaviour matrix rows all covered.
- **Type consistency:** `CallerAuth { bearer: Option<String> }`, `CloudAuth::{Static(String), PerCaller}`, `BackendError::MissingCredential`, `build_backend(kind, transport, local_url, cloud_url, cloud_token)`, `serve_http(backend, bind, require_auth)`, `serve_http_on(backend, listener, require_auth)`, `caller_from_extensions(&Extensions) -> CallerAuth`, `has_bearer(Option<&str>) -> bool` — used consistently across tasks.
- **Risk:** the only novel rmcp surface is a tool taking both `Parameters` + `RequestContext` (Task 3) — confirmed supported at the source level (`FromContextPart` for both); the step flags it to report rather than work around if it fails.
