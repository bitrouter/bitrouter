//! End-to-end integration tests.
//!
//! These build the **full assembled `App`** from a config — routing table,
//! HTTP executor, auth, policy, metering, the four inbound protocol routes —
//! and drive requests through it against a high-fidelity mock upstream
//! (`wiremock`). This exercises the whole stack: inbound protocol parse →
//! pipeline → routing → HttpExecutor → outbound protocol render → upstream →
//! response parse → metering recorder.

use bitrouter_sdk::caller::CallerContext;
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

    // metering ran — a metering row exists with the full identity context.
    let row: (i64, String) =
        sqlx::query_as("SELECT estimated_charge_micro_usd, model_id FROM requests LIMIT 1")
            .fetch_one(&assembled.pool)
            .await
            .expect("a metering row was written");
    assert_eq!(row.1, "test-model", "metering row records the routed model");
    // 11 prompt × 2 + 7 completion × 10 = 92 µ$
    assert_eq!(row.0, 92, "estimated charge derived from pricing × tokens");
}

#[tokio::test]
async fn e2e_http_server_chat_completions_end_to_end() {
    use axum_test::TestServer;

    let upstream = mock_openai_upstream().await;
    let cfg = config_for(&upstream.uri());
    let assembled = bitrouter::build_app(&cfg).await.expect("app assembles");

    let state = AppState {
        language_model: assembled.app.language_model().unwrap().clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
    };
    let server = TestServer::new(build_router(state));

    let resp = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "messages": [{ "role": "user", "content": "hello" }],
        }))
        .await;
    resp.assert_status_ok();
    let json: serde_json::Value = resp.json();
    assert_eq!(
        json["choices"][0]["message"]["content"],
        "hello from the mock upstream"
    );

    // /health is up
    let health = server.get("/health").await;
    health.assert_status_ok();

    // /metrics renders the Prometheus exposition that the pipeline just
    // accumulated. The earlier /chat/completions request fed the observer,
    // so the requests_total{outcome="completed"} counter is at least 1.
    let metrics = server.get("/metrics").await;
    metrics.assert_status_ok();
    let ct = metrics.header("content-type");
    assert!(
        ct.to_str().unwrap().starts_with("text/plain"),
        "Prometheus content-type must be text/plain (got {ct:?})"
    );
    let text = metrics.text();
    assert!(
        text.contains("bitrouter_requests_total{outcome=\"completed\"}"),
        "/metrics should expose the completed-request counter; got:\n{text}"
    );
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
async fn e2e_metering_drives_policy_spend_cap() {
    // Validates the full OSS path with no charging code: a real `brvk_`
    // key is bound to a policy with `max_spend_micro_usd: 50`. The first
    // request (92 µ$ from pricing × tokens) is allowed because rolling
    // spend reads 0µ$ at PolicyHook time; the second one is denied because
    // the 92µ$ now in the requests table is ≥ the 50µ$ cap. (Spend cap is
    // a rolling-window gate, not a per-request budget.)
    use bitrouter::auth::{NewApiKey, db as auth_db, generate};

    let upstream = mock_openai_upstream().await;

    // Same provider/pricing as `config_for`, but augmented with a policy
    // directory containing a single `pol_cap` policy with a 50µ$ ceiling.
    let policy_dir = std::env::temp_dir().join(format!(
        "bitrouter-e2e-policy-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    tokio::fs::create_dir_all(&policy_dir).await.unwrap();
    tokio::fs::write(
        policy_dir.join("pol_cap.yaml"),
        "id: pol_cap\nmax_spend_micro_usd: 50\n",
    )
    .await
    .unwrap();

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
"#,
        upstream = upstream.uri(),
        policy_path = policy_dir.display(),
    );
    let cfg: config::Config = config::parse_with(&yaml, |_| None).expect("config parses");
    let assembled = bitrouter::build_app(&cfg).await.expect("app assembles");

    // Mint a `brvk_` key bound to `pol_cap`.
    let user = "spender";
    auth_db::upsert_user(&assembled.pool, user).await.unwrap();
    let key = generate();
    let key_id = "spender-key".to_string();
    auth_db::insert_api_key(
        &assembled.pool,
        &NewApiKey {
            id: key_id.clone(),
            key_hash: key.hash.clone(),
            user_id: user.to_string(),
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: Some("pol_cap".to_string()),
        },
    )
    .await
    .unwrap();

    let pipeline = assembled.app.language_model().unwrap().clone();

    // First request: anonymous caller + bearer credential → AuthHook
    // upgrades; PolicyHook sees 0µ$ accrued < 50µ$ cap → Allow; metering
    // recorder writes 92µ$ row.
    let mut req1 = PipelineRequest::new("test-model", CallerContext::anonymous(), chat_prompt());
    req1.headers.insert(
        "authorization",
        format!("Bearer {}", key.secret).parse().unwrap(),
    );
    pipeline.execute(req1).await.expect("first request allowed");

    // Confirm the row landed with the expected estimated charge.
    let row: (i64,) =
        sqlx::query_as("SELECT estimated_charge_micro_usd FROM requests WHERE api_key_id = ?")
            .bind(&key_id)
            .fetch_one(&assembled.pool)
            .await
            .expect("metering wrote a row");
    assert_eq!(row.0, 92, "first request bills 92µ$ (11×2 + 7×10)");

    // Second request: 92µ$ already accrued ≥ 50µ$ cap → PolicyHook denies
    // at the `max_spend_micro_usd` check with 403.
    let mut req2 = PipelineRequest::new("test-model", CallerContext::anonymous(), chat_prompt());
    req2.headers.insert(
        "authorization",
        format!("Bearer {}", key.secret).parse().unwrap(),
    );
    let err = pipeline
        .execute(req2)
        .await
        .expect_err("second request denied by spend cap");
    assert_eq!(err.status(), 403, "spend-cap deny is 403");
    let msg = err.to_string();
    assert!(
        msg.contains("spend limit") && msg.contains("50"),
        "deny reason should mention spend limit and cap: {msg}"
    );

    let _ = tokio::fs::remove_dir_all(&policy_dir).await;
}

#[tokio::test]
async fn e2e_mcp_route_invokes_the_pure_routing_pipeline() {
    use async_trait::async_trait;
    use bitrouter_sdk::App;
    use bitrouter_sdk::mcp::{
        Executor, McpRequest, McpResponse, McpTarget, McpTransport, RoutingTable,
    };
    use http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    // A tiny static MCP routing table + echo executor — just enough to prove
    // the pipeline wires through the HTTP route.
    struct StaticTable;
    #[async_trait]
    impl RoutingTable for StaticTable {
        async fn resolve(
            &self,
            server: &str,
            _caller: &bitrouter_sdk::caller::CallerContext,
        ) -> bitrouter_sdk::Result<McpTarget> {
            if server == "known" {
                Ok(McpTarget {
                    server_name: server.to_string(),
                    transport: McpTransport::Stdio {
                        command: "/bin/true".into(),
                        args: vec![],
                        env: Default::default(),
                    },
                })
            } else {
                Err(bitrouter_sdk::BitrouterError::NotFound(format!(
                    "unknown mcp server '{server}'"
                )))
            }
        }
    }
    struct EchoExecutor;
    #[async_trait]
    impl Executor for EchoExecutor {
        async fn execute(
            &self,
            target: &McpTarget,
            request: &McpRequest,
        ) -> bitrouter_sdk::Result<McpResponse> {
            Ok(McpResponse {
                request_id: request.request_id.clone(),
                result: serde_json::json!({
                    "method": request.method,
                    "server": target.server_name,
                    "params_seen": request.params,
                }),
            })
        }
    }

    // Minimal LM executor — never actually called by the MCP route. We only
    // need a Pipeline to satisfy AppState.language_model.
    struct UnusedLmExecutor;
    #[async_trait]
    impl bitrouter_sdk::language_model::Executor for UnusedLmExecutor {
        async fn execute(
            &self,
            _target: &bitrouter_sdk::language_model::RoutingTarget,
            _prompt: &bitrouter_sdk::language_model::Prompt,
        ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::ExecutionResult> {
            Err(bitrouter_sdk::BitrouterError::internal(
                "unused in this test",
            ))
        }
        async fn execute_stream(
            &self,
            _target: &bitrouter_sdk::language_model::RoutingTarget,
            _prompt: &bitrouter_sdk::language_model::Prompt,
        ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::StreamPartStream> {
            Err(bitrouter_sdk::BitrouterError::internal(
                "unused in this test",
            ))
        }
    }

    // Build an App with both pipelines — the LM is just enough to satisfy
    // AppState; the test exercises POST /mcp/{name}.
    let app = App::builder()
        .language_model(|lm| {
            lm.routing_table(Arc::new(
                bitrouter_sdk::language_model::StaticRoutingTable::new(),
            ))
            .executor(Arc::new(UnusedLmExecutor));
        })
        .mcp(|m| {
            m.routing_table(Arc::new(StaticTable))
                .executor(Arc::new(EchoExecutor));
        })
        .skip_auth(true)
        .build()
        .expect("app builds");

    let state = AppState {
        language_model: app.language_model().unwrap().clone(),
        mcp: app.mcp().cloned(),
        skip_auth: true,
        metrics_renderer: None,
    };
    let router = build_router(state);

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": { "filter": "" }
    });
    let request = Request::builder()
        .method("POST")
        .uri("/mcp/known")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["jsonrpc"], "2.0");
    // JSON-RPC 2.0 — response MUST echo the inbound `id` verbatim, not a fresh
    // server-side UUID (modelcontextprotocol.io/specification/2025-06-18/basic).
    assert_eq!(json["id"], 1);
    assert_eq!(json["result"]["method"], "tools/list");
    assert_eq!(json["result"]["server"], "known");

    // String id also round-trips.
    let body_str = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "req-abc-42",
        "method": "tools/list",
        "params": {}
    });
    let request = Request::builder()
        .method("POST")
        .uri("/mcp/known")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body_str.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["id"], "req-abc-42");

    // Missing `jsonrpc` field — JSON-RPC error envelope at HTTP 400, id echoed.
    let bad_envelope = serde_json::json!({"id": 7, "method": "tools/list", "params": {}});
    let request = Request::builder()
        .method("POST")
        .uri("/mcp/known")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(bad_envelope.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 400);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["jsonrpc"], "2.0");
    assert_eq!(json["id"], 7);
    assert_eq!(json["error"]["code"], -32600);

    // Unsupported MCP-Protocol-Version is rejected with 400 per spec.
    let request = Request::builder()
        .method("POST")
        .uri("/mcp/known")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2099-01-01")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 400);

    // Non-loopback Origin rejected with 403 to defeat DNS rebinding.
    let request = Request::builder()
        .method("POST")
        .uri("/mcp/known")
        .header("content-type", "application/json")
        .header("origin", "http://evil.example.com")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 403);

    // Unknown MCP server → JSON-RPC "Method not found" wrapped in HTTP 404.
    let bad = Request::builder()
        .method("POST")
        .uri("/mcp/no-such-server")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = router.oneshot(bad).await.unwrap();
    assert_eq!(response.status(), 404);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["jsonrpc"], "2.0");
    assert_eq!(json["id"], 1);
    assert_eq!(json["error"]["code"], -32601);
}
