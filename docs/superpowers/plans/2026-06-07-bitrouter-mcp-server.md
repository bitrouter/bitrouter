# BitRouter Origin MCP Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship an origin MCP server exposing BitRouter's own `complete` / `list_models` / `status` tools over both stdio (local daemon) and streamable HTTP (cloud), driven by `bitrouter mcp serve` / `install`.

**Architecture:** A new top-level `/mcp` **library** crate built on `rmcp` 1.7 (server features). A `Backend` trait (`LocalBackend` → `127.0.0.1:4356`, `CloudBackend` → `api.bitrouter.ai`) — both **pure reqwest clients**, no dependency on `apps/bitrouter` or `bitrouter-sdk` (zero cycle risk). One `#[tool_router]` definition is served over stdio and streamable HTTP. `apps/bitrouter` gains a thin `Mcp` subcommand that calls into the crate.

**Tech Stack:** Rust 2024, `rmcp` 1.7 (`server`, `macros`, `transport-io`, `transport-streamable-http-server`), `axum` 0.8, `reqwest`, `tokio`, `wiremock` (tests), `cargo-nextest`.

---

## Scope refinements baked into this plan (vs. the spec)

These tighten the spec's open questions into concrete v1 decisions. All reduce scope/risk:

1. **`LocalBackend` is pure reqwest** — no control socket, no `Config`, no `apps/bitrouter` / `bitrouter-sdk` dep. `status` derives everything from `GET /v1/models` (liveness, model count, and the **distinct providers** across models' `providers` arrays). This **fully dissolves the §4.1 dependency-cycle concern** and drops `pid` from v1 local status.
2. **`CloudBackend` is token-based** — takes a bearer via `--token` / `BITROUTER_TOKEN`, hits `api.bitrouter.ai/v1/{chat/completions,models,billing/balance}` directly. Auto-reading the stored OAuth credential (so no `--token`) and per-caller multi-tenant bearer forwarding + native browser OAuth are **v1.x** (see rmcp `*_auth_streamhttp.rs` examples).
3. **No tier/allowlist machinery in v1** — admin tools are deferred, so per CLAUDE.md #4 (no over-design / dead code) v1 ships exactly the 3 tools with no `--enable-tool` flag. The tier concept is documented; the filter lands with the first admin tool.
4. **HTTP mount path:** `/mcp-control` (axum `nest_service`), distinct from the gateway's `/mcp`.

---

## File structure

| File | Responsibility |
|------|----------------|
| `mcp/Cargo.toml` | crate manifest; enables rmcp server features |
| `mcp/src/lib.rs` | public `serve(ServeOptions)` / `install(InstallOptions)`; `Transport`/`BackendKind` enums |
| `mcp/src/backend/mod.rs` | `Backend` trait + shared types (`CompleteRequest`, `CompleteResponse`, `Usage`, `ModelInfo`, `StatusInfo`) |
| `mcp/src/backend/local.rs` | `LocalBackend` (reqwest → `:4356`) |
| `mcp/src/backend/cloud.rs` | `CloudBackend` (reqwest + bearer → `api.bitrouter.ai`) |
| `mcp/src/server.rs` | `BitrouterMcp` rmcp handler: the 3 `#[tool]`s, `ServerHandler`, `serve_stdio`, `serve_http` |
| `mcp/src/install.rs` | render/write client config blocks |
| `apps/bitrouter/src/main.rs` | add `Mcp { Serve, Install }` subcommand + dispatch |
| `Cargo.toml` (root) | add `"mcp"` to `members` |
| `skills/bitrouter/SKILL.md` + `references/` | document the origin server (CLAUDE.md requirement) |

---

## Task 1: Scaffold the crate and wire it into the workspace

**Files:**
- Create: `mcp/Cargo.toml`
- Create: `mcp/src/lib.rs`
- Modify: `Cargo.toml` (root, `members`)

- [ ] **Step 1: Create `mcp/Cargo.toml`**

```toml
[package]
name = "bitrouter-mcp"
version = { workspace = true }
edition = { workspace = true }
license = { workspace = true }
authors = { workspace = true }
repository = { workspace = true }

[lib]

[dependencies]
rmcp = { workspace = true, features = [
    "server",
    "macros",
    "transport-io",
    "transport-streamable-http-server",
] }
async-trait = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "signal"] }
tokio-util = { workspace = true }
reqwest = { workspace = true }
axum = { workspace = true }
tracing = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
wiremock = "0.6"
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "test-util"] }
```

- [ ] **Step 2: Create a minimal `mcp/src/lib.rs`**

```rust
//! BitRouter origin MCP server — exposes BitRouter's own tools
//! (`complete` / `list_models` / `status`) over stdio and streamable HTTP.
//!
//! Distinct from the MCP *gateway* in `bitrouter-sdk::mcp`, which proxies
//! *upstream* MCP servers. This crate is the *origin* server for BitRouter's
//! own capabilities.

pub mod backend;
pub mod install;
pub mod server;

/// Which wire transport the server speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Newline-delimited JSON-RPC over stdin/stdout (local clients launch this).
    Stdio,
    /// Streamable HTTP, mounted at `/mcp-control`.
    Http,
}

/// Which backend the tools route to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// The local BYOK daemon at `127.0.0.1:4356`.
    Local,
    /// BitRouter Cloud at `api.bitrouter.ai`.
    Cloud,
}
```

- [ ] **Step 3: Add `"mcp"` to the root workspace members**

In `Cargo.toml` (root), change:

```toml
members = ["crates/*", "plugins/*", "apps/*"]
```
to:
```toml
members = ["crates/*", "plugins/*", "apps/*", "mcp"]
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p bitrouter-mcp`
Expected: compiles (the `server`/`install`/`backend` modules don't exist yet — so this step will FAIL until Step 5 stubs them).

- [ ] **Step 5: Add empty module stubs so the crate compiles**

Create `mcp/src/backend/mod.rs`, `mcp/src/install.rs`, `mcp/src/server.rs` each containing only:
```rust
//! stub — filled in by later tasks
```
And in `lib.rs`, `pub mod backend;` already points at `backend/mod.rs`.

Run: `cargo build -p bitrouter-mcp`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add mcp/Cargo.toml mcp/src/lib.rs mcp/src/backend/mod.rs mcp/src/install.rs mcp/src/server.rs Cargo.toml
git commit -m "feat(mcp): scaffold bitrouter-mcp crate"
```

---

## Task 2: Backend trait and shared types

**Files:**
- Modify: `mcp/src/backend/mod.rs`

- [ ] **Step 1: Write the shared types and trait**

Replace the stub in `mcp/src/backend/mod.rs` with:

```rust
//! The `Backend` abstraction over *where* tool calls route, plus the wire
//! types the tools and both backends share. Implementations are thin reqwest
//! clients — no routing logic lives here.

use async_trait::async_trait;

pub mod cloud;
pub mod local;

/// A normalized completion request, independent of the upstream wire shape.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CompleteRequest {
    /// Routable model name (e.g. `openai/gpt-4o`), from `list_models`.
    pub model: String,
    /// Chat messages, passed through to the OpenAI-shaped upstream verbatim.
    pub messages: Vec<serde_json::Value>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub system: Option<String>,
}

/// Token accounting for a completion.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A full (non-streaming) completion result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CompleteResponse {
    pub content: String,
    pub model: String,
    pub usage: Usage,
    pub finish_reason: String,
}

/// One routable model.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub active: bool,
}

/// Backend-specific status payload.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum StatusInfo {
    Local {
        running: bool,
        listen: String,
        models: usize,
        providers: Vec<ProviderStatus>,
    },
    Cloud {
        available_micro_usd: i64,
        balance_micro_usd: i64,
        pending_micro_usd: i64,
    },
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ProviderStatus {
    pub id: String,
    pub active: bool,
}

/// Errors surfaced to the MCP client as tool failures.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("daemon not reachable at {0} — run `bitrouter start`")]
    DaemonUnreachable(String),
    #[error("upstream returned {status}: {body}")]
    Upstream { status: u16, body: String },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("malformed upstream response: {0}")]
    Decode(String),
}

/// Where tool calls route. Object-safe so tools hold `Arc<dyn Backend>`.
#[async_trait]
pub trait Backend: Send + Sync {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, BackendError>;
    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError>;
    async fn status(&self) -> Result<StatusInfo, BackendError>;
}
```

- [ ] **Step 2: Create empty backend module files so it compiles**

Create `mcp/src/backend/local.rs` and `mcp/src/backend/cloud.rs`, each:
```rust
//! stub — filled in by later tasks
```

- [ ] **Step 3: Verify it builds**

Run: `cargo build -p bitrouter-mcp`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add mcp/src/backend/
git commit -m "feat(mcp): backend trait and shared wire types"
```

---

## Task 3: `LocalBackend::list_models`

**Files:**
- Modify: `mcp/src/backend/local.rs`
- Test: inline `#[cfg(test)]` in `mcp/src/backend/local.rs`

`GET /v1/models` returns `{ "object": "list", "data": [ { "id": "...", "object": "model", "providers": ["openai", ...] } ] }` (verified in `bitrouter-sdk/src/server.rs`). `provider` = first entry of `providers`; `active` = `true` (a routable model is active).

- [ ] **Step 1: Write the failing test**

Put in `mcp/src/backend/local.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn list_models_maps_data_to_modelinfo() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    { "id": "openai/gpt-4o", "object": "model", "providers": ["openai"] },
                    { "id": "claude/sonnet",  "object": "model", "providers": ["anthropic"] }
                ]
            })))
            .mount(&server)
            .await;

        let backend = LocalBackend::new(server.uri());
        let models = backend.list_models().await.expect("list_models");

        assert_eq!(models, vec![
            ModelInfo { id: "openai/gpt-4o".into(), provider: "openai".into(), active: true },
            ModelInfo { id: "claude/sonnet".into(), provider: "anthropic".into(), active: true },
        ]);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p bitrouter-mcp list_models_maps_data_to_modelinfo`
Expected: FAIL — `LocalBackend` undefined.

- [ ] **Step 3: Implement `LocalBackend` + `list_models`**

At the top of `mcp/src/backend/local.rs`:

```rust
//! `LocalBackend` — thin reqwest client against the local BYOK daemon
//! (`http://127.0.0.1:4356`). Pure HTTP: no control socket, no config, no
//! dependency on `apps/bitrouter` (which would be a cycle).

use async_trait::async_trait;

use super::{
    Backend, BackendError, CompleteRequest, CompleteResponse, ModelInfo, ProviderStatus,
    StatusInfo, Usage,
};

/// Routes tool calls to the local daemon's `/v1/*` HTTP API.
pub struct LocalBackend {
    base_url: String,
    http: reqwest::Client,
}

impl LocalBackend {
    /// `base_url` is the daemon root, e.g. `http://127.0.0.1:4356`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            http: reqwest::Client::new(),
        }
    }
}

#[derive(serde::Deserialize)]
struct ModelsEnvelope {
    data: Vec<ModelEntry>,
}

#[derive(serde::Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    providers: Vec<String>,
}

#[async_trait]
impl Backend for LocalBackend {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| BackendError::DaemonUnreachable(self.base_url.clone()).if_not(e))?;
        let env: ModelsEnvelope = resp
            .json()
            .await
            .map_err(|e| BackendError::Decode(e.to_string()))?;
        Ok(env
            .data
            .into_iter()
            .map(|m| ModelInfo {
                provider: m.providers.first().cloned().unwrap_or_default(),
                id: m.id,
                active: true,
            })
            .collect())
    }

    async fn complete(&self, _req: CompleteRequest) -> Result<CompleteResponse, BackendError> {
        unimplemented!("Task 4")
    }

    async fn status(&self) -> Result<StatusInfo, BackendError> {
        unimplemented!("Task 5")
    }
}

/// Map a reqwest transport error to `DaemonUnreachable` when it is a connect
/// failure, else a generic transport error.
trait IfNot {
    fn if_not(self, e: reqwest::Error) -> BackendError;
}
impl IfNot for BackendError {
    fn if_not(self, e: reqwest::Error) -> BackendError {
        if e.is_connect() {
            self
        } else {
            BackendError::Transport(e.to_string())
        }
    }
}
```

> Note: `unimplemented!` here is scaffolding for the next two tasks, not shipped behavior — Tasks 4 and 5 replace both before any commit that wires tools. It is acceptable transiently because no caller reaches it yet.

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo nextest run -p bitrouter-mcp list_models_maps_data_to_modelinfo`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add mcp/src/backend/local.rs
git commit -m "feat(mcp): LocalBackend::list_models"
```

---

## Task 4: `LocalBackend::complete`

**Files:**
- Modify: `mcp/src/backend/local.rs`

Forwards an OpenAI-shaped body to `POST /v1/chat/completions` and extracts `choices[0].message.content`, `choices[0].finish_reason`, and `usage`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `mcp/src/backend/local.rs`:

```rust
#[tokio::test]
async fn complete_posts_openai_body_and_extracts_content() {
    use wiremock::matchers::body_partial_json;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(serde_json::json!({ "model": "openai/gpt-4o" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [ { "message": { "content": "hi there" }, "finish_reason": "stop" } ],
            "usage": { "prompt_tokens": 12, "completion_tokens": 5 }
        })))
        .mount(&server)
        .await;

    let backend = LocalBackend::new(server.uri());
    let out = backend
        .complete(CompleteRequest {
            model: "openai/gpt-4o".into(),
            messages: vec![serde_json::json!({ "role": "user", "content": "hi" })],
            max_tokens: Some(64),
            temperature: None,
            system: None,
        })
        .await
        .expect("complete");

    assert_eq!(out.content, "hi there");
    assert_eq!(out.finish_reason, "stop");
    assert_eq!(out.usage, Usage { input_tokens: 12, output_tokens: 5 });
    assert_eq!(out.model, "openai/gpt-4o");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p bitrouter-mcp complete_posts_openai_body`
Expected: FAIL — `complete` panics with `unimplemented!`.

- [ ] **Step 3: Implement `complete`**

Replace the `complete` body in the `impl Backend for LocalBackend` block:

```rust
async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, BackendError> {
    let url = format!("{}/v1/chat/completions", self.base_url);
    let mut body = serde_json::json!({
        "model": req.model,
        "messages": req.messages,
    });
    if let Some(m) = req.max_tokens {
        body["max_tokens"] = m.into();
    }
    if let Some(t) = req.temperature {
        body["temperature"] = t.into();
    }
    if let Some(s) = req.system {
        body["system"] = s.into();
    }
    let resp = self
        .http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| BackendError::DaemonUnreachable(self.base_url.clone()).if_not(e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(BackendError::Upstream {
            status: status.as_u16(),
            body: resp.text().await.unwrap_or_default(),
        });
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| BackendError::Decode(e.to_string()))?;
    let choice = v
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| BackendError::Decode("no choices in response".into()))?;
    Ok(CompleteResponse {
        content: choice
            .pointer("/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_owned(),
        finish_reason: choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .unwrap_or_default()
            .to_owned(),
        usage: Usage {
            input_tokens: v.pointer("/usage/prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
            output_tokens: v.pointer("/usage/completion_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
        },
        model: req.model,
    })
}
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo nextest run -p bitrouter-mcp complete_posts_openai_body`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add mcp/src/backend/local.rs
git commit -m "feat(mcp): LocalBackend::complete"
```

---

## Task 5: `LocalBackend::status`

**Files:**
- Modify: `mcp/src/backend/local.rs`

Derives status from `GET /v1/models`: `running=true`, `listen=base_url`, `models=data.len()`, `providers` = distinct union of each model's `providers`, each `active:true`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module:

```rust
#[tokio::test]
async fn status_summarizes_models_and_distinct_providers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [
                { "id": "openai/gpt-4o", "providers": ["openai"] },
                { "id": "openai/gpt-4o-mini", "providers": ["openai"] },
                { "id": "claude/sonnet", "providers": ["anthropic"] }
            ]
        })))
        .mount(&server)
        .await;

    let backend = LocalBackend::new(server.uri());
    match backend.status().await.expect("status") {
        StatusInfo::Local { running, models, mut providers, .. } => {
            assert!(running);
            assert_eq!(models, 3);
            providers.sort_by(|a, b| a.id.cmp(&b.id));
            assert_eq!(providers, vec![
                ProviderStatus { id: "anthropic".into(), active: true },
                ProviderStatus { id: "openai".into(), active: true },
            ]);
        }
        other => panic!("expected Local, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p bitrouter-mcp status_summarizes_models`
Expected: FAIL — `status` panics with `unimplemented!`.

- [ ] **Step 3: Implement `status`**

Replace the `status` body:

```rust
async fn status(&self) -> Result<StatusInfo, BackendError> {
    let url = format!("{}/v1/models", self.base_url);
    let resp = self
        .http
        .get(&url)
        .send()
        .await
        .map_err(|e| BackendError::DaemonUnreachable(self.base_url.clone()).if_not(e))?;
    let env: ModelsEnvelope = resp
        .json()
        .await
        .map_err(|e| BackendError::Decode(e.to_string()))?;

    let mut seen = std::collections::BTreeSet::new();
    let mut providers = Vec::new();
    for m in &env.data {
        for p in &m.providers {
            if seen.insert(p.clone()) {
                providers.push(ProviderStatus { id: p.clone(), active: true });
            }
        }
    }
    Ok(StatusInfo::Local {
        running: true,
        listen: self.base_url.clone(),
        models: env.data.len(),
        providers,
    })
}
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo nextest run -p bitrouter-mcp status_summarizes_models`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add mcp/src/backend/local.rs
git commit -m "feat(mcp): LocalBackend::status"
```

---

## Task 6: `CloudBackend` (all three methods)

**Files:**
- Modify: `mcp/src/backend/cloud.rs`

Same `/v1/chat/completions` and `/v1/models` shapes as local, against `https://api.bitrouter.ai` with a `Authorization: Bearer <token>` header. `status` calls `GET /v1/billing/balance` → `{ balance_micro_usd, pending_micro_usd, available_micro_usd }` (verified shape in `bitrouter-cloud-sdk/src/management/billing.rs`).

- [ ] **Step 1: Write the failing tests**

In `mcp/src/backend/cloud.rs`:

```rust
//! `CloudBackend` — thin reqwest client against BitRouter Cloud
//! (`https://api.bitrouter.ai`) with a bearer token. v1 takes the token
//! explicitly; auto-reading the stored OAuth credential is v1.x.

use async_trait::async_trait;

use super::{
    Backend, BackendError, CompleteRequest, CompleteResponse, ModelInfo, StatusInfo, Usage,
};

pub struct CloudBackend {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl CloudBackend {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            token: token.into(),
            http: reqwest::Client::new(),
        }
    }

    fn bearer(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(&self.token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn status_reads_billing_balance_with_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/billing/balance"))
            .and(header("authorization", "Bearer brk_test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "balance_micro_usd": 5_000_000,
                "pending_micro_usd": 769_000,
                "available_micro_usd": 4_231_000
            })))
            .mount(&server)
            .await;

        let backend = CloudBackend::new(server.uri(), "brk_test");
        match backend.status().await.expect("status") {
            StatusInfo::Cloud { available_micro_usd, balance_micro_usd, pending_micro_usd } => {
                assert_eq!(available_micro_usd, 4_231_000);
                assert_eq!(balance_micro_usd, 5_000_000);
                assert_eq!(pending_micro_usd, 769_000);
            }
            other => panic!("expected Cloud, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_models_sends_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer brk_test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [ { "id": "openai/gpt-4o", "providers": ["openai"] } ]
            })))
            .mount(&server)
            .await;

        let backend = CloudBackend::new(server.uri(), "brk_test");
        let models = backend.list_models().await.expect("models");
        assert_eq!(models, vec![ModelInfo {
            id: "openai/gpt-4o".into(), provider: "openai".into(), active: true,
        }]);
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p bitrouter-mcp cloud`
Expected: FAIL — `Backend for CloudBackend` not implemented.

- [ ] **Step 3: Implement `Backend for CloudBackend`**

Add, before the `#[cfg(test)]` module:

```rust
#[derive(serde::Deserialize)]
struct ModelsEnvelope {
    data: Vec<ModelEntry>,
}
#[derive(serde::Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    providers: Vec<String>,
}
#[derive(serde::Deserialize)]
struct Balance {
    balance_micro_usd: i64,
    pending_micro_usd: i64,
    available_micro_usd: i64,
}

#[async_trait]
impl Backend for CloudBackend {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .bearer(self.http.get(&url))
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        let env: ModelsEnvelope = resp
            .json()
            .await
            .map_err(|e| BackendError::Decode(e.to_string()))?;
        Ok(env
            .data
            .into_iter()
            .map(|m| ModelInfo {
                provider: m.providers.first().cloned().unwrap_or_default(),
                id: m.id,
                active: true,
            })
            .collect())
    }

    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, BackendError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let mut body = serde_json::json!({ "model": req.model, "messages": req.messages });
        if let Some(m) = req.max_tokens { body["max_tokens"] = m.into(); }
        if let Some(t) = req.temperature { body["temperature"] = t.into(); }
        if let Some(s) = req.system { body["system"] = s.into(); }
        let resp = self
            .bearer(self.http.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(BackendError::Upstream {
                status: status.as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| BackendError::Decode(e.to_string()))?;
        let choice = v.get("choices").and_then(|c| c.get(0))
            .ok_or_else(|| BackendError::Decode("no choices in response".into()))?;
        Ok(CompleteResponse {
            content: choice.pointer("/message/content").and_then(|c| c.as_str()).unwrap_or_default().to_owned(),
            finish_reason: choice.get("finish_reason").and_then(|f| f.as_str()).unwrap_or_default().to_owned(),
            usage: Usage {
                input_tokens: v.pointer("/usage/prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                output_tokens: v.pointer("/usage/completion_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
            },
            model: req.model,
        })
    }

    async fn status(&self) -> Result<StatusInfo, BackendError> {
        let url = format!("{}/v1/billing/balance", self.base_url);
        let resp = self
            .bearer(self.http.get(&url))
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        let b: Balance = resp.json().await.map_err(|e| BackendError::Decode(e.to_string()))?;
        Ok(StatusInfo::Cloud {
            available_micro_usd: b.available_micro_usd,
            balance_micro_usd: b.balance_micro_usd,
            pending_micro_usd: b.pending_micro_usd,
        })
    }
}
```

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo nextest run -p bitrouter-mcp cloud`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add mcp/src/backend/cloud.rs
git commit -m "feat(mcp): CloudBackend complete/list_models/status"
```

---

## Task 7: rmcp server handler with the three tools

**Files:**
- Modify: `mcp/src/server.rs`

Grounded in rmcp 1.7 `examples/servers/src/common/counter.rs`. The handler holds `Arc<dyn Backend>` and a `ToolRouter<Self>`.

⚠ **rmcp API verification:** rmcp 1.7's exact macro/serve API is confirmed from the published examples, but the `schemars` re-export path and `CallToolResult`/`Content` constructors should be confirmed by `cargo build` in Step 3. If `rmcp::schemars` is not re-exported, add `schemars` as a direct dependency at the version rmcp 1.7 requires and import `schemars::JsonSchema`.

- [ ] **Step 1: Write the failing test**

In `mcp/src/server.rs`:

```rust
//! `BitrouterMcp` — the rmcp origin server handler. One `#[tool_router]`
//! definition serves both stdio and streamable HTTP.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::backend::{Backend, CompleteRequest};

#[derive(Clone)]
pub struct BitrouterMcp {
    backend: Arc<dyn Backend>,
    tool_router: ToolRouter<BitrouterMcp>,
}

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct CompleteArgs {
    /// Routable model name (from `list_models`).
    pub model: String,
    /// Chat messages, OpenAI shape: `[{"role":"user","content":"…"}]`.
    pub messages: Vec<serde_json::Value>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub system: Option<String>,
}

#[tool_router]
impl BitrouterMcp {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend, tool_router: Self::tool_router() }
    }

    #[tool(description = "Route a completion through BitRouter and return the full result.")]
    async fn complete(
        &self,
        Parameters(args): Parameters<CompleteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let req = CompleteRequest {
            model: args.model,
            messages: args.messages,
            max_tokens: args.max_tokens,
            temperature: args.temperature,
            system: args.system,
        };
        match self.backend.complete(req).await {
            Ok(r) => Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string(&r).unwrap_or_default(),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(description = "List models routable through BitRouter.")]
    async fn list_models(&self) -> Result<CallToolResult, McpError> {
        match self.backend.list_models().await {
            Ok(m) => Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string(&m).unwrap_or_default(),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(description = "Report BitRouter status (local: liveness/models/providers; cloud: credit balance).")]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        match self.backend.status().await {
            Ok(s) => Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string(&s).unwrap_or_default(),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }
}

#[tool_handler]
impl ServerHandler for BitrouterMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "BitRouter origin MCP server. Use `list_models` to discover routable \
                 models, `complete` to run a completion, `status` for health/credits."
                    .to_string(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendError, CompleteResponse, ModelInfo, StatusInfo, Usage};

    struct StubBackend;
    #[async_trait::async_trait]
    impl Backend for StubBackend {
        async fn complete(&self, _: CompleteRequest) -> Result<CompleteResponse, BackendError> {
            Ok(CompleteResponse {
                content: "ok".into(), model: "m".into(),
                usage: Usage { input_tokens: 1, output_tokens: 1 }, finish_reason: "stop".into(),
            })
        }
        async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> { Ok(vec![]) }
        async fn status(&self) -> Result<StatusInfo, BackendError> {
            Ok(StatusInfo::Cloud { available_micro_usd: 1, balance_micro_usd: 1, pending_micro_usd: 0 })
        }
    }

    #[test]
    fn handler_constructs_with_three_tools() {
        let h = BitrouterMcp::new(Arc::new(StubBackend));
        assert_eq!(h.tool_router.list_all().len(), 3);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p bitrouter-mcp handler_constructs_with_three_tools`
Expected: FAIL (compile error — server module was a stub).

- [ ] **Step 3: Make it compile and pass; resolve the rmcp specifics**

Build and fix any rmcp-path issues flagged by the compiler (the verification note above): confirm `rmcp::schemars::JsonSchema`, `CallToolResult::success`/`error`, `Content::text`, `ToolRouter::list_all`. Adjust imports per `cargo build` errors only — the structure is fixed.

Run: `cargo nextest run -p bitrouter-mcp handler_constructs_with_three_tools`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add mcp/src/server.rs
git commit -m "feat(mcp): rmcp handler with complete/list_models/status tools"
```

---

## Task 8: Serve over stdio and streamable HTTP; public `serve()`

**Files:**
- Modify: `mcp/src/server.rs` (add `serve_stdio`, `serve_http`)
- Modify: `mcp/src/lib.rs` (add `ServeOptions` + `serve`)

Grounded in `counter_stdio.rs` and `counter_streamhttp.rs` (rmcp 1.7).

- [ ] **Step 1: Add the serve functions to `server.rs`**

Append:

```rust
use crate::backend::cloud::CloudBackend;
use crate::backend::local::LocalBackend;

/// Serve over stdio until the client disconnects.
pub async fn serve_stdio(backend: Arc<dyn Backend>) -> anyhow::Result<()> {
    use rmcp::{ServiceExt, transport::stdio};
    let service = BitrouterMcp::new(backend).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Serve streamable HTTP at `/mcp-control` on `bind` until Ctrl-C.
pub async fn serve_http(backend: Arc<dyn Backend>, bind: &str) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    let ct = tokio_util::sync::CancellationToken::new();
    let service = StreamableHttpService::new(
        move || Ok(BitrouterMcp::new(backend.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp-control", service);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let shutdown = {
        let ct = ct.clone();
        async move { let _ = tokio::signal::ctrl_c().await; ct.cancel(); }
    };
    axum::serve(listener, router).with_graceful_shutdown(shutdown).await?;
    Ok(())
}

/// Build the backend for a given kind from connection params.
pub fn build_backend(
    kind: crate::BackendKind,
    local_url: &str,
    cloud_url: &str,
    cloud_token: Option<&str>,
) -> anyhow::Result<Arc<dyn Backend>> {
    match kind {
        crate::BackendKind::Local => Ok(Arc::new(LocalBackend::new(local_url))),
        crate::BackendKind::Cloud => {
            let token = cloud_token
                .ok_or_else(|| anyhow::anyhow!("cloud backend needs a bearer token (--token or BITROUTER_TOKEN)"))?;
            Ok(Arc::new(CloudBackend::new(cloud_url, token)))
        }
    }
}
```

> `StreamableHttpServerConfig::default()` may need `.with_cancellation_token(ct.child_token())` depending on the exact 1.7 API; the compiler/example will confirm. Keep `ct` wired so graceful shutdown works.

- [ ] **Step 2: Add `ServeOptions` and `serve()` to `lib.rs`**

```rust
/// Parameters for `serve`.
pub struct ServeOptions {
    pub transport: Transport,
    pub backend: BackendKind,
    /// Local daemon root. Default `http://127.0.0.1:4356`.
    pub local_url: String,
    /// Cloud root. Default `https://api.bitrouter.ai`.
    pub cloud_url: String,
    /// Bearer for the cloud backend (from `--token` / `BITROUTER_TOKEN`).
    pub cloud_token: Option<String>,
    /// HTTP bind address (only for `Transport::Http`). Default `127.0.0.1:4357`.
    pub bind: String,
}

/// Run the MCP server to completion.
pub async fn serve(opts: ServeOptions) -> anyhow::Result<()> {
    let backend = server::build_backend(
        opts.backend,
        &opts.local_url,
        &opts.cloud_url,
        opts.cloud_token.as_deref(),
    )?;
    match opts.transport {
        Transport::Stdio => server::serve_stdio(backend).await,
        Transport::Http => server::serve_http(backend, &opts.bind).await,
    }
}
```

Add `anyhow = { workspace = true }` to `mcp/Cargo.toml` `[dependencies]`.

- [ ] **Step 3: Verify it builds**

Run: `cargo build -p bitrouter-mcp`
Expected: PASS (fix rmcp serve-API specifics flagged by the compiler per the note).

- [ ] **Step 4: Smoke-test stdio with a list_tools request**

Add an integration test `mcp/tests/stdio_smoke.rs`:

```rust
//! Drives the stdio server with a raw initialize + tools/list and asserts the
//! three tools are advertised.
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn stdio_lists_three_tools() {
    // The crate is a lib; expose a tiny test binary via `examples/`.
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-stdio-local"))
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;
    stdin.write_all(format!("{init}\n").as_bytes()).await.unwrap();
    let _ = out.next_line().await.unwrap(); // init result
    let listed = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
    stdin.write_all(format!("{listed}\n").as_bytes()).await.unwrap();
    let line = out.next_line().await.unwrap().unwrap();
    assert!(line.contains("complete") && line.contains("list_models") && line.contains("status"));
    let _ = child.kill().await;
}
```

Create the test binary `mcp/examples/mcp-stdio-local.rs`:
```rust
use std::sync::Arc;
use bitrouter_mcp::backend::local::LocalBackend;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let backend = Arc::new(LocalBackend::new("http://127.0.0.1:4356"));
    bitrouter_mcp::server::serve_stdio(backend).await
}
```

> The example name `mcp-stdio-local` makes `CARGO_BIN_EXE_mcp-stdio-local` available to the test. If the harness resolves examples differently, switch to spawning `cargo run --example mcp-stdio-local`. This needs `LocalBackend`/`serve_stdio` to be `pub` — they are.

Run: `cargo nextest run -p bitrouter-mcp stdio_lists_three_tools`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add mcp/src/server.rs mcp/src/lib.rs mcp/Cargo.toml mcp/tests/stdio_smoke.rs mcp/examples/mcp-stdio-local.rs
git commit -m "feat(mcp): serve over stdio and streamable HTTP"
```

---

## Task 9: `install` — render client config blocks

**Files:**
- Modify: `mcp/src/install.rs`

Renders the `mcpServers` JSON block for a client. v1 clients: `claude`, `cursor`. v1 renders the **stdio** block (the common local path). Writing into the on-disk config is non-destructive merge; `--print` emits to stdout.

- [ ] **Step 1: Write the failing test**

In `mcp/src/install.rs`:

```rust
//! Render (and optionally write) MCP client config blocks for `bitrouter mcp serve`.

/// Supported clients.
#[derive(Debug, Clone, Copy)]
pub enum Client { Claude, Cursor }

/// Render the `mcpServers` entry (stdio) as pretty JSON for `client`.
pub fn render_block(_client: Client) -> serde_json::Value {
    serde_json::json!({
        "mcpServers": {
            "bitrouter": { "command": "bitrouter", "args": ["mcp", "serve"] }
        }
    })
}

/// Merge `block`'s `mcpServers` into an existing client config `doc`
/// non-destructively (never clobbering unrelated servers).
pub fn merge_into(doc: &mut serde_json::Value, block: &serde_json::Value) {
    let dst = doc.as_object_mut().expect("config root must be an object");
    let servers = dst
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if let (Some(servers), Some(add)) = (servers.as_object_mut(), block["mcpServers"].as_object()) {
        for (k, v) in add {
            servers.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_existing_servers() {
        let mut existing = serde_json::json!({
            "mcpServers": { "other": { "command": "x" } },
            "theme": "dark"
        });
        merge_into(&mut existing, &render_block(Client::Claude));
        assert_eq!(existing["theme"], "dark");
        assert_eq!(existing["mcpServers"]["other"]["command"], "x");
        assert_eq!(existing["mcpServers"]["bitrouter"]["command"], "bitrouter");
    }
}
```

- [ ] **Step 2: Run it to verify it fails, then passes**

Run: `cargo nextest run -p bitrouter-mcp merge_preserves_existing_servers`
Expected: FAIL first (stub), then PASS once the code above is in place (it is — the test and impl land together; if the harness needs a red phase, comment out `merge_into`'s body to `unimplemented!()` first, watch it fail, then restore).

- [ ] **Step 3: Add the `install` entry to `lib.rs`**

```rust
use std::path::PathBuf;

pub struct InstallOptions {
    pub client: install::Client,
    /// When set, write+merge into this config path; otherwise print to stdout.
    pub config_path: Option<PathBuf>,
}

pub fn install(opts: InstallOptions) -> anyhow::Result<()> {
    let block = install::render_block(opts.client);
    match opts.config_path {
        None => {
            println!("{}", serde_json::to_string_pretty(&block)?);
            Ok(())
        }
        Some(path) => {
            let mut doc: serde_json::Value = if path.exists() {
                serde_json::from_str(&std::fs::read_to_string(&path)?)
                    .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))?
            } else {
                serde_json::json!({})
            };
            install::merge_into(&mut doc, &block);
            std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
            println!("wrote bitrouter MCP server into {}", path.display());
            Ok(())
        }
    }
}
```

Run: `cargo build -p bitrouter-mcp`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add mcp/src/install.rs mcp/src/lib.rs
git commit -m "feat(mcp): install command renders/merges client config"
```

---

## Task 10: Wire `bitrouter mcp` subcommand in `apps/bitrouter`

**Files:**
- Modify: `apps/bitrouter/Cargo.toml` (add `bitrouter-mcp` dep)
- Modify: `apps/bitrouter/src/main.rs` (clap `Mcp` subcommand + dispatch)

- [ ] **Step 1: Add the dependency**

In `apps/bitrouter/Cargo.toml` `[dependencies]`:
```toml
bitrouter-mcp = { path = "../../mcp", version = "1.0.0-alpha.9" }
```

- [ ] **Step 2: Add the `Mcp` subcommand to the clap `Command` enum**

In `apps/bitrouter/src/main.rs`, inside `enum Command { … }`, add:
```rust
    /// Run or install BitRouter's origin MCP server.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
```
And add the subcommand enum near the other `#[derive(Subcommand)]` blocks:
```rust
#[derive(Subcommand)]
enum McpAction {
    /// Serve the MCP server (stdio by default).
    Serve {
        /// `stdio` (local daemon) or `http` (cloud).
        #[arg(long, default_value = "stdio")]
        transport: String,
        /// `local` or `cloud`. Defaults: stdio→local, http→cloud.
        #[arg(long)]
        backend: Option<String>,
        /// Local daemon root.
        #[arg(long, default_value = "http://127.0.0.1:4356")]
        local_url: String,
        /// Cloud root.
        #[arg(long, default_value = "https://api.bitrouter.ai")]
        cloud_url: String,
        /// Cloud bearer token (else `BITROUTER_TOKEN`).
        #[arg(long)]
        token: Option<String>,
        /// HTTP bind address.
        #[arg(long, default_value = "127.0.0.1:4357")]
        bind: String,
    },
    /// Write/print the client config block.
    Install {
        /// `claude` or `cursor`.
        #[arg(long, default_value = "claude")]
        client: String,
        /// Config file to merge into; omit to print to stdout.
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },
}
```

- [ ] **Step 3: Dispatch in the `match` in `main`**

Add an arm alongside the other `Command::…` arms:
```rust
Command::Mcp { action } => match action {
    McpAction::Serve { transport, backend, local_url, cloud_url, token, bind } => {
        let transport = match transport.as_str() {
            "stdio" => bitrouter_mcp::Transport::Stdio,
            "http" => bitrouter_mcp::Transport::Http,
            other => return Err(anyhow::anyhow!("unknown transport '{other}'")),
        };
        let backend = match backend.as_deref() {
            Some("local") => bitrouter_mcp::BackendKind::Local,
            Some("cloud") => bitrouter_mcp::BackendKind::Cloud,
            None => match transport {
                bitrouter_mcp::Transport::Stdio => bitrouter_mcp::BackendKind::Local,
                bitrouter_mcp::Transport::Http => bitrouter_mcp::BackendKind::Cloud,
            },
            Some(other) => return Err(anyhow::anyhow!("unknown backend '{other}'")),
        };
        let cloud_token = token.or_else(|| std::env::var("BITROUTER_TOKEN").ok());
        bitrouter_mcp::serve(bitrouter_mcp::ServeOptions {
            transport, backend, local_url, cloud_url, cloud_token, bind,
        }).await
    }
    McpAction::Install { client, config } => {
        let client = match client.as_str() {
            "claude" => bitrouter_mcp::install::Client::Claude,
            "cursor" => bitrouter_mcp::install::Client::Cursor,
            other => return Err(anyhow::anyhow!("unknown client '{other}'")),
        };
        bitrouter_mcp::install(bitrouter_mcp::InstallOptions { client, config_path: config })
    }
},
```

> Match the surrounding `main` style: if other arms are sync and this needs `.await`, follow the existing async-dispatch pattern in `main.rs` (the daemon arms already `.await`). Confirm the return type is `anyhow::Result<()>`-compatible.

- [ ] **Step 4: Verify the whole workspace builds**

Run: `cargo build`
Expected: PASS.

- [ ] **Step 5: Manual smoke test**

Run: `cargo run -p bitrouter -- mcp install --client claude`
Expected: prints a JSON block with `"bitrouter": { "command": "bitrouter", "args": ["mcp","serve"] }`.

- [ ] **Step 6: Commit**

```bash
git add apps/bitrouter/Cargo.toml apps/bitrouter/src/main.rs
git commit -m "feat(mcp): wire bitrouter mcp serve/install subcommand"
```

---

## Task 11: Update the `/bitrouter` skill (CLAUDE.md requirement)

**Files:**
- Modify: `skills/bitrouter/SKILL.md`
- Create: `skills/bitrouter/references/mcp-server.md`

- [ ] **Step 1: Add a short MCP section to `SKILL.md`**

Add a section (keep SKILL.md under ~200 lines — deep detail goes to the reference):

```markdown
## Origin MCP server

BitRouter can expose its **own** tools to MCP clients (distinct from the MCP
*gateway* at `/mcp`, which proxies upstream servers):

    bitrouter mcp serve                 # stdio, local daemon backend
    bitrouter mcp serve --transport http --bind 127.0.0.1:4357   # cloud backend
    bitrouter mcp install --client claude   # write client config (or --print)

Tools: `complete`, `list_models`, `status`. Local backend → `127.0.0.1:4356`;
cloud backend → `api.bitrouter.ai` (needs `--token`/`BITROUTER_TOKEN`). HTTP
mounts at `/mcp-control`. See `references/mcp-server.md`.
```

- [ ] **Step 2: Write `references/mcp-server.md`**

Document: the three tools and their JSON shapes (from this plan's Task 2/5/6), both transports, the local-vs-cloud backend split, the `/mcp-control` route and why it differs from the gateway's `/mcp`, the v1 token-based cloud auth, and the deferred tier/admin-tools + native-OAuth roadmap.

- [ ] **Step 3: Commit**

```bash
git add skills/bitrouter/SKILL.md skills/bitrouter/references/mcp-server.md
git commit -m "docs(skill): document the origin MCP server"
```

---

## Task 12: Full verification

- [ ] **Step 1: Tests**

Run: `cargo nextest run --all-features` (or `cargo test --all-features` if nextest absent)
Expected: all pass.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --all-features`
Expected: no warnings. Fix any (no `#[allow]`, no `.unwrap`/`.expect`/`panic!` in non-test code — the backends already return `Result`; the `.unwrap_or_default()` on `serde_json::to_string` of owned structs is infallible-by-shape but prefer `?`-free `unwrap_or_default()` only where serialization cannot fail).

- [ ] **Step 3: Format**

Run: `cargo fmt -- --check` (auto-fix: `cargo fmt`)
Expected: clean.

- [ ] **Step 4: Final commit if fmt/clippy changed anything**

```bash
git add -A
git commit -m "chore(mcp): clippy + fmt"
```

---

## Self-review notes (addressed)

- **Spec coverage:** `complete`/`list_models`/`status` (Tasks 3–8), both transports (Task 8), `bitrouter mcp serve`/`install` (Tasks 9–10), local+cloud backends (Tasks 3–6), skill update (Task 11), tests/clippy/fmt (Task 12). Deferred-by-design (documented): admin tools + tier filter, native browser OAuth, per-caller bearer forwarding, control-socket-derived `pid`.
- **Type consistency:** `Backend` methods, `CompleteRequest`/`CompleteResponse`/`Usage`/`ModelInfo`/`StatusInfo`/`ProviderStatus` are defined once in Task 2 and used unchanged in Tasks 3–8.
- **rmcp risk:** the only unverified-from-repo surface is rmcp 1.7's exact serve/macro paths; Task 7 Step 3 and Task 8 Step 3 are explicit compile-and-fix gates grounded in the published 1.7 examples, not guesses.
```
