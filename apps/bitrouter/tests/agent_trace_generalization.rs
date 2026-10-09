//! Shared semantic routing through actual App policy selection and all stock
//! inbound adapters. Provider labels are fixtures; these are contract tests.
#[path = "support/decision.rs"]
mod decision;

use anyhow::{Context, Result};
use axum_test::TestServer;
use bitrouter::policy_lock::{PolicyDefinition, PolicyLock, deterministic_yaml};
use bitrouter_sdk::config;
use bitrouter_sdk::server::{AppState, build_router};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn labels(input: &Value) -> (&str, &str, &str) {
    if input["signals"]
        .as_array()
        .is_some_and(|signals| signals.iter().any(|signal| signal["kind"] == "tool_result"))
    {
        ("code:debugging", "verify", "progressing")
    } else {
        ("code:generation", "implement", "progressing")
    }
}

async fn fixture(
    assessed: bool,
) -> Result<(
    TestServer,
    bitrouter::Assembled,
    MockServer,
    Option<MockServer>,
    tempfile::TempDir,
)> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"fixture-response", "object":"chat.completion", "model":"cheap",
            "choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":4,"completion_tokens":1,"total_tokens":5}
        }))).mount(&upstream).await;
    let dir = tempfile::tempdir()?;
    let mut lock = PolicyLock::default();
    lock.policies.insert(
        "auto".into(),
        PolicyDefinition {
            tiers: BTreeMap::from([
                ("strong".into(), "mock:strong".into()),
                ("cheap".into(), "mock:cheap".into()),
            ]),
            routes: BTreeMap::from([(
                "semantic_route/v1|code:debugging|verify|normal".into(),
                "cheap".into(),
            )]),
            default_tier: Some("strong".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["strong".into(), "cheap".into()],
            ..PolicyDefinition::default()
        },
    );
    decision::certify_routes(&mut lock);
    std::fs::write(
        dir.path().join("policy-lock.yaml"),
        deterministic_yaml(&lock)?,
    )?;
    let mut config = config::parse_with(
        &format!(
            r#"
inherit_defaults: false
server:
  skip_auth: true
database:
  url: 'sqlite::memory:'
providers:
  mock:
    api_base: '{}'
    api_key: fixture
    models: [{{id: strong}}, {{id: cheap}}]
routers:
  auto:
    selection: {{kind: policy, policy: auto, base_model: 'mock:strong'}}
policy:
  path: './policy-lock.yaml'
"#,
            upstream.uri()
        ),
        |_| None,
    )?;
    let backend = if assessed {
        Some(decision::attach(&mut config, labels).await?)
    } else {
        None
    };
    let assembled =
        bitrouter::build_app_with_path(&config, Some(&dir.path().join("bitrouter.yaml"))).await?;
    let server = TestServer::new(build_router(AppState {
        language_model: assembled.app.language_model().context("pipeline")?.clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    }));
    Ok((server, assembled, upstream, backend, dir))
}

fn requests() -> Vec<(&'static str, Value)> {
    vec![
        (
            "/v1/chat/completions",
            json!({"model":"bitrouter/auto","messages":[{"role":"user","content":"fix the failing parser"},{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{}"}}]},{"role":"tool","tool_call_id":"c1","content":"parser source"}]}),
        ),
        (
            "/v1/responses",
            json!({"model":"bitrouter/auto","input":[{"role":"user","content":"fix the failing parser"},{"type":"function_call","call_id":"c1","name":"read_file","arguments":"{}"},{"type":"function_call_output","call_id":"c1","output":"parser source"}]}),
        ),
        (
            "/v1/messages",
            json!({"model":"bitrouter/auto","max_tokens":128,"messages":[{"role":"user","content":"fix the failing parser"},{"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"read_file","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"parser source"}]}]}),
        ),
    ]
}

#[tokio::test]
async fn rich_requests_route_by_shared_semantics_across_protocols_and_private_hints() -> Result<()>
{
    let (server, _assembled, upstream, backend, _dir) = fixture(true).await?;
    let cases = requests();
    for (endpoint, body) in &cases {
        for hints in [false, true] {
            let mut request = server.post(endpoint);
            if hints {
                request = request
                    .add_header("x-bitrouter-workflow-stage", "deploy")
                    .add_header("x-bitrouter-harness", "arbitrary");
            }
            request.json(body).await.assert_status_ok();
        }
    }
    let generation = upstream
        .received_requests()
        .await
        .context("upstream capture")?;
    assert_eq!(generation.len(), cases.len() * 2);
    for request in generation {
        let body: Value = serde_json::from_slice(&request.body)?;
        assert_eq!(body["model"], "cheap");
    }
    let semantic = backend
        .context("decision backend")?
        .received_requests()
        .await
        .context("semantic capture")?;
    assert_eq!(semantic.len(), cases.len() * 2);
    for pair in semantic.as_chunks::<2>().0 {
        let left: Value = serde_json::from_slice(&pair[0].body)?;
        let right: Value = serde_json::from_slice(&pair[1].body)?;
        assert_eq!(
            left, right,
            "private hints must not alter semantic inference"
        );
        assert_eq!(left["questions"].as_object().context("questions")?.len(), 3);
    }
    Ok(())
}

#[tokio::test]
async fn missing_backend_uses_unknown_and_conservative_default() -> Result<()> {
    let (server, _assembled, upstream, _, _dir) = fixture(false).await?;
    for (endpoint, body) in requests() {
        server.post(endpoint).json(&body).await.assert_status_ok();
    }
    for request in upstream.received_requests().await.context("capture")? {
        let body: Value = serde_json::from_slice(&request.body)?;
        assert_eq!(body["model"], "strong");
    }
    Ok(())
}

#[tokio::test]
async fn invalid_named_entry_rejects_before_classification_or_generation() -> Result<()> {
    let (server, _assembled, upstream, backend, _dir) = fixture(true).await?;
    server
        .post("/v1/chat/completions")
        .json(&json!({"model":"bitrouter/missing","messages":[{"role":"user","content":"fix"}]}))
        .await
        .assert_status_bad_request();
    assert!(
        upstream
            .received_requests()
            .await
            .context("capture")?
            .is_empty()
    );
    assert!(
        backend
            .context("backend")?
            .received_requests()
            .await
            .context("capture")?
            .is_empty()
    );
    Ok(())
}
