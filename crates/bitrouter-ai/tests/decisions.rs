//! Native decision fidelity and selected-target invocation contracts.

use bitrouter_ai::client::{HttpTimeouts, ModelClient};
use bitrouter_ai::decisions::DecisionAnswer;
use bitrouter_ai::error::ModelError;
use bitrouter_ai::protocol::decisions::DecisionsCodec;
use bitrouter_ai::target::ModelTarget;
use bitrouter_ai::types::{ApiProtocol, AuthScheme, UsageOrigin};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn request() -> Value {
    json!({
        "model": "public-selector",
        "input": [{"role":"user","type":"message","content":[
            {"type":"input_text","text":"evidence"},
            {"type":"input_image","image_url":"data:image/png;base64,AQID","detail":"original"}
        ]}],
        "safety_identifier":"opaque-user",
        "questions":[
            {"type":"predicate","name":"check","instructions":"A condition?"},
            {"type":"choice","instructions":"Select a value.","choices":[
                {"value":true,"description":"Boolean"},
                {"value":"true","description":"String"}
            ]},
            {"type":"score","name":"severity","instructions":"Rate the evidence.","levels":[
                {"label":"low"},{"label":"high","description":"Highest level"}
            ]}
        ]
    })
}

fn usage() -> Value {
    json!({"input_tokens":42,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
        "output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":42})
}

fn response() -> Value {
    json!({"model":"native-model","answers":[
        {"type":"predicate","name":"check","probability":0.75,"future_detail":{"v":1}},
        {"type":"choice","name":null,"choice":true,"probabilities":[
            {"value":true,"probability":0.8},{"value":"true","probability":0.2}
        ],"confidence":0.4},
        {"type":"score","name":"severity","score":0.7,"probabilities":[
            {"value":0,"label":"low","probability":0.3},
            {"value":1,"label":"high","probability":0.7}
        ],"confidence":0.2}
    ],"usage":usage(),"future_envelope":{"version":2}})
}

fn target(base: String) -> ModelTarget {
    ModelTarget {
        provider_name: "fixture".into(),
        service_id: "native-model".into(),
        api_protocol: ApiProtocol::Decisions,
        api_base: base,
        api_key: "private-key".into(),
        credential_priority: Default::default(),
        account_label: None,
        auth_scheme: AuthScheme::Bearer,
        compatibility: Default::default(),
    }
}

#[test]
fn native_round_trip_preserves_typed_values_and_extensions() -> TestResult {
    let request = DecisionsCodec::parse_request(request())?;
    assert_eq!(DecisionsCodec::render_request(&request)?, self::request());
    let result = DecisionsCodec::parse_response(response(), &request)?;
    assert_eq!(result.usage.origin, UsageOrigin::ProviderReported);
    assert_eq!(result.usage.completion_tokens, 0);
    assert_eq!(
        DecisionsCodec::render_response(&result, &request)?,
        response()
    );
    Ok(())
}

#[test]
fn nullable_native_fields_retain_explicit_null() -> TestResult {
    let mut body = request();
    body["safety_identifier"] = Value::Null;
    body["input"][0]["content"][1]["detail"] = Value::Null;
    let request = DecisionsCodec::parse_request(body.clone())?;
    assert_eq!(DecisionsCodec::render_request(&request)?, body);
    Ok(())
}

#[test]
fn unsupported_inputs_are_refused_without_echoing_private_values() -> TestResult {
    let mut inputs = Vec::new();
    let mut value = request();
    value["stream"] = json!(true);
    inputs.push(value);
    let mut value = request();
    value["input"][0]["role"] = json!("private-role");
    inputs.push(value);
    let mut value = request();
    value["input"][0]["content"][1]["image_url"] = json!("https://private-image.invalid/a");
    inputs.push(value);
    let mut value = request();
    value["questions"][0]["private-field"] = json!("private-body");
    inputs.push(value);
    for input in inputs {
        let error = DecisionsCodec::parse_request(input)
            .err()
            .ok_or("input admitted")?;
        assert!(matches!(error, ModelError::InvalidRequest { .. }));
        let diagnostic = format!("{error:?} {error}");
        for private in [
            "private-role",
            "private-image",
            "private-field",
            "private-body",
        ] {
            assert!(!diagnostic.contains(private));
        }
    }
    Ok(())
}

#[test]
fn refusals_and_invalid_answers_keep_independent_usage_evidence() -> TestResult {
    let request = DecisionsCodec::parse_request(request())?;
    let mut refusal = response();
    refusal["answers"][1] = json!({"type":"refusal","name":null});
    let result = DecisionsCodec::parse_response(refusal, &request)?;
    assert!(matches!(result.answers[1], DecisionAnswer::Refusal { .. }));
    let mut malformed = response();
    malformed["answers"][1]["choice"] = json!("private-invalid-choice");
    malformed["usage"]["private-extension"] = json!("private-key");
    let error = DecisionsCodec::parse_response(malformed, &request)
        .err()
        .ok_or("answer admitted")?;
    assert_eq!(
        error.decision_usage().map(|usage| usage.prompt_tokens),
        Some(42)
    );
    assert!(error.is_completed_decision_failure());
    assert!(!format!("{error:?} {error}").contains("private-invalid-choice"));
    assert!(!format!("{error:?} {error}").contains("private-key"));
    Ok(())
}

#[test]
fn image_budget_and_inconsistent_usage_are_refused() -> TestResult {
    let mut body = request();
    let image = body["input"][0]["content"][1].clone();
    body["input"][0]["content"] = Value::Array(vec![image.clone(); 128]);
    DecisionsCodec::parse_request(body.clone())?;
    body["input"][0]["content"] = Value::Array(vec![image; 129]);
    assert!(DecisionsCodec::parse_request(body).is_err());
    let request = DecisionsCodec::parse_request(self::request())?;
    let mut body = response();
    body["usage"]["input_tokens_details"]["cached_tokens"] = json!(43);
    let error = DecisionsCodec::parse_response(body, &request)
        .err()
        .ok_or("usage admitted")?;
    assert!(error.decision_usage().is_none());
    assert!(error.is_completed_decision_failure());
    let mut body = response();
    body["answers"] = json!([
        {"type":"refusal","name":"check"},
        {"type":"refusal","name":null},
        {"type":"refusal","name":"severity"}
    ]);
    assert_eq!(
        DecisionsCodec::parse_response(body, &request)?
            .usage
            .prompt_tokens,
        42
    );
    Ok(())
}

#[derive(Default)]
struct SelectedAuth {
    refreshes: AtomicUsize,
    preparations: AtomicUsize,
}

#[async_trait::async_trait]
impl bitrouter_ai::auth::AuthApplier for SelectedAuth {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        target: &ModelTarget,
    ) -> bitrouter_ai::error::Result<reqwest::Request> {
        assert_eq!(target.account_label.as_deref(), Some("selected"));
        let value = if self.refreshes.load(Ordering::SeqCst) == 0 {
            "Bearer first-private"
        } else {
            "Bearer replacement-private"
        };
        request.headers_mut().insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static(value),
        );
        Ok(request)
    }

    async fn prepare_body(
        &self,
        _body: &mut Value,
        _target: &ModelTarget,
    ) -> bitrouter_ai::error::Result<()> {
        self.preparations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn refresh_after_unauthorized(
        &self,
        target: &ModelTarget,
        _rejected: Option<&reqwest::header::HeaderValue>,
    ) -> bitrouter_ai::error::Result<bool> {
        assert_eq!(target.account_label.as_deref(), Some("selected"));
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
}

#[tokio::test]
async fn authentication_refresh_rebuilds_once_for_the_same_selected_account() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(path("/decisions"))
        .and(header("authorization", "Bearer first-private"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/decisions"))
        .and(header("authorization", "Bearer replacement-private"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response()))
        .expect(1)
        .mount(&server)
        .await;
    let auth = Arc::new(SelectedAuth::default());
    let client = ModelClient::new(HttpTimeouts::default())?
        .with_auth_appliers(bitrouter_ai::auth::AuthAppliers::new().with("fixture", auth.clone()));
    let mut target = target(server.uri());
    target.account_label = Some("selected".into());
    let request = DecisionsCodec::parse_request(request())?;
    client
        .decide(&target, &request, &CancellationToken::new())
        .await?;
    assert_eq!(auth.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(auth.preparations.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn malformed_answers_return_usage_without_replaying_or_exposing_credentials() -> TestResult {
    let server = MockServer::start().await;
    let mut body = response();
    body["answers"] = json!([{ "type":"private-key", "name":null }]);
    body["usage"]["echo"] = json!("private-key");
    Mock::given(path("/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    let client = ModelClient::new(HttpTimeouts::default())?;
    let request = DecisionsCodec::parse_request(request())?;
    let error = client
        .decide(&target(server.uri()), &request, &CancellationToken::new())
        .await
        .err()
        .ok_or("malformed response succeeded")?;
    assert!(error.is_completed_decision_failure());
    let usage = error.decision_usage().ok_or("usage was lost")?;
    assert_eq!(usage.prompt_tokens, 42);
    assert!(!format!("{error:?} {error} {:?}", usage.raw).contains("private-key"));
    Ok(())
}

#[tokio::test]
async fn selected_call_projects_model_without_mutating_source() -> TestResult {
    let server = MockServer::start().await;
    let request = DecisionsCodec::parse_request(request())?;
    let before = request.clone();
    let mut expected = self::request();
    expected["model"] = json!("native-model");
    Mock::given(method("POST"))
        .and(path("/decisions"))
        .and(header("authorization", "Bearer private-key"))
        .and(body_json(expected))
        .respond_with(ResponseTemplate::new(200).set_body_json(response()))
        .expect(1)
        .mount(&server)
        .await;
    let client = ModelClient::new(HttpTimeouts::default())?;
    let result = client
        .decide(&target(server.uri()), &request, &CancellationToken::new())
        .await?;
    assert_eq!(result.model, "native-model");
    assert_eq!(request, before);
    Ok(())
}

#[tokio::test]
async fn operation_mismatch_and_cancellation_stop_before_dispatch() -> TestResult {
    let server = MockServer::start().await;
    let request = DecisionsCodec::parse_request(request())?;
    let client = ModelClient::new(HttpTimeouts::default())?;
    let mut wrong = target(server.uri());
    wrong.api_protocol = ApiProtocol::Responses;
    assert!(
        client
            .decide(&wrong, &request, &CancellationToken::new())
            .await
            .is_err()
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        client
            .decide(&target(server.uri()), &request, &cancellation)
            .await,
        Err(ModelError::Cancelled)
    ));
    let received = server
        .received_requests()
        .await
        .ok_or("requests unavailable")?;
    assert!(received.is_empty());
    Ok(())
}
