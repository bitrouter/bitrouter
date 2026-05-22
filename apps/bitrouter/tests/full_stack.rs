//! Full-stack integration tests — every plugin + every binary-private
//! business module exercised by a single HTTP request through the
//! assembled `App`.
//!
//! What's wired by `assemble_full_stack` (mirroring production
//! `apps/bitrouter/src/assemble.rs`):
//!
//! - `crate::auth::AuthHook` (binary module: `brvk_` validation)
//! - `crate::policy::PolicyHook` (binary module: model + tool + spend)
//! - `bitrouter_guardrails::*` (shared plugin: pre-request block +
//!   stream-stage redact)
//! - `bitrouter_observe::OtelObserveHook` (shared plugin: per-request OTLP
//!   trace + metric export)
//! - `crate::metering::MeteringRecorder` (binary module: SettlementRecorder)
//!
//! These tests are the canonical "did anyone break the assembly?" gate —
//! a hook silently dropped from `assemble.rs`, an `Arc` not shared
//! between writer and renderer, etc., shows up here even when every unit
//! test still passes.

use std::sync::Arc;
use std::time::Duration;

use axum_test::TestServer;
use bitrouter_sdk::config;
use bitrouter_sdk::server::{AppState, build_router};
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use bitrouter::auth::{NewApiKey, db as auth_db, generate};
use bitrouter::daemon::ObserveStatusProvider;
use bitrouter::metering::entities::requests;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter};

// ===== shared setup =====

/// Everything one full-stack test needs after assembly.
struct FullStack {
    server: TestServer,
    db: DatabaseConnection,
    brvk_secret: String,
    upstream: MockServer,
    otlp_collector: MockServer,
    /// Same provider production assembles. Held here so each test can
    /// call `teardown()` and drive the OTel SDK's flush via
    /// `spawn_blocking` before this struct drops — otherwise the
    /// SDK's `Drop` parks the tokio worker on its `rt-tokio`
    /// background-task channel and deadlocks on a `current_thread`
    /// test runtime.
    observe: Arc<dyn ObserveStatusProvider>,
    _policy_dir: PolicyDir,
}

impl FullStack {
    /// Flush OTel state and drop the harness. Each test MUST call this
    /// before returning; the test runtime is `current_thread`, so the
    /// SDK's implicit `Drop` shutdown deadlocks if this is skipped.
    async fn teardown(self) {
        self.observe.shutdown().await;
        // Remaining fields drop synchronously; their Drop impls do not
        // touch the OTel SDK.
    }
}

/// Owns the temp policy directory and removes it on drop so a test failure
/// doesn't litter `/tmp` with old runs.
struct PolicyDir(std::path::PathBuf);
impl Drop for PolicyDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build the full assembled stack: every plugin + binary module wired into
/// the router, a brvk_ key minted, the upstream + OTLP collector stood up.
async fn assemble_full_stack() -> FullStack {
    // ── policy: one `pol_main` policy bound to the key we'll mint ──
    let policy_dir = std::env::temp_dir().join(format!(
        "bitrouter-fullstack-pol-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    tokio::fs::create_dir_all(&policy_dir).await.unwrap();
    tokio::fs::write(
        policy_dir.join("pol_main.yaml"),
        // High enough that none of the spend asserts trip; spend-cap
        // enforcement has its own e2e (`e2e_metering_drives_policy_spend_cap`).
        // `allowed_tools` deliberately omits `filesystem` so Test 3 can
        // catch the deny.
        "id: pol_main\n\
         allowed_models: [test-model]\n\
         allowed_tools: [search]\n\
         max_spend_micro_usd: 100000000\n",
    )
    .await
    .unwrap();

    // ── upstream: SSE stream that carries one SSN-shaped span we expect
    //    the guardrail stream-hook to redact, plus a non-streaming
    //    branch with the same content for the block-test path ──
    let upstream = MockServer::start().await;
    let sse_body = build_sse_stream_with_ssn();
    Mock::given(method("POST"))
        .and(wm_path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    // ── OTLP collector — accepts every `POST /v1/traces` for assertion ──
    let otlp_collector = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/v1/traces"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&otlp_collector)
        .await;

    // ── config wiring every plugin + module ──
    let yaml = format!(
        r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: false
database:
  url: "sqlite::memory:"
providers:
  mock:
    api_base: {upstream}
    api_key: test-key
    api_protocol:
      - "*": openai
    models:
      - id: test-model
        pricing:
          input_micro_usd_per_token: 2.0
          output_micro_usd_per_token: 10.0
plugins:
  bitrouter-policy:
    policy_dir: "{policy_path}"
  bitrouter-guardrails:
    custom_patterns:
      - {{ name: ssn,       pattern: '\d{{3}}-\d{{2}}-\d{{4}}', action: redact }}
      - {{ name: forbidden, pattern: '(?i)forbidden',           action: block  }}
  bitrouter-observe:
    otlp_endpoint: "{otlp}"
"#,
        upstream = upstream.uri(),
        policy_path = policy_dir.display(),
        otlp = otlp_collector.uri(),
    );
    let cfg: config::Config = config::parse_with(&yaml, |_| None).expect("config parses");
    let assembled = bitrouter::build_app(&cfg).await.expect("app assembles");
    let observe = assembled.observe.clone();

    // ── mint a brvk_ key bound to pol_main ──
    let user = "fullstack-user";
    auth_db::upsert_user(&assembled.db, user).await.unwrap();
    let key = generate();
    let key_id = "fullstack-key".to_string();
    auth_db::insert_api_key(
        &assembled.db,
        &NewApiKey {
            id: key_id,
            key_hash: key.hash.clone(),
            user_id: user.to_string(),
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: Some("pol_main".to_string()),
        },
    )
    .await
    .unwrap();

    // ── router + axum_test server ──
    let state = AppState {
        language_model: assembled.app.language_model().unwrap().clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
    };
    let router = build_router(state);
    let server = TestServer::new(router);

    FullStack {
        server,
        db: assembled.db,
        brvk_secret: key.secret,
        upstream,
        otlp_collector,
        observe,
        _policy_dir: PolicyDir(policy_dir),
    }
}

/// OpenAI-style SSE: role; content with an SSN-shaped span; finish
/// chunk carrying `usage` (so MeteringRecorder has something to price);
/// then `[DONE]`.
fn build_sse_stream_with_ssn() -> String {
    let mut out = String::new();
    let chunk = |delta: serde_json::Value, finish: Option<&str>| -> String {
        let body = serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "model": "test-model",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        format!("data: {}\n\n", body)
    };
    out.push_str(&chunk(
        serde_json::json!({"role": "assistant", "content": "your "}),
        None,
    ));
    out.push_str(&chunk(
        serde_json::json!({"content": "ssn is 123-45-6789 ok"}),
        None,
    ));
    // The finish chunk also carries usage so the metering recorder has
    // pipeline-observed token counts to price.
    let finish = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "model": "test-model",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18},
    });
    out.push_str(&format!("data: {}\n\n", finish));
    out.push_str("data: [DONE]\n\n");
    out
}

/// A clean OpenAI Chat Completions body the auth/policy/guardrails path
/// should let through.
fn clean_body(stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{"function": {"name": "search"}}],
        "stream": stream,
    })
}

// ===== Test 1 — full happy path =====

#[tokio::test]
async fn e2e_full_stack_streaming_redacts_and_meters() {
    let fs = assemble_full_stack().await;

    // The HTTP call goes auth → policy → guardrails → executor → stream
    // hook → observe → metering, in that registered order.
    let resp = fs
        .server
        .post("/v1/chat/completions")
        .add_header("authorization", format!("Bearer {}", fs.brvk_secret))
        .add_header("accept", "text/event-stream")
        .json(&clean_body(true))
        .await;
    resp.assert_status_ok();
    assert_eq!(
        resp.header("content-type").to_str().unwrap(),
        "text/event-stream",
        "streaming response must advertise text/event-stream",
    );
    let body = resp.text();
    assert!(
        body.contains("[REDACTED]"),
        "GuardrailStreamHook should have inserted [REDACTED] into the SSE body; got:\n{body}"
    );
    assert!(
        !body.contains("123-45-6789"),
        "GuardrailStreamHook must have stripped the SSN-shaped span from the SSE body; got:\n{body}"
    );

    // The streaming pipeline finalises settlement asynchronously after
    // the last byte is sent; give the detached task a moment to land.
    wait_for_metering_row(&fs.db, "fullstack-key").await;

    // ── MeteringRecorder ran: one row with streamed=1, identity intact ──
    let row = requests::Entity::find()
        .filter(requests::Column::ApiKeyId.eq("fullstack-key"))
        .one(&fs.db)
        .await
        .expect("metering query runs")
        .expect("metering wrote a row for the streamed request");
    assert_eq!(
        row.model_id, "test-model",
        "metering row records the routed model"
    );
    assert_eq!(
        row.streamed, 1,
        "metering row marks the request as streamed"
    );
    // pricing 2µ$/prompt + 10µ$/completion × usage 11 + 7 = 92µ$.
    // Exact match so a regression that quietly halves the rate can't
    // sneak past with a > 0 check.
    assert_eq!(
        row.estimated_charge_micro_usd, 92,
        "estimated_charge_micro_usd should be 2*11 + 10*7 = 92µ$",
    );

    // ── OtlpExportHook ran at least once. The exporter batches with a
    //    short interval; wait briefly for the collector to see it. ──
    wait_for_otlp(&fs.otlp_collector).await;

    fs.teardown().await;
}

// ===== Test 2 — guardrail block at pre-request =====

#[tokio::test]
async fn e2e_full_stack_guardrail_blocks_at_pre_request() {
    let fs = assemble_full_stack().await;

    // The `forbidden` pattern is a Block rule — request denied before
    // any upstream call, no metering row.
    let resp = fs
        .server
        .post("/v1/chat/completions")
        .add_header("authorization", format!("Bearer {}", fs.brvk_secret))
        .expect_failure()
        .json(&serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "please do the forbidden thing"}],
            "tools": [{"function": {"name": "search"}}],
        }))
        .await;
    resp.assert_status_bad_request();

    // No upstream call.
    let upstream_requests = fs.upstream.received_requests().await.unwrap_or_default();
    assert!(
        upstream_requests.is_empty(),
        "blocked request must not reach the upstream; saw {} request(s)",
        upstream_requests.len(),
    );

    // No metering row.
    let row_count = requests::Entity::find()
        .filter(requests::Column::ApiKeyId.eq("fullstack-key"))
        .count(&fs.db)
        .await
        .expect("count query runs");
    assert_eq!(
        row_count, 0,
        "blocked request must not produce a metering row",
    );

    fs.teardown().await;
}

// ===== Test 3 — policy tool restriction (ordering proof) =====

#[tokio::test]
async fn e2e_full_stack_policy_denies_disallowed_tool_before_guardrails() {
    let fs = assemble_full_stack().await;

    // Allowed model but disallowed tool. The policy `allowed_tools` is
    // `[search]`; declaring `filesystem` violates it. PolicyHook runs
    // BEFORE GuardrailPreHook (per `assemble.rs` registration order),
    // so the failure reason should be tool-policy, not guardrails.
    let resp = fs
        .server
        .post("/v1/chat/completions")
        .add_header("authorization", format!("Bearer {}", fs.brvk_secret))
        .expect_failure()
        .json(&serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "clean prompt"}],
            "tools": [{"function": {"name": "filesystem"}}],
        }))
        .await;
    resp.assert_status_forbidden();
    let err: serde_json::Value = resp.json();
    let msg = err["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("tool") && msg.contains("filesystem"),
        "deny reason must come from PolicyHook (tool restriction), not guardrails; got: {msg}",
    );

    // No upstream call, no metering row.
    let upstream_requests = fs.upstream.received_requests().await.unwrap_or_default();
    assert!(upstream_requests.is_empty());
    let row_count = requests::Entity::find()
        .filter(requests::Column::ApiKeyId.eq("fullstack-key"))
        .count(&fs.db)
        .await
        .expect("count query runs");
    assert_eq!(row_count, 0);

    fs.teardown().await;
}

// ===== helpers =====

/// Polling budget for the two async-side-effect waits. 5s is generous
/// enough that loaded CI (cold-start of two MockServers + sqlite
/// migrations + tokio task stalls) won't flake, but a real wiring
/// failure still surfaces fast.
const ASYNC_WAIT_BUDGET_MS: u64 = 5_000;
const POLL_INTERVAL_MS: u64 = 50;

/// Streaming settlement is detached after the response finishes — poll
/// for the metering row to appear so the test doesn't race the pipeline's
/// background task.
async fn wait_for_metering_row(db: &DatabaseConnection, api_key_id: &str) {
    for _ in 0..(ASYNC_WAIT_BUDGET_MS / POLL_INTERVAL_MS) {
        let count = requests::Entity::find()
            .filter(requests::Column::ApiKeyId.eq(api_key_id))
            .count(db)
            .await
            .expect("count query runs");
        if count >= 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
    panic!(
        "metering row never appeared within {}ms of stream completion",
        ASYNC_WAIT_BUDGET_MS
    );
}

/// Poll the OTLP collector until it has seen at least one `/v1/traces` POST.
async fn wait_for_otlp(collector: &MockServer) {
    for _ in 0..(ASYNC_WAIT_BUDGET_MS / POLL_INTERVAL_MS) {
        let n = collector
            .received_requests()
            .await
            .map(|r| r.len())
            .unwrap_or(0);
        if n >= 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
    panic!(
        "OTLP collector never received an export within {}ms",
        ASYNC_WAIT_BUDGET_MS
    );
}
