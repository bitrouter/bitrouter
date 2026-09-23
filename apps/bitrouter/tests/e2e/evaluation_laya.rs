//! Local Laya format through the assembled evaluation endpoint. The synthetic
//! upstream fixture is derived from a real run of the pinned checkpoint.

use anyhow::Context;
use axum::Router;
use axum_test::TestServer;
use bitrouter::assemble::Assembled;
use bitrouter_laya_extension::{MAX_QUESTIONS, PROVIDER_MODEL_ID};
use bitrouter_sdk::config;
use bitrouter_sdk::extension::ExtensionApi;
use bitrouter_sdk::server::{AppState, RouterOptions, build_router_with_options};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "laya/typed-decisions-f9ab0b2";
const TOKEN: &str = "local-fixture-token-1234";
const MIXED_REQUEST: &str =
    include_str!("../../../../crates/bitrouter-laya-extension/tests/fixtures/mixed_request.json");
const MIXED_RESPONSE: &str =
    include_str!("../../../../crates/bitrouter-laya-extension/tests/fixtures/mixed_response.json");

fn fixture_config(base: &str, other: &str, skip_auth: bool) -> anyhow::Result<config::Config> {
    Ok(config::parse(&format!(
        r#"
inherit_defaults: false
server:
  skip_auth: {skip_auth}
database:
  url: "sqlite::memory:"
providers:
  laya:
    api_base: {base}
    api_key: {TOKEN}
    operations:
      evaluate:
        endpoint: /v1/decisions
        format: {{ extension: laya, adapter: local_system_one, revision: 1 }}
    models:
      - id: {MODEL}
        provider_model_id: {PROVIDER_MODEL_ID}
        operations:
          evaluate:
            question_types: [noul, choice, score]
            max_choice_options: 20
            max_score_levels: 10
  other:
    api_base: {other}
    api_key: other-fixture-token-1234
    operations:
      evaluate:
        endpoint: /v1/decisions
        format: {{ extension: laya, adapter: local_system_one, revision: 1 }}
    models:
      - id: {MODEL}
        provider_model_id: {PROVIDER_MODEL_ID}
        operations:
          evaluate:
            question_types: [noul, choice, score]
"#
    ))?)
}

async fn server(config: &config::Config) -> anyhow::Result<TestServer> {
    let mut extensions = ExtensionApi::new();
    bitrouter_laya_extension::register(&mut extensions)?;
    let assembled =
        bitrouter::assemble::build_app_with_extensions(config, None, &extensions, None).await?;
    Ok(TestServer::new(router_for_assembled(config, &assembled)?))
}

fn router_for_assembled(config: &config::Config, assembled: &Assembled) -> anyhow::Result<Router> {
    let state = AppState {
        language_model: assembled
            .app
            .language_model()
            .context("missing language-model pipeline")?
            .clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    };
    Ok(build_router_with_options(
        state,
        RouterOptions {
            omit_v1_models: true,
            ..RouterOptions::default()
        },
    )
    .merge(bitrouter::evaluation_http::router(config, assembled)))
}

fn request() -> anyhow::Result<Value> {
    Ok(serde_json::from_str(MIXED_REQUEST)?)
}

fn response() -> anyhow::Result<Value> {
    Ok(serde_json::from_str(MIXED_RESPONSE)?)
}

#[tokio::test]
async fn laya_route_needs_explicit_native_registration() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    let other = MockServer::start().await;
    let config = fixture_config(&upstream.uri(), &other.uri(), true)?;
    assert!(
        bitrouter::assemble::build_app_with_extensions(&config, None, &ExtensionApi::new(), None)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn local_laya_preserves_three_types_and_pinned_mapping_without_cost() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .and(header("authorization", format!("Bearer {TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(response()?))
        .mount(&upstream)
        .await;
    let config = fixture_config(&upstream.uri(), &other.uri(), true)?;
    let server = server(&config).await?;
    let listing: Value = server.get("/v1/models").await.json();
    assert!(
        listing["data"]
            .as_array()
            .is_some_and(|models| models.iter().any(|entry| {
                entry["id"] == format!("laya:{MODEL}") && entry["operations"] == json!(["evaluate"])
            }))
    );
    let mut input = request()?;
    input["model"] = json!(format!("laya:{MODEL}"));
    input["future_field"] = json!({"ignored": true});
    let result = server.post("/v1/evaluate").json(&input).await;
    result.assert_status_ok();
    let body: Value = result.json();
    assert_eq!(body["provider"], "laya");
    assert_eq!(body["model"], PROVIDER_MODEL_ID);
    assert_eq!(body["answers"]["billing"]["type"], "noul");
    assert_eq!(body["answers"]["team"]["type"], "choice");
    assert_eq!(body["answers"]["urgency"]["type"], "score");
    assert!(body["answers"]["team"].get("action").is_none());
    assert!(body["answers"]["billing"].get("confidence").is_none());
    assert_eq!(body["usage"]["input_tokens"], 92);
    assert_eq!(body["usage"]["output_tokens"], 0);
    assert!(body["usage"].get("cost").is_none() || body["usage"]["cost"].is_null());
    let calls = upstream
        .received_requests()
        .await
        .context("local mock request capture missing")?;
    assert_eq!(calls.len(), 1);
    let sent: Value = serde_json::from_slice(&calls[0].body)?;
    assert_eq!(sent["model"], PROVIDER_MODEL_ID);
    assert!(sent.get("future_field").is_none());
    assert!(
        other
            .received_requests()
            .await
            .context("other mock request capture missing")?
            .is_empty()
    );
    assert_eq!(server.post("/v1/systemone").await.status_code(), 404);
    Ok(())
}

#[tokio::test]
async fn local_laya_auth_and_limits_reject_before_transport() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    let other = MockServer::start().await;
    let protected = server(&fixture_config(&upstream.uri(), &other.uri(), false)?).await?;
    assert_eq!(
        protected
            .post("/v1/evaluate")
            .json(&request()?)
            .await
            .status_code(),
        401
    );
    let open = server(&fixture_config(&upstream.uri(), &other.uri(), true)?).await?;
    let mut excessive = request()?;
    for index in 0..MAX_QUESTIONS {
        excessive["questions"][format!("extra-{index}")] =
            json!({"type": "noul", "instructions": "check"});
    }
    let result = open.post("/v1/evaluate").json(&excessive).await;
    assert_eq!(result.status_code(), 400);
    assert_eq!(
        result.json::<Value>()["error"]["code"],
        "invalid_evaluation_request"
    );
    assert!(
        upstream
            .received_requests()
            .await
            .context("local mock request capture missing")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn pinned_local_laya_does_not_fall_back_to_another_provider() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(502).set_body_string("private local failure"))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response()?))
        .mount(&other)
        .await;
    let open = server(&fixture_config(&upstream.uri(), &other.uri(), true)?).await?;
    let mut input = request()?;
    input["model"] = json!(format!("laya:{MODEL}"));
    let result = open.post("/v1/evaluate").json(&input).await;
    assert_eq!(result.status_code(), 503);
    let body: Value = result.json();
    assert_eq!(body["error"]["code"], "upstream_unavailable");
    assert!(!body.to_string().contains("private local failure"));
    assert!(
        other
            .received_requests()
            .await
            .context("other mock request capture missing")?
            .is_empty()
    );
    Ok(())
}

/// Opt-in, offline real-checkpoint smoke. Download the exact pinned snapshot
/// once, then set LAYA_PYTHON to a Python with laya==0.3.6 and HF_HOME to the
/// snapshot cache. CI runs only the fake-process and mock-upstream tests.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires a locally cached pinned Laya checkpoint"]
async fn real_laya_process_through_evaluate() -> anyhow::Result<()> {
    use std::path::Path;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::process::Command;

    let python = std::env::var("LAYA_PYTHON").context("set LAYA_PYTHON to the Laya venv Python")?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/bitrouter-laya-extension/local_provider/server.py");
    let mut child = Command::new(python)
        .arg("-B")
        .arg(script)
        .arg("--port")
        .arg(port.to_string())
        .env("LAYA_LOCAL_TOKEN", TOKEN)
        .env("LAYA_OFFLINE", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("real Laya provider exited before readiness: {status}");
        }
        if let Ok(response) = client.get(format!("{base}/health")).send().await
            && response.status().is_success()
        {
            let health: Value = response.json().await?;
            anyhow::ensure!(health["model"] == PROVIDER_MODEL_ID, "wrong snapshot ready");
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "Laya readiness timed out"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let other = MockServer::start().await;
    let mut config = fixture_config(&base, &other.uri(), true)?;
    config.providers.remove("other");
    let server = server(&config).await?;
    let listing: Value = server.get("/v1/models").await.json();
    assert!(listing["data"].as_array().is_some_and(|models| {
        models
            .iter()
            .any(|entry| entry["id"] == MODEL && entry["operations"] == json!(["evaluate"]))
    }));
    let response = server.post("/v1/evaluate").json(&request()?).await;
    anyhow::ensure!(
        response.status_code() == 200,
        "real Laya evaluation returned {}",
        response.status_code()
    );
    let body: Value = response.json();
    assert_eq!(body["provider"], "laya");
    assert_eq!(body["model"], PROVIDER_MODEL_ID);
    assert_eq!(body["answers"]["billing"]["type"], "noul");
    assert_eq!(body["answers"]["team"]["type"], "choice");
    assert_eq!(body["answers"]["urgency"]["type"], "score");
    assert!(
        body["usage"]["input_tokens"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert_eq!(body["usage"]["output_tokens"], 0);
    assert!(body["usage"].get("cost").is_none() || body["usage"]["cost"].is_null());
    let pid = child.id().context("Laya child PID missing")?;
    let signal = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await?;
    anyhow::ensure!(signal.success(), "could not stop Laya process");
    let status = child.wait().await?;
    anyhow::ensure!(status.success(), "Laya child did not shut down cleanly");
    Ok(())
}
