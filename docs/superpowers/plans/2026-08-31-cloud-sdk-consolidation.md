# BitRouter Cloud SDK Consolidation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move every live `bitrouter-cloud-sdk` responsibility into its owning core crate or application module, enforce one Cloud account credential source with explicit API-key precedence, and delete the obsolete crate.

**Architecture:** `bitrouter-providers` gains an optional `hosted` feature containing the account store, OAuth primitives, shared `CredentialManager`, and provider `AuthApplier`. `apps/bitrouter` owns commands and Cloud HTTP clients and injects one manager into provider and telemetry consumers; `bitrouter-mcp` owns its small billing wire type. The migration remains buildable until the final task removes the old crate.

**Tech Stack:** Rust 2024, Tokio, reqwest, serde, thiserror, chrono, wiremock, Cargo nextest

**Spec:** `docs/superpowers/specs/2026-08-31-cloud-sdk-consolidation-design.md`

## Global Constraints

- Keep exactly one persistent BitRouter Cloud credential at `<bitrouter-data-dir>/account-credentials.json`; never write it to `oauth-tokens.json`.
- A non-empty explicit API key returns before credential-file access, metadata discovery, or OAuth refresh.
- Precedence is raw request `Authorization`, explicit API key, stored account credential, then not signed in.
- Stored credentials are origin-confined; settlement accepts static API keys only.
- Share one `Arc<CredentialManager>` between provider authentication and account telemetry in an assembled application.
- Preserve legacy untagged OAuth JSON, tagged credential JSON, atomic writes, and Unix mode `0600`.
- Do not add Cloud APIs to `bitrouter-sdk` or create another Cloud crate.
- Public modules do not re-export items defined in sibling public modules.
- Do not add `#[allow(...)]`, `unwrap`, `expect`, `panic`, dead code, non-English source comments, or private Cloud implementation references.
- Every production behavior change follows RED, verified failure, GREEN, verified pass, then refactor.
- Use Conventional Commits and leave unrelated work untouched.

---

### Task 1: Hosted account credential foundation

**Files:**
- Modify: `crates/bitrouter-providers/Cargo.toml`
- Modify: `crates/bitrouter-providers/src/lib.rs`
- Create: `crates/bitrouter-providers/src/hosted/mod.rs`
- Create: `crates/bitrouter-providers/src/hosted/account/{mod.rs,credentials.rs,flow.rs,metadata.rs,settings.rs,manager.rs}`
- Test: unit tests in the new account modules

**Interfaces:**
- Consumes: credential schema and RFC flows in `crates/bitrouter-cloud-sdk/src/auth/`.
- Produces: defining paths under `hosted::account` and `manager::{CredentialError, CredentialManager, CredentialSource, ResolvedCredential}`.

- [ ] **Step 1: Write RED manager tests**

Add tests for the desired API before declaring the hosted module:

```rust
#[tokio::test]
async fn explicit_api_key_bypasses_malformed_store() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("account-credentials.json");
    std::fs::write(&path, b"not json")?;
    let manager = CredentialManager::with_client(path, reqwest::Client::new());
    let resolved = manager
        .resolve_bearer(Some("brk_explicit.secret"), Some("https://api.bitrouter.ai/v1"))
        .await?;
    assert_eq!(resolved.secret(), "brk_explicit.secret");
    assert_eq!(resolved.source(), CredentialSource::ExplicitApiKey);
    Ok(())
}

#[tokio::test]
async fn api_key_only_rejects_oauth() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let manager = CredentialManager::with_client(
        directory.path().join("account-credentials.json"),
        reqwest::Client::new(),
    );
    let credential = Credentials {
        access_token: "access-token".to_owned(),
        refresh_token: Some("refresh-token".to_owned()),
        expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
        refresh_token_expires_at: None,
        token_type: "Bearer".to_owned(),
        scope: "inference:invoke".to_owned(),
        client_id: "bitrouter-cli".to_owned(),
        authorization_server: "https://api.bitrouter.ai".to_owned(),
        namespace_id: Some("ns-test".to_owned()),
        subject: None,
    };
    manager.save(StoredCredential::from(credential)).await?;
    let error = match manager.resolve_api_key(None, Some("https://api.bitrouter.ai/v1")).await {
        Ok(_) => anyhow::bail!("OAuth unexpectedly resolved as an API key"),
        Err(error) => error,
    };
    assert!(matches!(error, CredentialError::WrongCredentialKind));
    Ok(())
}

#[tokio::test]
async fn stored_credential_is_origin_confined() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let manager = CredentialManager::with_client(
        directory.path().join("account-credentials.json"),
        reqwest::Client::new(),
    );
    manager
        .save(StoredCredential::api_key(
            "brk_stored.secret".to_owned(),
            "https://api.bitrouter.ai".to_owned(),
        ))
        .await?;
    let error = match manager.resolve_bearer(None, Some("https://example.com/v1")).await {
        Ok(_) => anyhow::bail!("credential unexpectedly crossed origins"),
        Err(error) => error,
    };
    assert!(matches!(error, CredentialError::OriginMismatch { .. }));
    Ok(())
}
```

Also adapt the existing credential tests for legacy/tagged JSON, permissions,
atomic persistence, refresh rotation, and redacted `Debug`.

- [ ] **Step 2: Verify RED**

Run `cargo test -p bitrouter-providers --features hosted hosted::account`.

Expected: compilation fails because the `hosted` feature and modules do not
exist, rather than because of a malformed fixture.

- [ ] **Step 3: Add the feature and protocol modules**

Keep `default = []` and add:

```toml
hosted = ["dep:anyhow", "dep:base64", "dep:chrono", "dep:rand", "dep:url", "tokio/sync"]
```

Make `anyhow` and `chrono` optional and reuse the existing optional dependency
entries. Move the four old auth primitive files to their exact account paths,
adjust module-relative imports, and preserve their tests. Declare modules only:

```rust
// hosted/mod.rs
pub mod account;

// hosted/account/mod.rs
pub mod credentials;
pub mod flow;
pub mod manager;
pub mod metadata;
pub mod settings;

// lib.rs
#[cfg(feature = "hosted")]
pub mod hosted;
```

- [ ] **Step 4: Implement `CredentialManager`**

Use these public types and signatures:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource { ExplicitApiKey, StoredApiKey, StoredOauth }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OauthIdentity {
    authorization_server: String,
    namespace_id: Option<String>,
}

pub struct ResolvedCredential {
    secret: String,
    source: CredentialSource,
    oauth_identity: Option<OauthIdentity>,
}

impl ResolvedCredential {
    pub fn secret(&self) -> &str;
    pub fn source(&self) -> CredentialSource;
    pub fn oauth_identity(&self) -> Option<&OauthIdentity>;
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("no BitRouter Cloud account credential is stored")]
    NotSignedIn,
    #[error("could not access BitRouter Cloud credential store: {0}")]
    Store(String),
    #[error("could not discover BitRouter Cloud authorization metadata: {0}")]
    Metadata(String),
    #[error("could not refresh BitRouter Cloud OAuth credential: {0}")]
    Refresh(String),
    #[error("stored BitRouter Cloud credential origin {actual} does not match {expected}")]
    OriginMismatch { expected: String, actual: String },
    #[error("this operation requires a static BitRouter API key; the stored credential is OAuth")]
    WrongCredentialKind,
}

impl CredentialManager {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, CredentialError>;
    pub fn with_client(path: impl Into<PathBuf>, client: reqwest::Client) -> Self;
    pub fn path(&self) -> &Path;
    pub async fn current(&self) -> Result<Option<StoredCredential>, CredentialError>;
    pub async fn save(&self, credential: StoredCredential) -> Result<(), CredentialError>;
    pub async fn clear(&self) -> Result<Option<StoredCredential>, CredentialError>;
    pub async fn resolve_bearer(&self, explicit_api_key: Option<&str>, expected_origin: Option<&str>) -> Result<ResolvedCredential, CredentialError>;
    pub async fn resolve_api_key(&self, explicit_api_key: Option<&str>, expected_origin: Option<&str>) -> Result<ResolvedCredential, CredentialError>;
}
```

The manager owns path, HTTP client, metadata cache keyed by AS origin, and one
async gate. Both resolution methods test a non-empty explicit key before any
file access. Stored resolution acquires the gate, reloads from disk, checks
scheme/host/effective port, fetches metadata only for OAuth, refreshes, and
persists rotation. API-key-only mode rejects OAuth before metadata or refresh.
`current`, `save`, and `clear` use the same gate. All secret-bearing `Debug`
implementations redact. `ResolvedCredential` carries only the resolved secret
plus non-secret OAuth identity metadata; it must not also clone the complete
stored credential and thereby retain a second token set in memory.

- [ ] **Step 5: Verify GREEN and feature isolation**

Run:

```text
env -u http_proxy -u https_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY cargo test -p bitrouter-providers --features hosted hosted::account
cargo check -p bitrouter-providers
cargo check -p bitrouter-providers --features hosted
```

Expected: tests pass and both feature configurations compile.

- [ ] **Step 6: Commit**

```text
git add crates/bitrouter-providers
git commit -m "feat(providers): add hosted credential manager"
```

---

### Task 2: Hosted AuthApplier and shared application integration

**Files:**
- Create: `crates/bitrouter-providers/src/hosted/applier.rs`
- Modify: `crates/bitrouter-providers/src/hosted/mod.rs`
- Modify: `apps/bitrouter/{Cargo.toml,src/cloud/mod.rs,src/assemble.rs,src/acp_cli.rs}`
- Test: unit tests in the provider applier and app Cloud modules

**Interfaces:**
- Consumes: Task 1 manager and resolved credential source.
- Produces: `hosted::applier::{BitrouterAuthApplier, PROVIDER_ID, onboarding_hint}` and manager-accepting app adapters.

- [ ] **Step 1: Write RED precedence and concurrency tests**

Move/adapt old applier tests, changing `tmp_creds_path` and `empty_request` to
return `anyhow::Result` so their callers use `?`, and add:

```rust
#[tokio::test]
async fn preserves_request_authorization_header() -> anyhow::Result<()> {
    let path = tmp_creds_path("raw-authorization")?;
    std::fs::write(&path, b"malformed")?;
    let manager = CredentialManager::with_client(path, reqwest::Client::new());
    let applier = BitrouterAuthApplier::new(Arc::new(manager));
    let mut request = empty_request()?;
    request.headers_mut().insert(AUTHORIZATION, HeaderValue::from_static("Bearer raw-request-token"));
    let applied = applier.apply(request, &target_with_api_key("brk_config.secret")).await?;
    assert_eq!(applied.headers()[AUTHORIZATION], "Bearer raw-request-token");
    Ok(())
}

#[tokio::test]
async fn explicit_target_key_bypasses_bad_oauth_store() -> anyhow::Result<()> {
    let path = tmp_creds_path("explicit-bypass")?;
    std::fs::write(&path, b"malformed")?;
    let manager = CredentialManager::with_client(path, reqwest::Client::new());
    let applier = BitrouterAuthApplier::new(Arc::new(manager));
    let applied = applier.apply(empty_request()?, &target_with_api_key("brk_config.secret")).await?;
    assert_eq!(applied.headers()[AUTHORIZATION], "Bearer brk_config.secret");
    Ok(())
}
```

Add an app test that gives one manager to applier and telemetry, concurrently
resolves a near-expiry OAuth credential, asserts one wiremock refresh, and
asserts the rotated token is persisted.

- [ ] **Step 2: Verify RED**

Run `cargo test -p bitrouter-providers --features hosted hosted::applier` and
`cargo test -p bitrouter cloud::tests`.

Expected: compilation fails because the new applier and manager-accepting app
adapters do not exist.

- [ ] **Step 3: Implement the provider applier**

```rust
pub const PROVIDER_ID: &str = "bitrouter";

pub struct BitrouterAuthApplier { manager: Arc<CredentialManager> }

impl BitrouterAuthApplier {
    pub fn new(manager: Arc<CredentialManager>) -> Self;
}
```

Preserve an existing request `Authorization` header and return unproven auth.
Otherwise pass `target.effective_api_key()` and the actual request URL to the
manager. Preserve separate continuation-authority derivation for explicit key,
stored key, and OAuth namespace. Map missing/refresh to 401, metadata to 502,
and store corruption to internal error. Declare `pub mod applier;` without a
re-export.

- [ ] **Step 4: Inject one manager into provider and telemetry**

Enable `features = ["hosted", "pkce"]` for the app provider dependency. Use:

```rust
fn build_auth_appliers(config: &Config, cloud_manager: Arc<CredentialManager>) -> Result<AuthAppliers>;
pub fn register_if_configured(config: &Config, appliers: &mut AuthAppliers, manager: Arc<CredentialManager>) -> Result<()>;

pub struct CloudBearer {
    manager: Arc<CredentialManager>,
    expected_origin: String,
}
```

`build_app_with_path` creates the default manager once and passes the same
`Arc` to auth appliers and telemetry. `CloudBearer::bearer` resolves with the
export origin and maps all failures to `None`. Keep the existing ACP explicit
environment-key check before stored fallback. Standalone surfaces construct at
most one manager for their Cloud consumer.

- [ ] **Step 5: Verify GREEN**

Run:

```text
env -u http_proxy -u https_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY cargo test -p bitrouter-providers --features hosted hosted::applier
env -u http_proxy -u https_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY cargo test -p bitrouter cloud::tests
```

Expected: precedence, origin, shared refresh, persistence, and error mapping pass.

- [ ] **Step 6: Commit**

```text
git add crates/bitrouter-providers apps/bitrouter
git commit -m "refactor: share cloud credentials across auth consumers"
```

---

### Task 3: Move Cloud commands and HTTP clients into the application

**Files:**
- Create: `apps/bitrouter/src/cloud/auth.rs`
- Create: `apps/bitrouter/src/cloud/api_client.rs`
- Create: `apps/bitrouter/src/cloud/management/{mod.rs,error.rs,billing.rs,budgets.rs,byok.rs,keys.rs,namespaces.rs,oauth_clients.rs,policies.rs,presets.rs,types.rs,usage.rs}`
- Modify: `apps/bitrouter/src/cloud/{mod.rs,api.rs,cli.rs}`
- Modify: `apps/bitrouter/src/onboarding.rs`
- Modify: `apps/bitrouter/tests/cloud_api.rs`
- Test: app Cloud unit and integration tests

**Interfaces:**
- Consumes: Task 1 manager/account modules and Task 2 adapters.
- Produces: app-owned `auth`, `api_client`, and `management` modules with no SDK imports in these consumers.

- [ ] **Step 1: Write RED manager-owned login and client tests**

Add a test for replacement through the manager:

```rust
#[tokio::test]
async fn api_key_login_replaces_oauth_in_manager_store() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let manager = Arc::new(CredentialManager::with_client(
        directory.path().join("account-credentials.json"),
        reqwest::Client::new(),
    ));
    manager
        .save(StoredCredential::from(Credentials {
            access_token: "oauth-access".to_owned(),
            refresh_token: Some("oauth-refresh".to_owned()),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
            refresh_token_expires_at: None,
            token_type: "Bearer".to_owned(),
            scope: "inference:invoke".to_owned(),
            client_id: "bitrouter-cli".to_owned(),
            authorization_server: "https://api.bitrouter.ai".to_owned(),
            namespace_id: Some("ns-test".to_owned()),
            subject: None,
        }))
        .await?;
    let stored = login_api_key(
        Arc::clone(&manager),
        "brk_replacement.secret".to_owned(),
        "https://api.bitrouter.ai".to_owned(),
    )
    .await?;
    assert_eq!(stored.kind(), CredentialKind::ApiKey);
    let current = match manager.current().await? {
        Some(current) => current,
        None => anyhow::bail!("credential disappeared"),
    };
    assert_eq!(current.kind(), CredentialKind::ApiKey);
    Ok(())
}
```

Change `apps/bitrouter/tests/cloud_api.rs` imports to the desired app-owned
module path before the modules exist. Do not add facade re-exports.

- [ ] **Step 2: Verify RED**

Run `cargo test -p bitrouter cloud::auth` and
`cargo test -p bitrouter --test cloud_api`.

Expected: compilation fails because app-owned auth/client modules and login
functions do not exist.

- [ ] **Step 3: Move login orchestration into `auth.rs`**

Use:

```rust
#[derive(Default, Clone)]
pub struct LoginInputs {
    pub authorization_server: Option<String>,
    pub client_id: Option<String>,
    pub scope: Option<String>,
    pub api_key: Option<String>,
}

pub async fn login(manager: Arc<CredentialManager>, inputs: LoginInputs) -> anyhow::Result<StoredCredential>;
pub async fn logout(manager: Arc<CredentialManager>, inputs: LoginInputs) -> anyhow::Result<()>;
pub async fn login_api_key(manager: Arc<CredentialManager>, api_key: String, base_url: String) -> anyhow::Result<StoredCredential>;
```

Move device-flow orchestration and presentation from the old commands module.
All current/save/clear operations go through the injected manager. Preserve API
key and secure-origin validation, best-effort OAuth revocation, offline
`whoami`, and secret redaction. Cloud CLI and onboarding each construct one
manager for the invocation; the provider-login alias calls the same `login`.
When relocating existing tests, convert their panic-producing setup helpers to
`Result`-returning helpers and `?`; moving an existing `.unwrap`, `.expect`, or
`panic!` into a new module is not permitted.

- [ ] **Step 4: Move raw API and management clients**

Move the old raw API client to `api_client.rs` and every management file to the
listed app directory. Replace embedded `CredentialsStore` instances with an
`Arc<CredentialManager>` and async constructors:

```rust
impl CloudApiClient {
    pub async fn from_manager(manager: Arc<CredentialManager>) -> anyhow::Result<Self>;
}

impl ManagementClient {
    pub async fn from_manager(manager: Arc<CredentialManager>) -> Result<Self, management::error::Error>;
}
```

Derive base URL and namespace from `manager.current().await`; resolve a fresh,
origin-confined bearer for each request. Preserve redirects-disabled raw HTTP,
streaming, redaction, raw `Authorization` precedence, all wire types, and all
management error behavior. Update imports in Cloud CLI/API, onboarding, and
integration tests; remove copied SDK/private rustdoc.

- [ ] **Step 5: Verify GREEN and imports**

Run:

```text
env -u http_proxy -u https_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY cargo test -p bitrouter cloud::
env -u http_proxy -u https_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY cargo test -p bitrouter --test cloud_api
! rg -n 'bitrouter_cloud_sdk|bitrouter-cloud-sdk' apps/bitrouter/src/cloud apps/bitrouter/src/onboarding.rs apps/bitrouter/tests/cloud_api.rs
```

Expected: Cloud auth, CLI formatting, API, management, onboarding, and
integration tests pass; the negated search exits 0.

- [ ] **Step 6: Commit**

```text
git add apps/bitrouter
git commit -m "refactor(cli): own cloud clients in the application"
```

---

### Task 4: Move settlement and enforce API-key-only resolution

**Files:**
- Create: `apps/bitrouter/src/cloud/settlement.rs`
- Modify: `apps/bitrouter/src/cloud/mod.rs`
- Modify: `apps/bitrouter/src/main.rs`
- Modify: `apps/bitrouter/src/metering/{reconciliation.rs,store.rs,tests.rs}`
- Modify: `apps/bitrouter/src/workflow_state/archive.rs`
- Modify: `docs/CLI.md`
- Modify: `skills/bitrouter/references/metering.md`
- Test: main, settlement, and metering tests

**Interfaces:**
- Consumes: Task 1 `CredentialManager::resolve_api_key`.
- Produces: `crate::cloud::settlement::{SettlementClient, SettlementError, SettlementReceipt, SettlementState, SettlementUsage}` and an API-key-only CLI resolver.

- [ ] **Step 1: Write RED settlement-priority tests**

Replace the old OAuth-success expectation with:

```rust
#[tokio::test]
async fn settlement_explicit_key_bypasses_malformed_credentials_file() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("account-credentials.json");
    std::fs::write(&path, b"malformed")?;
    let key = settlement_api_key(Some("brk_explicit.secret"), Some(&path), "https://api.bitrouter.ai/v1").await?;
    assert_eq!(key, "brk_explicit.secret");
    Ok(())
}

#[tokio::test]
async fn settlement_credentials_file_rejects_oauth() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("account-credentials.json");
    let manager = CredentialManager::with_client(&path, reqwest::Client::new());
    manager
        .save(StoredCredential::from(Credentials {
            access_token: "oauth-access".to_owned(),
            refresh_token: Some("oauth-refresh".to_owned()),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
            refresh_token_expires_at: None,
            token_type: "Bearer".to_owned(),
            scope: "inference:invoke".to_owned(),
            client_id: "bitrouter-cli".to_owned(),
            authorization_server: "https://api.bitrouter.ai".to_owned(),
            namespace_id: Some("ns-test".to_owned()),
            subject: None,
        }))
        .await?;
    let error = match settlement_api_key(None, Some(&path), "https://api.bitrouter.ai/v1").await {
        Ok(_) => anyhow::bail!("OAuth unexpectedly resolved for settlement"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("requires a static BitRouter API key"));
    Ok(())
}

#[tokio::test]
async fn settlement_credentials_file_rejects_wrong_origin() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("account-credentials.json");
    let manager = CredentialManager::with_client(&path, reqwest::Client::new());
    manager
        .save(StoredCredential::api_key(
            "brk_stored.secret".to_owned(),
            "https://other.example".to_owned(),
        ))
        .await?;
    let error = match settlement_api_key(None, Some(&path), "https://api.bitrouter.ai/v1").await {
        Ok(_) => anyhow::bail!("credential unexpectedly crossed origins"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("does not match"));
    Ok(())
}
```

- [ ] **Step 2: Verify RED**

Run `cargo test -p bitrouter settlement_`.

Expected: the old implementation accepts OAuth and lacks the explicit-key
bypass helper, so tests fail for the intended behavior gap.

- [ ] **Step 3: Move settlement types and client**

Move the old settlement implementation to the app module, declare it in
`cloud/mod.rs`, and update metering/workflow imports to
`crate::cloud::settlement`. Preserve wire formats, retry/status behavior, and
wiremock tests.

- [ ] **Step 4: Implement API-key-only CLI resolution**

Use:

```rust
async fn settlement_api_key(
    explicit_api_key: Option<&str>,
    credentials_file: Option<&Path>,
    api_base: &str,
) -> Result<String>;
```

Return a non-empty explicit key immediately. Otherwise require a credential
path, create a path-specific manager, call
`resolve_api_key(None, Some(api_base))`, and clone its secret. The command reads
the selected environment variable before calling the helper; missing/empty
falls through to the optional file. If neither exists, return a clear error
naming the environment variable. Change help, `docs/CLI.md`, and
`skills/bitrouter/references/metering.md` to state that the file must contain a
static key and OAuth is never refreshed for settlement.

- [ ] **Step 5: Verify GREEN**

Run:

```text
env -u http_proxy -u https_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY cargo test -p bitrouter settlement_
env -u http_proxy -u https_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY cargo test -p bitrouter metering
```

Expected: explicit bypass, OAuth rejection, origin rejection, settlement wire
behavior, and metering reconciliation pass.

- [ ] **Step 6: Commit**

```text
git add apps/bitrouter/src apps/bitrouter/tests docs/CLI.md skills/bitrouter/references/metering.md
git commit -m "refactor: make settlement api-key only"
```

---

### Task 5: Decouple MCP and delete `bitrouter-cloud-sdk`

**Files:**
- Modify: `crates/bitrouter-mcp/{Cargo.toml,src/backend/cloud.rs}`
- Modify: `apps/bitrouter/Cargo.toml`
- Modify: `Cargo.toml`, `Cargo.lock`, `release-plz.toml`, `docs/DEVELOPMENT.md`
- Delete: `crates/bitrouter-cloud-sdk/`
- Test: MCP Cloud backend and manifest/reference searches

**Interfaces:**
- Consumes: Tasks 1-4 leave no required live consumer in the old crate.
- Produces: a workspace without the obsolete package and an MCP-local billing response.

- [ ] **Step 1: Write the RED MCP wire test**

```rust
#[test]
fn billing_balance_response_decodes_locally() -> anyhow::Result<()> {
    let response: BillingBalanceResponse = serde_json::from_value(serde_json::json!({
        "available_micro_usd": 11,
        "balance_micro_usd": 17,
        "pending_debits_micro_usd": 6
    }))?;
    assert_eq!(response.available_micro_usd, 11);
    assert_eq!(response.balance_micro_usd, 17);
    assert_eq!(response.pending_debits_micro_usd, 6);
    Ok(())
}
```

- [ ] **Step 2: Verify RED**

Run `cargo test -p bitrouter-mcp billing_balance_response_decodes_locally`.

Expected: compilation fails because the local type is not defined.

- [ ] **Step 3: Add the MCP-local response**

```rust
#[derive(Debug, serde::Deserialize)]
struct BillingBalanceResponse {
    available_micro_usd: i64,
    balance_micro_usd: i64,
    pending_debits_micro_usd: i64,
}
```

Use it in `status`, remove the MCP manifest dependency, then run
`cargo test -p bitrouter-mcp backend::cloud` and expect all tests to pass.

- [ ] **Step 4: Delete crate and dependency metadata**

Delete `crates/bitrouter-cloud-sdk` with `apply_patch`, remove its workspace and
app dependencies, release-plz entry, and stale architecture comments. Run
`cargo check --workspace --all-features` to regenerate the lockfile rather than
editing registry checksums manually.

- [ ] **Step 5: Prove cleanup and focused suites**

Run:

```text
test ! -d crates/bitrouter-cloud-sdk
! rg -n 'bitrouter_cloud_sdk' --glob '*.rs' apps crates
! rg -n 'bitrouter-cloud-sdk' Cargo.toml Cargo.lock release-plz.toml apps crates docs/CLI.md docs/DEVELOPMENT.md
cargo test -p bitrouter-providers --features hosted
env -u http_proxy -u https_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY cargo test -p bitrouter --lib cloud::
cargo test -p bitrouter-mcp
```

Expected: negated searches exit 0 and focused suites pass. Historical
`CHANGELOG.md` entries may retain the released name.

- [ ] **Step 6: Commit**

```text
git add Cargo.toml Cargo.lock release-plz.toml apps crates docs/DEVELOPMENT.md
git commit -m "refactor: remove bitrouter cloud sdk crate"
```

---

### Task 6: Repository-wide verification and final review readiness

**Files:**
- Modify only files required to fix failures caused by Tasks 1-5
- Test: complete workspace

**Interfaces:**
- Consumes: fully migrated workspace.
- Produces: formatter, tests, lints, rustdoc, and audit evidence for final review.

- [ ] **Step 1: Format and inspect**

Run:

```text
cargo fmt --all
git diff --check
git status --short
git diff --stat origin/main...HEAD
```

Expected: no whitespace errors and only planned files.

- [ ] **Step 2: Run every required gate**

```text
cargo check -p bitrouter-providers
cargo test -p bitrouter-providers --features hosted
env -u http_proxy -u https_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY cargo nextest run --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
```

Expected: every command exits 0 with no warnings promoted to errors.

- [ ] **Step 3: Audit requirements**

```text
test ! -d crates/bitrouter-cloud-sdk
! rg -n 'bitrouter_cloud_sdk' --glob '*.rs' apps crates
! rg -n 'bitrouter-cloud-sdk' Cargo.toml Cargo.lock release-plz.toml apps crates docs/CLI.md docs/DEVELOPMENT.md
git grep -n 'oauth-tokens.json' -- crates/bitrouter-providers apps/bitrouter | cat
git grep -n 'account-credentials.json' -- crates/bitrouter-providers apps/bitrouter | cat
```

Inspect the final two outputs: Cloud account code names only
`account-credentials.json`; generic upstream auth may name only its own store.

- [ ] **Step 4: Commit verification fixes when present**

For any failure, write and observe a focused failing regression test before the
fix, then rerun the focused test. If source changed, commit:

```text
git add -A
git commit -m "fix: complete cloud sdk consolidation"
```

Do not create an empty commit. Record all commands/results in the task report.
