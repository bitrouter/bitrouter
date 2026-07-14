# BitRouter Cloud API Command Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `gh api`-style `bitrouter cloud api` command and first-class `bitrouter cloud login --api-key` credentials, with streaming inference support, complete tests, and bilingual documentation.

**Architecture:** `bitrouter-cloud-sdk` owns compatible credential persistence and an origin-confined raw HTTP client. `apps/bitrouter/src/cloud/api.rs` owns CLI field parsing, request assembly, and terminal output, while the existing cloud CLI only declares and dispatches arguments. Existing provider, telemetry, and management consumers switch to shared credential accessors.

**Tech Stack:** Rust 2024, clap 4, reqwest 0.13, tokio 1, serde/serde_json, wiremock 0.6, anyhow/thiserror.

## Global Constraints

- Work only on `codex/cloud-api` in `/Users/archer/code/bitrouter/bitrouter-ws/repos/bitrouter-cloud-api`.
- Never use `#[allow(...)]`, `.unwrap()`, `.expect()`, or `panic!()` in production code.
- Keep all code, comments, commit messages, and public documentation source in English; maintain product-doc English and Simplified-Chinese siblings in lockstep.
- Preserve legacy untagged OAuth credential files and Unix mode `0600` persistence.
- Never print or debug-render OAuth tokens or API keys.
- Never send a stored credential to an origin other than the login origin; disable redirects.
- Preserve response bytes for non-TTY pipelines and stream SSE without whole-body buffering.
- Keep `skills/bitrouter/` synchronized with every added command and flag.
- Use conventional commit headers shorter than 60 characters.

---

### Task 1: Compatible stored credential variants

**Files:**
- Modify: `crates/bitrouter-cloud-sdk/src/auth/credentials.rs`
- Modify: `crates/bitrouter-cloud-sdk/src/auth/flow.rs`
- Modify: `crates/bitrouter-cloud-sdk/src/auth/commands.rs`
- Test: `crates/bitrouter-cloud-sdk/src/auth/credentials.rs`

**Interfaces:**
- Consumes: existing OAuth `Credentials`, `CredentialsStore`, and refresh flow.
- Produces: `StoredCredential`, `CredentialKind`, `StoredCredential::base_url`, `oauth`, `namespace_id`, `scope`, `subject`, and `CredentialsStore::current_token(client, Option<&AsMetadata>)`.

- [ ] **Step 1: Add failing credential compatibility and redaction tests**

Add tests that express the intended public API:

```rust
#[test]
fn tagged_api_key_round_trips_and_redacts() {
    let path = tmp_dir("api-key").join(DEFAULT_FILENAME);
    let credential = StoredCredential::api_key(
        "brk_AAAAAAAAAAAAAAAA.secret-value".to_owned(),
        "https://api.bitrouter.ai".to_owned(),
    );
    let mut store = CredentialsStore::load(&path).unwrap();
    store.save(credential).unwrap();

    let reloaded = CredentialsStore::load(&path).unwrap();
    let current = reloaded.current().unwrap();
    assert_eq!(current.kind(), CredentialKind::ApiKey);
    assert_eq!(current.base_url(), "https://api.bitrouter.ai");
    let rendered = format!("{current:?}");
    assert!(!rendered.contains("secret-value"));
}

#[test]
fn legacy_untagged_oauth_file_still_loads() {
    let path = tmp_dir("legacy").join(DEFAULT_FILENAME);
    std::fs::write(&path, serde_json::to_vec(&sample_credentials()).unwrap()).unwrap();

    let store = CredentialsStore::load(path).unwrap();
    let current = store.current().unwrap();
    assert_eq!(current.kind(), CredentialKind::Oauth);
    assert_eq!(current.oauth().unwrap().access_token, "AT");
}

#[tokio::test]
async fn api_key_current_token_needs_no_metadata() {
    let path = tmp_dir("api-key-token").join(DEFAULT_FILENAME);
    let mut store = CredentialsStore::load(path).unwrap();
    store
        .save(StoredCredential::api_key(
            "brk_AAAAAAAAAAAAAAAA.secret".to_owned(),
            "https://api.bitrouter.ai".to_owned(),
        ))
        .unwrap();

    let token = store
        .current_token(&reqwest::Client::new(), None)
        .await
        .unwrap();
    assert_eq!(token, "brk_AAAAAAAAAAAAAAAA.secret");
}
```

- [ ] **Step 2: Run the credential tests and verify RED**

Run:

```bash
cargo test -p bitrouter-cloud-sdk auth::credentials::tests --all-features
```

Expected: compilation fails because `StoredCredential`, `CredentialKind`, and the optional-metadata token interface do not exist.

- [ ] **Step 3: Implement the tagged credential model and legacy reader**

Keep `Credentials` as the OAuth payload to minimize refresh-flow churn. Add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    Oauth,
    ApiKey,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredCredential {
    Oauth {
        #[serde(flatten)]
        credential: Credentials,
    },
    ApiKey {
        api_key: String,
        base_url: String,
    },
}

impl From<Credentials> for StoredCredential {
    fn from(credential: Credentials) -> Self {
        Self::Oauth { credential }
    }
}

impl StoredCredential {
    pub fn api_key(api_key: String, base_url: String) -> Self {
        Self::ApiKey { api_key, base_url }
    }

    pub fn kind(&self) -> CredentialKind {
        match self {
            Self::Oauth { .. } => CredentialKind::Oauth,
            Self::ApiKey { .. } => CredentialKind::ApiKey,
        }
    }

    pub fn base_url(&self) -> &str {
        match self {
            Self::Oauth { credential } => &credential.authorization_server,
            Self::ApiKey { base_url, .. } => base_url,
        }
    }

    pub fn oauth(&self) -> Option<&Credentials> {
        match self {
            Self::Oauth { credential } => Some(credential),
            Self::ApiKey { .. } => None,
        }
    }

    pub fn namespace_id(&self) -> Option<&str> {
        self.oauth().and_then(|credential| credential.namespace_id.as_deref())
    }

    pub fn scope(&self) -> Option<&str> {
        self.oauth().map(|credential| credential.scope.as_str())
    }

    pub fn subject(&self) -> Option<&str> {
        self.oauth().and_then(|credential| credential.subject.as_deref())
    }
}
```

Implement custom `Deserialize` by first trying a tagged helper enum and then the existing OAuth `Credentials` object. Implement custom `Debug` so both variants show only `<redacted>` for secret fields. Change `CredentialsStore.current` to `Option<StoredCredential>`, make `save` accept `impl Into<StoredCredential>`, and branch in `current_token`: API keys return immediately; OAuth requires metadata and retains the existing refresh implementation.

- [ ] **Step 4: Update OAuth construction and command return types**

Keep `flow::credentials_from_token_set` returning OAuth `Credentials`; wrap it at the storage boundary. Change login to return `StoredCredential`, saving OAuth via `StoredCredential::from(credentials)`. Update logout to unwrap the OAuth payload only for RFC 7009 revocation.

- [ ] **Step 5: Run credential and OAuth tests and verify GREEN**

Run:

```bash
cargo test -p bitrouter-cloud-sdk auth:: --all-features
cargo test -p bitrouter-cloud-sdk --test oauth_device_flow --all-features
```

Expected: all credential and OAuth tests pass, including the legacy file test.

- [ ] **Step 6: Commit the credential model**

```bash
git add crates/bitrouter-cloud-sdk/src/auth
git commit -m "feat(cloud): store API key credentials"
```

---

### Task 2: API-key login and existing credential consumers

**Files:**
- Modify: `crates/bitrouter-cloud-sdk/src/auth/commands.rs`
- Modify: `crates/bitrouter-cloud-sdk/src/provider/applier.rs`
- Modify: `crates/bitrouter-cloud-sdk/src/management/mod.rs`
- Modify: `apps/bitrouter/src/cloud/cli.rs`
- Modify: `apps/bitrouter/src/cloud/mod.rs`
- Modify: `apps/bitrouter/src/main.rs`
- Test: the corresponding inline test modules and `crates/bitrouter-cloud-sdk/tests/oauth_device_flow.rs`

**Interfaces:**
- Consumes: Task 1 `StoredCredential` accessors and optional-metadata bearer resolution.
- Produces: `LoginInputs.api_key`, API-key login validation, safe logout/whoami behavior, provider/management/telemetry reuse.

- [ ] **Step 1: Add failing login and consumer tests**

Add tests for pure key parsing and no-network storage:

```rust
#[test]
fn validates_brk_api_key_shape() {
    assert!(validate_api_key("brk_AAAAAAAAAAAAAAAA.secret").is_ok());
    assert!(validate_api_key("sk-not-bitrouter").is_err());
    assert!(validate_api_key("brk_missing-dot").is_err());
    assert!(validate_api_key("brk_.secret").is_err());
    assert!(validate_api_key("brk_token.").is_err());
}

#[tokio::test]
async fn api_key_login_persists_without_discovery() {
    let path = tmp_credentials_path("login-api-key");
    let credential = login_api_key_at_path(
        "brk_AAAAAAAAAAAAAAAA.secret".to_owned(),
        "https://api.bitrouter.ai".to_owned(),
        &path,
    )
    .unwrap();
    assert_eq!(credential.kind(), CredentialKind::ApiKey);
    assert_eq!(CredentialsStore::load(path).unwrap().current().unwrap().kind(), CredentialKind::ApiKey);
}
```

Add provider and management tests asserting a stored API key is sent without an OAuth metadata request and namespaced management uses `/v1/namespaces/me/keys`. Add a telemetry test asserting the stored API key resolves as a bearer.

- [ ] **Step 2: Run targeted tests and verify RED**

```bash
cargo test -p bitrouter-cloud-sdk api_key --all-features
cargo test -p bitrouter cloud:: --all-features
```

Expected: tests fail because the login flag, helpers, and consumer branches are absent.

- [ ] **Step 3: Implement API-key login**

Extend `LoginInputs`:

```rust
pub struct LoginInputs {
    pub authorization_server: Option<String>,
    pub client_id: Option<String>,
    pub scope: Option<String>,
    pub api_key: Option<String>,
}
```

At the top of `login`, reject `api_key` combined with `client_id` or `scope`, resolve the base URL through the existing secure URL resolver, validate `brk_<token_id>.<secret>`, and persist without constructing an HTTP client. Extract `login_api_key_at_path` so tests never mutate the process-wide data directory. Never render the key in success output.

- [ ] **Step 4: Declare the clap flag and safe identity output**

Add to `CloudAction::Login`:

```rust
#[arg(long, value_name = "BRK_API_KEY", conflicts_with_all = ["client_id", "scope"])]
api_key: Option<String>,
```

Pass it into `LoginInputs`. Update `whoami` JSON/text output with `authentication: oauth|api_key`; only OAuth emits namespace, scope, subject, and expiry. Match API-key logout before metadata discovery and clear it locally.

- [ ] **Step 5: Adapt provider, management, telemetry, and aliases**

For provider and telemetry consumers, resolve metadata only when `current().oauth()` is present, then call `current_token` with `metadata.as_ref()`. In `ManagementClient`, derive `base_url` from `StoredCredential::base_url`; keep a `CredentialKind` and make `namespaced` use `me` for API-key credentials while preserving `NoNamespace` for legacy OAuth credentials without a namespace.

Update every `LoginInputs` literal, including `providers login bitrouter`, with `api_key: None`.

- [ ] **Step 6: Run consumer suites and verify GREEN**

```bash
cargo test -p bitrouter-cloud-sdk --all-features
cargo test -p bitrouter cloud:: --all-features
```

Expected: all previous OAuth tests and new API-key consumer tests pass.

- [ ] **Step 7: Commit login and consumer support**

```bash
git add crates/bitrouter-cloud-sdk apps/bitrouter/src/cloud apps/bitrouter/src/main.rs
git commit -m "feat(cloud): login with API keys"
```

---

### Task 3: Origin-confined raw Cloud API client

**Files:**
- Create: `crates/bitrouter-cloud-sdk/src/api.rs`
- Modify: `crates/bitrouter-cloud-sdk/src/lib.rs`
- Modify: `crates/bitrouter-cloud-sdk/Cargo.toml`
- Test: `crates/bitrouter-cloud-sdk/src/api.rs`

**Interfaces:**
- Consumes: Task 1 stored bearer and base URL.
- Produces: `CloudApiClient`, `ApiRequest`, and `ApiResponse` with a streaming `reqwest::Response` body.

- [ ] **Step 1: Add failing URL and wire tests**

Define tests against the wished-for interface:

```rust
#[test]
fn resolves_only_relative_paths_on_the_login_origin() {
    let base = url::Url::parse("https://api.bitrouter.ai/oauth").unwrap();
    assert_eq!(
        resolve_endpoint(&base, "/v1/models?owned=true").unwrap().as_str(),
        "https://api.bitrouter.ai/v1/models?owned=true"
    );
    assert!(resolve_endpoint(&base, "https://evil.example/v1/models").is_err());
    assert!(resolve_endpoint(&base, "//evil.example/v1/models").is_err());
    assert!(resolve_endpoint(&base, "/v1/models#fragment").is_err());
}

#[tokio::test]
async fn sends_stored_api_key_and_preserves_response_stream() {
    let server = wiremock::MockServer::start().await;
    let path = tmp_credentials_path("raw-api");
    save_api_key(&path, &server.uri());
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v1/models"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer brk_AAAAAAAAAAAAAAAA.secret",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(
            "{\"data\":[]}",
            "application/json",
        ))
        .mount(&server)
        .await;

    let client = CloudApiClient::from_credentials_path(path).unwrap();
    let response = client
        .execute(ApiRequest::new(reqwest::Method::GET, "/v1/models"))
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.into_response().text().await.unwrap(), "{\"data\":[]}");
}
```

- [ ] **Step 2: Run raw-client tests and verify RED**

```bash
cargo test -p bitrouter-cloud-sdk api::tests --all-features
```

Expected: compilation fails because the module and client types do not exist.

- [ ] **Step 3: Implement the client and URL gate**

Create focused request/response types:

```rust
pub struct ApiRequest {
    method: reqwest::Method,
    endpoint: String,
    headers: reqwest::header::HeaderMap,
    body: Option<Vec<u8>>,
}

pub struct ApiResponse {
    response: reqwest::Response,
}

pub struct CloudApiClient {
    base_url: url::Url,
    http: reqwest::Client,
    store: std::sync::Arc<tokio::sync::Mutex<CredentialsStore>>,
    metadata: std::sync::Arc<tokio::sync::Mutex<Option<AsMetadata>>>,
}
```

`CloudApiClient::from_credentials_path` reads the stored origin and builds a reqwest client with `Policy::none()`. `resolve_endpoint` parses only a relative reference, strips the stored URL to its origin root, rejects fragments and scheme-relative/absolute input, and joins the path. `execute` refreshes OAuth or returns the static API key, supplies Authorization/User-Agent/Accept defaults, applies user headers/body, and returns without interpreting HTTP status or buffering the body.

- [ ] **Step 4: Test redirects and header overrides**

Add a two-server test where the first returns a 302 to the second; assert `execute` returns 302 and the second server receives zero requests. Add a same-origin test proving a user Authorization header overrides the stored default and that other repeated headers survive.

- [ ] **Step 5: Run the SDK suite and verify GREEN**

```bash
cargo test -p bitrouter-cloud-sdk --all-features
```

Expected: the raw-client tests and all auth/management/provider regressions pass.

- [ ] **Step 6: Commit the raw client**

```bash
git add crates/bitrouter-cloud-sdk
git commit -m "feat(cloud): add raw API client"
```

---

### Task 4: gh-api-style fields and streaming CLI output

**Files:**
- Create: `apps/bitrouter/src/cloud/api.rs`
- Modify: `apps/bitrouter/src/cloud/mod.rs`
- Modify: `apps/bitrouter/src/cloud/cli.rs`
- Modify: `apps/bitrouter/Cargo.toml`
- Test: `apps/bitrouter/src/cloud/api.rs`

**Interfaces:**
- Consumes: Task 3 `CloudApiClient` and raw response stream.
- Produces: `ApiArgs`, `run(ApiArgs)`, field-to-JSON/query conversion, and writer-injected output.

- [ ] **Step 1: Add failing clap and field grammar tests**

Declare the expected args in tests and cover typed/raw/nested behavior:

```rust
#[test]
fn typed_and_raw_fields_build_nested_json() {
    let fields = vec![
        InputField::typed("model=openai/gpt-5"),
        InputField::typed("stream=true"),
        InputField::raw("messages[][role]=user"),
        InputField::raw("messages[][content]=Hello"),
        InputField::typed("max_tokens=256"),
    ];
    assert_eq!(
        build_fields(&fields, &mut std::io::empty()).unwrap(),
        serde_json::json!({
            "model": "openai/gpt-5",
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}],
            "max_tokens": 256
        })
    );
}

#[test]
fn conflicting_nested_shapes_are_rejected() {
    let fields = vec![InputField::raw("a=value"), InputField::raw("a[b]=value")];
    assert!(build_fields(&fields, &mut std::io::empty()).is_err());
}

#[test]
fn fields_or_input_default_to_post() {
    assert_eq!(select_method(None, false, false), reqwest::Method::GET);
    assert_eq!(select_method(None, true, false), reqwest::Method::POST);
    assert_eq!(select_method(None, false, true), reqwest::Method::POST);
    assert_eq!(
        select_method(Some("PATCH"), false, false),
        reqwest::Method::PATCH
    );
}
```

Add a local `clap::Parser` harness asserting `api /v1/models`, short/long flags, repeated fields/headers, login conflicts, and `--silent` versus `--verbose` conflicts.

- [ ] **Step 2: Run app API tests and verify RED**

```bash
cargo test -p bitrouter cloud::api::tests --all-features
```

Expected: compilation fails because the API module and argument types do not exist.

- [ ] **Step 3: Implement arguments and field parser**

Create:

```rust
#[derive(Debug, clap::Args)]
pub struct ApiArgs {
    pub endpoint: String,
    #[arg(short = 'X', long, value_name = "METHOD")]
    pub method: Option<String>,
    #[arg(short = 'H', long = "header", value_name = "KEY:VALUE")]
    pub headers: Vec<String>,
    #[arg(short = 'f', long = "raw-field", value_name = "KEY=VALUE")]
    pub raw_fields: Vec<String>,
    #[arg(short = 'F', long = "field", value_name = "KEY=VALUE")]
    pub fields: Vec<String>,
    #[arg(long, value_name = "FILE")]
    pub input: Option<std::path::PathBuf>,
    #[arg(short = 'i', long)]
    pub include: bool,
    #[arg(long, conflicts_with = "verbose")]
    pub silent: bool,
    #[arg(long, conflicts_with = "silent")]
    pub verbose: bool,
}
```

Parse field keys into `KeyPart::Name(String)` and `KeyPart::Array`. Insert values into `serde_json::Value`, reusing the last object-array element until a repeated property starts a new element. Convert typed literals and `@file`/`@-`; track stdin ownership so only one consumer can claim it.

- [ ] **Step 4: Add failing request-assembly tests**

Test that fields become a JSON body for implicit POST, become query parameters for explicit GET, and become query parameters when `--input` owns the body. Test invalid methods/headers and exact input bytes.

- [ ] **Step 5: Implement request assembly and response writer**

Expose writer injection:

```rust
pub async fn run_with_io(
    args: ApiArgs,
    client: CloudApiClient,
    stdin: &mut dyn std::io::Read,
    stdout: &mut dyn std::io::Write,
    stderr: &mut dyn std::io::Write,
    stdout_is_terminal: bool,
) -> anyhow::Result<()>;
```

Build `ApiRequest`, execute it, optionally print HTTP version/status and sorted headers, then loop over `response.bytes_stream()` with `futures::StreamExt`. Buffer only interactive JSON so it can be pretty-printed; copy SSE and non-TTY output chunk by chunk. `--silent` drains without writing the body. `--verbose` prints method, confined URL, redacted request headers, response status, and redacted response headers to stderr. After emitting the body, return an error for status `>= 400`.

- [ ] **Step 6: Add output tests and verify RED/GREEN**

Use wiremock plus in-memory writers to cover JSON, split SSE events, include, silent, verbose redaction, and non-2xx body preservation. First run each new test before its implementation branch, confirm the expected assertion failure, then rerun:

```bash
cargo test -p bitrouter cloud::api::tests --all-features
```

Expected final result: all API module tests pass.

- [ ] **Step 7: Wire `CloudAction::Api` and verify command help**

Add `Api(ApiArgs)` to `CloudAction` and dispatch to `cloud::api::run`. Run:

```bash
cargo run -p bitrouter -- cloud api --help
cargo run -p bitrouter -- cloud login --help
```

Expected: help lists the designed flags and API-key login conflicts.

- [ ] **Step 8: Commit the CLI**

```bash
git add apps/bitrouter crates/bitrouter-cloud-sdk/Cargo.toml Cargo.lock
git commit -m "feat(cli): add cloud API command"
```

---

### Task 5: Initial endpoint integration matrix

**Files:**
- Create: `apps/bitrouter/tests/cloud_api.rs`
- Modify: `apps/bitrouter/Cargo.toml` only if a binary-test helper is required
- Test: `apps/bitrouter/tests/cloud_api.rs`

**Interfaces:**
- Consumes: completed public CLI and credential store.
- Produces: binary-level evidence for models and four generation protocol families under OAuth and API-key authentication.

- [ ] **Step 1: Add failing binary-level models test**

Use `CARGO_BIN_EXE_bitrouter`, a temporary `XDG_DATA_HOME`, and wiremock. Persist an API-key credential, run:

```rust
std::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"))
    .args(["cloud", "api", "/v1/models"])
    .env("XDG_DATA_HOME", data_home)
    .output()
```

Assert exit zero, exact JSON stdout, empty secret-free stderr, path `/v1/models`, and bearer header.

- [ ] **Step 2: Run the integration test and verify RED**

```bash
cargo test -p bitrouter --test cloud_api --all-features
```

Expected: the test fails before the binary-level harness and public command behavior are complete.

- [ ] **Step 3: Add the generation endpoint table**

Drive these cases through the actual binary with `--input`:

```rust
const CASES: &[(&str, &str)] = &[
    ("/v1/chat/completions", "chat"),
    ("/v1/messages", "messages"),
    ("/v1/responses", "responses"),
    (
        "/v1beta/models/google/gemini-2.5-flash:generateContent",
        "generate-content",
    ),
    (
        "/v1beta/models/google/gemini-2.5-flash:streamGenerateContent",
        "stream-generate-content",
    ),
];
```

Assert POST, exact input body, Authorization, success output, and SSE framing for the streaming case. Add OAuth fixture coverage using a fresh token plus mock metadata, and an HTTP error case proving body output plus non-zero exit.

- [ ] **Step 4: Run the endpoint matrix and verify GREEN**

```bash
cargo test -p bitrouter --test cloud_api --all-features
cargo test -p bitrouter-cloud-sdk --all-features
```

Expected: every endpoint/authentication case passes and no SDK regression appears.

- [ ] **Step 5: Commit endpoint coverage**

```bash
git add apps/bitrouter/tests/cloud_api.rs apps/bitrouter/Cargo.toml Cargo.lock
git commit -m "test(cli): cover cloud API protocols"
```

---

### Task 6: Product docs, Agent Skill, and PR validation

**Files:**
- Modify: `README.md`
- Modify: `CLI.md`
- Modify: `docs/concepts/cli.md`
- Modify: `docs/concepts/cli.zh.md`
- Create: `docs/guides/cloud-api.md`
- Create: `docs/guides/cloud-api.zh.md`
- Modify: `docs/guides/meta.json`
- Modify: `skills/bitrouter/references/cli.md`
- Modify: `skills/bitrouter/references/cloud-setup.md`

**Interfaces:**
- Consumes: the verified CLI behavior and examples.
- Produces: complete user-facing English/Chinese usage guidance and a PR ready for review.

- [ ] **Step 1: Document login and raw API reference**

Update `CLI.md`, both CLI concept pages, and the shipped Agent Skill with:

```bash
bitrouter cloud login --api-key "$BITROUTER_API_KEY"
bitrouter cloud api /v1/models
bitrouter cloud api /v1/chat/completions --input request.json
```

Document every implemented flag, method selection, field typing/nesting, stdin/file behavior, output modes, relative-path restriction, redirect protection, auth precedence, API-key logout semantics, and the first-release omissions.

- [ ] **Step 2: Add the bilingual Cloud API guide**

Create matching English and Simplified-Chinese pages with identical frontmatter keys, headings, code blocks, component tags, and link targets. Include copyable request files and commands for models, Chat Completions, Messages, Responses, `generateContent`, and `streamGenerateContent`. Add both pages as one `cloud-api` entry in `docs/guides/meta.json` according to the existing locale-neutral navigation format.

- [ ] **Step 3: Check documentation parity**

Run repository documentation checks discovered from `DEVELOPMENT.md`, then explicitly compare structure:

```bash
diff \
  <(rg '^(#|```|<|sourceHash:)' docs/guides/cloud-api.md | sed 's/^# .*/# TITLE/') \
  <(rg '^(#|```|<|sourceHash:)' docs/guides/cloud-api.zh.md | sed 's/^# .*/# TITLE/')
rg -n "cloud api|login --api-key" README.md CLI.md docs skills/bitrouter
```

Expected: structural diff is empty and every public surface has current references.

- [ ] **Step 4: Commit documentation**

```bash
git add README.md CLI.md docs skills/bitrouter
git commit -m "docs: explain cloud API workflows"
```

- [ ] **Step 5: Run fresh full verification**

Run exactly:

```bash
cargo fmt -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo nextest run --all-features || cargo test --all-features
git diff origin/main...HEAD --check
git status --short --branch
```

Expected: formatting and clippy exit zero; one complete test runner exits zero with no failures; diff check is empty; only intentional committed files differ from `origin/main`.

- [ ] **Step 6: Audit the specification requirement by requirement**

Read `design/2026-07-14-cloud-api-design.md` and map every acceptance criterion to a source file, automated test, documentation section, and fresh command result. Search the final diff for secrets, placeholder text, unhandled credential field access, and prohibited panic calls:

```bash
rg -n 'TB[D]|TO[D]O|FIX[M]E|brk_[A-Za-z0-9]{8,}\.[A-Za-z0-9]{8,}' \
  README.md CLI.md docs skills design apps crates || true
git diff origin/main...HEAD -- '*.rs' | rg '^\+.*(unwrap\(|expect\(|panic!\()' || true
```

Expected: no real-looking credential or placeholder remains; production additions contain no prohibited panic APIs; all ten acceptance criteria have direct evidence.

- [ ] **Step 7: Push and open the PR**

Confirm `gh auth status`, push with tracking, and create a draft PR targeting the repository default branch. Use a conventional title under 60 characters:

```text
feat(cli): add cloud API command
```

The PR body must summarize API-key login, raw/streaming API behavior, compatibility/security properties, documentation, and the exact verification commands.
