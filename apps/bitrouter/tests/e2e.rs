//! Phase-5 end-to-end integration tests.
//!
//! These build the **full assembled `App`** from a config — routing table,
//! HTTP executor, auth, policy, settlement, the four inbound protocol routes —
//! and drive requests through it against a high-fidelity mock upstream
//! (`wiremock`). This exercises the whole stack: inbound protocol parse →
//! pipeline → routing → HttpExecutor → outbound protocol render → upstream →
//! response parse → settlement → receipt.

use bitrouter_sdk::caller::{CallerContext, PaymentMethod};
use bitrouter_sdk::config;
use bitrouter_sdk::language_model::{GenerationParams, Message, PipelineRequest, Prompt, Role};
use bitrouter_sdk::server::{AppState, build_router};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Stand up a wiremock upstream speaking OpenAI Chat Completions.
async fn mock_openai_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello from the mock upstream" },
                "finish_reason": "stop",
            }],
            "usage": { "prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18 },
        })))
        .mount(&server)
        .await;
    server
}

/// A config pointing one provider at the mock upstream, `skip_auth: true` and
/// an in-memory database.
fn config_for(upstream: &str) -> config::Config {
    let yaml = format!(
        r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: true
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
"#
    );
    config::parse_with(&yaml, |_| None).expect("config parses")
}

fn chat_prompt() -> Prompt {
    Prompt {
        model: "test-model".to_string(),
        system: None,
        messages: vec![Message::text(Role::User, "hello")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        stream: false,
    }
}

#[tokio::test]
async fn e2e_assembled_pipeline_routes_to_mock_provider() {
    let upstream = mock_openai_upstream().await;
    let cfg = config_for(&upstream.uri());

    // Assemble the FULL app — db + migrations + routing + auth + policy +
    // settlement + the language_model pipeline.
    let assembled = bitrouter::build_app(&cfg).await.expect("app assembles");
    let pipeline = assembled
        .app
        .language_model()
        .expect("language_model pipeline configured")
        .clone();

    // skip_auth is on → a local caller passes through AuthHook.
    let req = PipelineRequest::new("test-model", CallerContext::local(), chat_prompt());
    let resp = pipeline.execute(req).await.expect("request succeeds e2e");

    // the mock upstream's content made it all the way back through conversion
    let text: String = resp
        .result
        .content
        .iter()
        .filter_map(|c| match c {
            bitrouter_sdk::language_model::Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello from the mock upstream");

    // settlement ran — a local (PaymentMethod::None) caller is not charged, but
    // a receipt row exists with the full identity context.
    let row: (i64, String) =
        sqlx::query_as("SELECT final_charge_micro_usd, model_id FROM requests LIMIT 1")
            .fetch_one(&assembled.pool)
            .await
            .expect("a receipt was written");
    assert_eq!(row.1, "test-model", "receipt records the routed model");
}

#[tokio::test]
async fn e2e_http_server_chat_completions_end_to_end() {
    use http::Request;
    use tower::ServiceExt;

    let upstream = mock_openai_upstream().await;
    let cfg = config_for(&upstream.uri());
    let assembled = bitrouter::build_app(&cfg).await.expect("app assembles");

    // Build the axum router directly (no port bind) and drive it with a
    // oneshot request — exercises server.rs + the inbound OpenAI adapter.
    let state = AppState {
        language_model: assembled.app.language_model().unwrap().clone(),
        skip_auth: assembled.app.skip_auth(),
    };
    let router = build_router(state);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{ "role": "user", "content": "hello" }],
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["choices"][0]["message"]["content"],
        "hello from the mock upstream"
    );

    // /health is up
    let health = router
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
}

#[tokio::test]
async fn e2e_unknown_model_is_a_clean_404() {
    let upstream = mock_openai_upstream().await;
    let cfg = config_for(&upstream.uri());
    let assembled = bitrouter::build_app(&cfg).await.expect("app assembles");
    let pipeline = assembled.app.language_model().unwrap().clone();

    let mut prompt = chat_prompt();
    prompt.model = "no-such-model".to_string();
    let req = PipelineRequest::new("no-such-model", CallerContext::local(), prompt);
    let err = pipeline.execute(req).await.unwrap_err();
    // no DEFAULT_PROVIDER fallback — a clean 404
    assert_eq!(err.status(), 404);
}

#[tokio::test]
async fn e2e_credits_caller_authenticates_and_is_charged() {
    let upstream = mock_openai_upstream().await;
    let cfg = config_for(&upstream.uri());
    let assembled = bitrouter::build_app(&cfg).await.expect("app assembles");

    // Mint a real brvk_ key for a credits user and give them a balance — the
    // full auth → policy → settlement path, not a hand-built caller.
    let key = bitrouter_auth::generate();
    bitrouter_auth::db::upsert_user(&assembled.pool, "payer")
        .await
        .unwrap();
    bitrouter_auth::db::insert_api_key(
        &assembled.pool,
        &bitrouter_auth::NewApiKey {
            id: "payer-key".to_string(),
            key_hash: key.hash.clone(),
            user_id: "payer".to_string(),
            payment_method: PaymentMethod::Credits,
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: None,
        },
    )
    .await
    .unwrap();
    bitrouter_settlement::add_credits(&assembled.pool, "payer", 1_000_000)
        .await
        .unwrap();

    let pipeline = assembled.app.language_model().unwrap().clone();
    // Start anonymous + present the credential — AuthHook upgrades the caller.
    let mut req = PipelineRequest::new("test-model", CallerContext::anonymous(), chat_prompt());
    req.headers.insert(
        "authorization",
        format!("Bearer {}", key.secret).parse().unwrap(),
    );
    let resp = pipeline.execute(req).await.expect("request succeeds");

    // mock usage 11 prompt + 7 completion; pricing 2 / 10 µ$ → 11*2 + 7*10 = 92
    assert_eq!(resp.final_charge_micro_usd, 92);
    let balance = bitrouter_settlement::credit_balance(&assembled.pool, "payer")
        .await
        .unwrap();
    assert_eq!(balance, 1_000_000 - 92);
}
