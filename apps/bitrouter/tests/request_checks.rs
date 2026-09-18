//! Request checks exercised through the shipped host and HTTP gateway.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use axum_test::TestServer;
use bitrouter_sdk::config;
use bitrouter_sdk::language_model::receipts::{
    RequestCheckStatus, RequestReceipt, RequestReceiptLookup, RequestReceiptOutcome,
};
use bitrouter_sdk::server::{AppState, build_router};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[derive(Clone)]
struct Verdict(&'static str);

impl Respond for Verdict {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
            return ResponseTemplate::new(400);
        };
        ResponseTemplate::new(200).set_body_json(json!({
            "contract_version": 1,
            "invocation_id": body["invocation_id"],
            "decision": self.0,
            "implementation_version": "fixture-v1"
        }))
    }
}

fn gateway(assembled: &bitrouter::Assembled) -> Result<TestServer> {
    Ok(TestServer::new(build_router(AppState {
        language_model: assembled
            .app
            .language_model()
            .context("no pipeline")?
            .clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    })))
}

fn configuration(upstream: &str, checker: &str, timeout: u64) -> Result<config::Config> {
    config::parse(&format!(
        r#"inherit_defaults: false
server:
  skip_auth: true
database:
  url: 'sqlite::memory:'
providers:
  fixture:
    api_base: {upstream}
    api_key: provider-secret-must-not-be-projected
    models:
      - id: model
checkers:
  first:
    endpoint: {checker}/allow
    contract_version: 1
  second:
    endpoint: {checker}/deny
    contract_version: 1
routers:
  coding:
    selection:
      kind: model
      model: fixture:model
    defaults:
      system_prompt: router-default-must-be-checked
    checks:
      request:
        - checker: first
          timeout_ms: {timeout}
  restricted:
    selection:
      kind: model
      model: fixture:model
    checks:
      request:
        - checker: second
          timeout_ms: {timeout}
"#
    ))
    .map_err(Into::into)
}

fn receipt(assembled: &bitrouter::Assembled, request_id: &str) -> Result<RequestReceipt> {
    match assembled.request_checks.receipts().get(request_id, None) {
        RequestReceiptLookup::Found { receipt, .. } => Ok(receipt),
        result => anyhow::bail!("request {request_id} has no receipt: {result:?}"),
    }
}

async fn mount_upstream(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "fixture-response",
            "object": "chat.completion",
            "model": "model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
        })))
        .mount(upstream)
        .await;
}

fn request(router: &str) -> Value {
    json!({"model": format!("bitrouter/{router}"), "messages": [{"role": "user", "content": "same input"}]})
}

#[tokio::test]
async fn routers_apply_distinct_checks_to_effective_text_before_model_dispatch() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = MockServer::start().await;
    mount_upstream(&upstream).await;
    Mock::given(path("/allow"))
        .respond_with(Verdict("allow"))
        .mount(&checker)
        .await;
    Mock::given(path("/deny"))
        .respond_with(Verdict("deny"))
        .mount(&checker)
        .await;
    let config = configuration(&upstream.uri(), &checker.uri(), 5_000)?;
    let assembled = bitrouter::build_app(&config).await?;
    let server = gateway(&assembled)?;

    let allowed = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "check-allowed")
        .json(&request("coding"))
        .await;
    ensure!(
        allowed.status_code().is_success(),
        "allow failed: {}",
        allowed.text()
    );
    ensure!(allowed.header("x-bitrouter-request-id") == "check-allowed");
    let denied = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "check-denied")
        .json(&request("restricted"))
        .await;
    ensure!(
        !denied.status_code().is_success(),
        "deny unexpectedly succeeded"
    );
    ensure!(denied.header("x-bitrouter-request-id") == "check-denied");

    let calls = upstream
        .received_requests()
        .await
        .context("upstream capture unavailable")?;
    ensure!(calls.len() == 1, "denied request reached upstream");
    let checked = checker
        .received_requests()
        .await
        .context("checker capture unavailable")?;
    ensure!(checked.len() == 2);
    let allowed_check = checked
        .iter()
        .find(|call| call.url.path() == "/allow")
        .context("no allow check")?;
    let body = String::from_utf8(allowed_check.body.clone())?;
    ensure!(body.contains("router-default-must-be-checked"));
    ensure!(body.contains("same input"));
    ensure!(!body.contains("provider-secret-must-not-be-projected"));
    ensure!(!allowed_check.headers.contains_key("authorization"));
    let allowed_receipt = receipt(&assembled, "check-allowed")?;
    let denied_receipt = receipt(&assembled, "check-denied")?;
    ensure!(allowed_receipt.identity.router_id == "coding");
    ensure!(denied_receipt.identity.router_id == "restricted");
    ensure!(allowed_receipt.upstream_started);
    ensure!(!denied_receipt.upstream_started);
    ensure!(allowed_receipt.outcome == Some(RequestReceiptOutcome::Completed));
    ensure!(denied_receipt.outcome == Some(RequestReceiptOutcome::Denied));
    ensure!(
        allowed_receipt
            .checks
            .first()
            .is_some_and(|check| check.status == RequestCheckStatus::Allowed)
    );
    ensure!(
        denied_receipt
            .checks
            .first()
            .is_some_and(|check| check.status == RequestCheckStatus::Denied)
    );
    let serialized = serde_json::to_string(&assembled.request_checks.receipts().list(20))?;
    ensure!(!serialized.contains("router-default-must-be-checked"));
    ensure!(!serialized.contains("same input"));
    ensure!(!serialized.contains("provider-secret-must-not-be-projected"));
    Ok(())
}

#[tokio::test]
async fn timeout_and_protocol_failure_never_dispatch_a_model() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = MockServer::start().await;
    Mock::given(path("/allow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(500))
                .set_body_json(json!({})),
        )
        .mount(&checker)
        .await;
    Mock::given(path("/deny"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"decision": "allow"})))
        .mount(&checker)
        .await;
    let assembled =
        bitrouter::build_app(&configuration(&upstream.uri(), &checker.uri(), 40)?).await?;
    let server = gateway(&assembled)?;
    for (router, prefix) in [
        ("coding", "check-timeout"),
        ("restricted", "check-malformed"),
    ] {
        for stream in [false, true] {
            let id = format!("{prefix}-{stream}");
            let mut body = request(router);
            body["stream"] = Value::Bool(stream);
            let response = server
                .post("/v1/chat/completions")
                .add_header("x-bitrouter-request-id", id.clone())
                .json(&body)
                .await;
            ensure!(!response.status_code().is_success());
            ensure!(response.header("x-bitrouter-request-id") == id.as_str());
            let retained = receipt(&assembled, &id)?;
            ensure!(!retained.upstream_started);
            ensure!(retained.outcome == Some(RequestReceiptOutcome::Failed));
            ensure!(
                retained
                    .checks
                    .first()
                    .is_some_and(|check| check.status == RequestCheckStatus::Failed)
            );
        }
    }
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture unavailable")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn checker_denial_stops_later_checker_and_model_dispatch() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = MockServer::start().await;
    Mock::given(path("/allow"))
        .respond_with(Verdict("allow"))
        .mount(&checker)
        .await;
    Mock::given(path("/deny"))
        .respond_with(Verdict("deny"))
        .mount(&checker)
        .await;
    let mut config = configuration(&upstream.uri(), &checker.uri(), 5_000)?;
    let restricted = config
        .routers
        .get_mut("restricted")
        .context("missing restricted router")?;
    restricted
        .checks
        .request
        .push(config::router::RouterRequestCheck {
            checker: "first".to_owned(),
            timeout_ms: 5_000,
            max_input_bytes: config::router::DEFAULT_CHECKER_MAX_INPUT_BYTES,
        });
    let assembled = bitrouter::build_app(&config).await?;
    let server = gateway(&assembled)?;
    let response = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "checker-rejected")
        .json(&request("restricted"))
        .await;
    ensure!(!response.status_code().is_success());
    let checks = checker
        .received_requests()
        .await
        .context("checker capture unavailable")?;
    ensure!(checks.len() == 1, "later checker ran after denial");
    ensure!(
        checks
            .first()
            .is_some_and(|request| request.url.path() == "/deny")
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture unavailable")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn required_missing_checker_credential_blocks_host_activation() -> Result<()> {
    let mut config = configuration("http://127.0.0.1:1", "http://127.0.0.1:2", 500)?;
    config
        .checkers
        .get_mut("first")
        .context("missing checker")?
        .credential_env = Some(format!(
        "BITROUTER_TEST_MISSING_CHECKER_KEY_{}",
        uuid::Uuid::new_v4().simple()
    ));
    ensure!(bitrouter::build_app(&config).await.is_err());
    Ok(())
}

#[tokio::test]
async fn probe_has_no_request_receipt_and_allow_does_not_mask_upstream_failure() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = MockServer::start().await;
    Mock::given(path("/allow"))
        .respond_with(Verdict("allow"))
        .mount(&checker)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstream)
        .await;
    let assembled =
        bitrouter::build_app(&configuration(&upstream.uri(), &checker.uri(), 5_000)?).await?;
    let before = assembled.request_checks.receipts().list(20);
    ensure!(before.receipts.is_empty());
    let _probe = assembled.request_checks.probe("first").await;
    ensure!(
        assembled
            .request_checks
            .receipts()
            .list(20)
            .receipts
            .is_empty()
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture unavailable")?
            .is_empty()
    );
    ensure!(
        checker
            .received_requests()
            .await
            .context("checker capture unavailable")?
            .len()
            == 1
    );
    let server = gateway(&assembled)?;
    let response = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "upstream-failed-after-allow")
        .json(&request("coding"))
        .await;
    ensure!(!response.status_code().is_success());
    let retained = receipt(&assembled, "upstream-failed-after-allow")?;
    ensure!(retained.upstream_started);
    ensure!(retained.outcome == Some(RequestReceiptOutcome::Failed));
    ensure!(
        retained
            .checks
            .first()
            .is_some_and(|check| check.status == RequestCheckStatus::Allowed)
    );
    Ok(())
}

#[tokio::test]
async fn oversize_text_is_rejected_without_checker_or_model_dispatch() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = MockServer::start().await;
    let mut config = configuration(&upstream.uri(), &checker.uri(), 5_000)?;
    let router = config
        .routers
        .get_mut("coding")
        .context("missing coding router")?;
    router.checks.request[0].max_input_bytes = 1024;
    let assembled = bitrouter::build_app(&config).await?;
    let server = gateway(&assembled)?;
    let mut body = request("coding");
    body["messages"][0]["content"] = Value::String("x".repeat(2048));
    let response = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "input-too-large")
        .json(&body)
        .await;
    ensure!(!response.status_code().is_success());
    ensure!(
        checker
            .received_requests()
            .await
            .context("checker capture unavailable")?
            .is_empty()
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture unavailable")?
            .is_empty()
    );
    let retained = receipt(&assembled, "input-too-large")?;
    ensure!(!retained.upstream_started);
    ensure!(retained.outcome == Some(RequestReceiptOutcome::Failed));
    Ok(())
}

#[tokio::test]
async fn unauthenticated_requests_do_not_invoke_a_checker_or_claim_router_admission() -> Result<()>
{
    let upstream = MockServer::start().await;
    let checker = MockServer::start().await;
    let mut config = configuration(&upstream.uri(), &checker.uri(), 5_000)?;
    config.server.skip_auth = false;
    let assembled = bitrouter::build_app(&config).await?;
    let server = gateway(&assembled)?;
    let response = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "unauthenticated")
        .json(&request("coding"))
        .await;
    ensure!(!response.status_code().is_success());
    ensure!(
        checker
            .received_requests()
            .await
            .context("checker capture unavailable")?
            .is_empty()
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture unavailable")?
            .is_empty()
    );
    ensure!(
        assembled
            .request_checks
            .receipts()
            .list(20)
            .receipts
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn streaming_success_retains_check_and_server_delivery_evidence() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = MockServer::start().await;
    Mock::given(path("/allow"))
        .respond_with(Verdict("allow"))
        .mount(&checker)
        .await;
    let first = json!({"id":"stream-1","object":"chat.completion.chunk","model":"model","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]});
    let last = json!({"id":"stream-1","object":"chat.completion.chunk","model":"model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}});
    Mock::given(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        ))
        .mount(&upstream)
        .await;
    let assembled =
        bitrouter::build_app(&configuration(&upstream.uri(), &checker.uri(), 5_000)?).await?;
    let server = gateway(&assembled)?;
    let mut body = request("coding");
    body["stream"] = Value::Bool(true);
    let response = tokio::time::timeout(Duration::from_secs(10), async {
        server
            .post("/v1/chat/completions")
            .add_header("x-bitrouter-request-id", "stream-completed")
            .json(&body)
            .await
    })
    .await?;
    ensure!(response.status_code().is_success(), "{}", response.text());
    ensure!(response.text().contains("[DONE]"));
    assembled
        .app
        .language_model()
        .context("no pipeline")?
        .drain_required_pending_settlements()
        .await?;
    let retained = receipt(&assembled, "stream-completed")?;
    ensure!(retained.upstream_started);
    ensure!(
        retained.outcome == Some(RequestReceiptOutcome::Completed),
        "{retained:?}"
    );
    ensure!(
        retained
            .checks
            .first()
            .is_some_and(|check| check.status == RequestCheckStatus::Allowed)
    );
    Ok(())
}

#[tokio::test]
async fn transport_retries_keep_separate_receipts_under_one_request_id() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = MockServer::start().await;
    mount_upstream(&upstream).await;
    Mock::given(path("/allow"))
        .respond_with(Verdict("allow"))
        .mount(&checker)
        .await;
    let assembled =
        bitrouter::build_app(&configuration(&upstream.uri(), &checker.uri(), 5_000)?).await?;
    let server = gateway(&assembled)?;
    for _ in 0..2 {
        let response = server
            .post("/v1/chat/completions")
            .add_header("x-bitrouter-request-id", "transport-retry")
            .json(&request("coding"))
            .await;
        ensure!(response.status_code().is_success(), "{}", response.text());
    }
    let lookup = serde_json::to_value(
        assembled
            .request_checks
            .receipts()
            .get("transport-retry", None),
    )?;
    ensure!(lookup["status"] == "found");
    ensure!(lookup["retained_matches"] == 2, "{lookup}");
    let listed = assembled.request_checks.receipts().list(20);
    ensure!(listed.receipts.len() == 2);
    let first = serde_json::to_value(&listed.receipts[0].identity)?;
    let second = serde_json::to_value(&listed.receipts[1].identity)?;
    ensure!(first["receipt_id"].is_string());
    ensure!(first["receipt_id"] != second["receipt_id"]);
    ensure!(first["request_id"] == second["request_id"]);
    Ok(())
}
