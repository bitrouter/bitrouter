//! End-to-end integration tests.
//!
//! These build the **full assembled `App`** from a config — routing table,
//! HTTP executor, auth, policy, metering, the four inbound protocol routes —
//! and drive requests through it against a high-fidelity mock upstream
//! (`wiremock`). This exercises the whole stack: inbound protocol parse →
//! pipeline → routing → HttpExecutor → outbound protocol render → upstream →
//! response parse → metering recorder.
//!
//! The `structured_outputs_matrix` block at the bottom of this file is a
//! 4×4 (inbound × outbound) sweep for `response_format` (PR #472). It uses
//! the same assembled-router + wiremock setup as the other tests but with
//! a four-protocol upstream and a four-provider config, so it lives here
//! rather than in its own file.

use axum_test::TestServer;
use bitrouter::metering::entities::requests;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::config;
use bitrouter_sdk::language_model::{GenerationParams, Message, PipelineRequest, Prompt, Role};
use bitrouter_sdk::server::{AppState, build_router};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Stand up a wiremock upstream speaking Chat Completions.
async fn mock_chat_completions_upstream() -> MockServer {
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
      - "*": chat_completions
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
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "hello")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    }
}

#[tokio::test]
async fn e2e_assembled_pipeline_routes_to_mock_provider() {
    let upstream = mock_chat_completions_upstream().await;
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
            bitrouter_sdk::language_model::Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello from the mock upstream");

    // metering ran — a metering row exists with the full identity context.
    let row = requests::Entity::find()
        .one(&assembled.db)
        .await
        .expect("metering query runs")
        .expect("a metering row was written");
    assert_eq!(
        row.model_id, "test-model",
        "metering row records the routed model"
    );
    // 11 prompt × 2 + 7 completion × 10 = 92 µ$
    assert_eq!(
        row.estimated_charge_micro_usd, 92,
        "estimated charge derived from pricing × tokens"
    );
}

#[tokio::test]
async fn e2e_http_server_chat_completions_end_to_end() {
    use axum_test::TestServer;

    let upstream = mock_chat_completions_upstream().await;
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

    // `/metrics` no longer serves Prometheus exposition — metrics are pushed
    // via OTLP. The route remains bound (so existing scraper config does not
    // 404 on upgrade) and renders a deprecation banner steering operators
    // toward OTLP push. Live emission is exercised separately via
    // `scripts/test_observability.sh` against a real OTel collector — this
    // e2e test has no OTLP mock and is not the place to assert it.
    let metrics = server.get("/metrics").await;
    metrics.assert_status_ok();
    let ct = metrics.header("content-type");
    assert!(
        ct.to_str().unwrap().starts_with("text/plain"),
        "/metrics content-type stays text/plain (got {ct:?})"
    );
    let text = metrics.text();
    assert!(
        text.contains("Prometheus metrics have been removed"),
        "/metrics should render the OTLP-migration banner; got:\n{text}"
    );
    assert!(
        text.contains("bitrouter-observe.otel"),
        "/metrics banner should point at the new config key; got:\n{text}"
    );
}

#[tokio::test]
async fn e2e_unknown_model_is_a_clean_404() {
    let upstream = mock_chat_completions_upstream().await;
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

    let upstream = mock_chat_completions_upstream().await;

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
      - "*": chat_completions
    models:
      - id: test-model
        pricing:
          input_micro_usd_per_token: 2.0
          output_micro_usd_per_token: 10.0
plugins:
  bitrouter-policy:
    policy_dir: '{policy_path}'
"#,
        upstream = upstream.uri(),
        // Single-quoted YAML scalar — double quotes would interpret the
        // backslashes in a Windows temp path (`C:\Users\…`) as escape
        // sequences and trip the parser on `\U`. Single quotes treat the
        // value literally; `temp_dir()` paths never contain a single quote.
        policy_path = policy_dir.display(),
    );
    let cfg: config::Config = config::parse_with(&yaml, |_| None).expect("config parses");
    let assembled = bitrouter::build_app(&cfg).await.expect("app assembles");

    // Mint a `brvk_` key bound to `pol_cap`.
    let user = "spender";
    auth_db::upsert_user(&assembled.db, user).await.unwrap();
    let key = generate();
    let key_id = "spender-key".to_string();
    auth_db::insert_api_key(
        &assembled.db,
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
    let row = requests::Entity::find()
        .filter(requests::Column::ApiKeyId.eq(&key_id))
        .one(&assembled.db)
        .await
        .expect("metering query runs")
        .expect("metering wrote a row");
    assert_eq!(
        row.estimated_charge_micro_usd, 92,
        "first request bills 92µ$ (11×2 + 7×10)"
    );

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
    use bitrouter_sdk::mcp::transport::McpTransport;
    use bitrouter_sdk::mcp::{
        Executor, McpRequest, McpResponse, McpTarget, RoutingTable, ServerSelector,
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
            selector: &ServerSelector,
            _caller: &bitrouter_sdk::caller::CallerContext,
        ) -> bitrouter_sdk::Result<McpTarget> {
            match selector {
                ServerSelector::Direct(name) if name == "known" => Ok(McpTarget::Direct {
                    server_name: name.clone(),
                    transport: McpTransport::Stdio {
                        command: "/bin/true".into(),
                        args: vec![],
                        env: Default::default(),
                    },
                }),
                ServerSelector::Direct(name) => Err(bitrouter_sdk::BitrouterError::NotFound(
                    format!("unknown mcp server '{name}'"),
                )),
                ServerSelector::Aggregate => Ok(McpTarget::Aggregate { members: vec![] }),
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
            let server = match target {
                McpTarget::Direct { server_name, .. } => server_name.clone(),
                McpTarget::Aggregate { .. } => "<aggregate>".to_string(),
            };
            Ok(McpResponse {
                request_id: request.request_id.clone(),
                result: serde_json::json!({
                    "method": request.method,
                    "server": server,
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
            _ctx: &bitrouter_sdk::language_model::PipelineContext,
        ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::ExecutionResult> {
            Err(bitrouter_sdk::BitrouterError::internal(
                "unused in this test",
            ))
        }
        async fn execute_stream(
            &self,
            _target: &bitrouter_sdk::language_model::RoutingTarget,
            _prompt: &bitrouter_sdk::language_model::Prompt,
            _ctx: &bitrouter_sdk::language_model::PipelineContext,
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

    // MCP lifecycle: `initialize` is answered by the gateway itself (not proxied
    // to the executor, which only knows tools/resources/prompts), so compliant
    // clients can complete the handshake. A supported requested protocolVersion
    // is echoed; serverInfo identifies the gateway.
    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {} }
    });
    let request = Request::builder()
        .method("POST")
        .uri("/mcp/known")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(init.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["id"], 1);
    assert_eq!(json["result"]["protocolVersion"], "2025-06-18");
    assert!(json["result"]["capabilities"]["tools"].is_object());
    assert_eq!(
        json["result"]["serverInfo"]["name"],
        "bitrouter-mcp-gateway"
    );

    // An unsupported requested version falls back to the gateway's latest.
    let init_old = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "initialize",
        "params": { "protocolVersion": "1999-01-01" }
    });
    let request = Request::builder()
        .method("POST")
        .uri("/mcp/known")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(init_old.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["result"]["protocolVersion"], "2025-11-25");

    // `notifications/initialized` is a notification — acked with 202, no body.
    let note = serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let request = Request::builder()
        .method("POST")
        .uri("/mcp/known")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(note.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 202);

    // `ping` → empty result, never proxied to the executor.
    let ping = serde_json::json!({ "jsonrpc": "2.0", "id": 9, "method": "ping" });
    let request = Request::builder()
        .method("POST")
        .uri("/mcp/known")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(ping.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["id"], 9);
    assert!(json["result"].as_object().unwrap().is_empty());

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

#[tokio::test]
async fn e2e_mcp_aggregate_and_sse_endpoints() {
    use async_trait::async_trait;
    use bitrouter_sdk::App;
    use bitrouter_sdk::mcp::transport::McpTransport;
    use bitrouter_sdk::mcp::{
        AggregateMember, Executor, McpRequest, McpResponse, McpTarget, RoutingTable, ServerSelector,
    };
    use bitrouter_sdk::server::{RouterOptions, build_router_with_options};
    use http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    // Two-member aggregate table. The executor branches on Direct vs
    // Aggregate so we test the SDK's AggregatingExecutor through its real
    // wiring rather than reimplementing fan-out in the fixture.
    struct TwoServerTable;
    #[async_trait]
    impl RoutingTable for TwoServerTable {
        async fn resolve(
            &self,
            selector: &ServerSelector,
            _caller: &bitrouter_sdk::caller::CallerContext,
        ) -> bitrouter_sdk::Result<McpTarget> {
            let stub = McpTransport::Stdio {
                command: "/bin/true".into(),
                args: vec![],
                env: Default::default(),
            };
            match selector {
                ServerSelector::Direct(name) => Ok(McpTarget::Direct {
                    server_name: name.clone(),
                    transport: stub,
                }),
                ServerSelector::Aggregate => Ok(McpTarget::Aggregate {
                    members: vec![
                        AggregateMember {
                            server_name: "a".into(),
                            tool_prefix: "a__".into(),
                            transport: stub.clone(),
                        },
                        AggregateMember {
                            server_name: "b".into(),
                            tool_prefix: "b__".into(),
                            transport: stub,
                        },
                    ],
                }),
            }
        }
    }

    struct StaticToolsExecutor;
    #[async_trait]
    impl Executor for StaticToolsExecutor {
        async fn execute(
            &self,
            target: &McpTarget,
            request: &McpRequest,
        ) -> bitrouter_sdk::Result<McpResponse> {
            let server = match target {
                McpTarget::Direct { server_name, .. } => server_name.clone(),
                McpTarget::Aggregate { .. } => unreachable!("AggregatingExecutor must intercept"),
            };
            let tools = serde_json::json!([
                { "name": "search", "description": format!("from {server}") },
            ]);
            Ok(McpResponse {
                request_id: request.request_id.clone(),
                result: serde_json::json!({ "tools": tools }),
            })
        }
    }

    struct UnusedLmExecutor;
    #[async_trait]
    impl bitrouter_sdk::language_model::Executor for UnusedLmExecutor {
        async fn execute(
            &self,
            _target: &bitrouter_sdk::language_model::RoutingTarget,
            _prompt: &bitrouter_sdk::language_model::Prompt,
            _ctx: &bitrouter_sdk::language_model::PipelineContext,
        ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::ExecutionResult> {
            Err(bitrouter_sdk::BitrouterError::internal("unused"))
        }
        async fn execute_stream(
            &self,
            _target: &bitrouter_sdk::language_model::RoutingTarget,
            _prompt: &bitrouter_sdk::language_model::Prompt,
            _ctx: &bitrouter_sdk::language_model::PipelineContext,
        ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::StreamPartStream> {
            Err(bitrouter_sdk::BitrouterError::internal("unused"))
        }
    }

    let app = App::builder()
        .language_model(|lm| {
            lm.routing_table(Arc::new(
                bitrouter_sdk::language_model::StaticRoutingTable::new(),
            ))
            .executor(Arc::new(UnusedLmExecutor));
        })
        .mcp(|m| {
            // The pipeline-level executor is the real AggregatingExecutor,
            // wrapping our canned StaticToolsExecutor at the leaves.
            let inner: Arc<StaticToolsExecutor> = Arc::new(StaticToolsExecutor);
            let agg = bitrouter_sdk::mcp::aggregating_executor::AggregatingExecutor::new(inner);
            m.routing_table(Arc::new(TwoServerTable))
                .executor(Arc::new(agg));
        })
        .mcp_aggregate_route("/mcp")
        .skip_auth(true)
        .build()
        .expect("app builds");

    let state = AppState {
        language_model: app.language_model().unwrap().clone(),
        mcp: app.mcp().cloned(),
        skip_auth: true,
        metrics_renderer: None,
    };
    let options = RouterOptions {
        omit_v1_models: false,
        mcp_aggregate_route: app.mcp_aggregate_route().map(String::from),
        router_wrapper: None,
    };
    let router = build_router_with_options(state, options);

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });

    // (1) POST /mcp returns merged tools/list with `{prefix}{name}`.
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let names: Vec<String> = json["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["a__search", "b__search"]);

    // (2) Accept: text/event-stream on the aggregate route returns SSE.
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .expect("SSE response must carry Content-Type")
        .to_str()
        .unwrap();
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected SSE Content-Type, got: {content_type}"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let body_str = String::from_utf8(bytes.to_vec()).unwrap();
    // SSE body contains at least one `data:` line carrying the JSON-RPC result.
    let data_line = body_str
        .lines()
        .find(|l| l.starts_with("data: "))
        .expect("SSE stream must include a data event");
    let payload: serde_json::Value =
        serde_json::from_str(data_line.trim_start_matches("data: ")).unwrap();
    assert_eq!(payload["jsonrpc"], "2.0");
    assert_eq!(payload["id"], 1);
    assert_eq!(payload["result"]["tools"][0]["name"], "a__search");

    // (3) JSON path on the per-server route still works after refactor.
    let request = Request::builder()
        .method("POST")
        .uri("/mcp/a")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["result"]["tools"][0]["name"], "search");
}

// ============================================================================
// structured outputs — the 4×4 inbound-protocol × outbound-protocol matrix
// for `response_format` (PR #472).
//
// Same assembly model as the tests above (assembled router + wiremock
// upstream + axum_test). The matrix supplies the same JSON schema in each
// inbound protocol's native shape and asserts it lands at the right native
// field on the wire to the upstream:
//
//   Chat Completions:      response_format.json_schema.schema
//   Responses: text.format.schema
//   Anthropic:        output_config.format.schema
//   Google:           generationConfig.responseSchema  (paired with
//                     responseMimeType == "application/json")
//
// `name` / `strict` are dropped on the outbound to Anthropic and Google
// because those native APIs don't carry them.
//
// Capability-gate coverage (a `Custom` outbound adapter without
// `supports_response_format()` produces a 400) lives at the SDK level in
// `crates/bitrouter-sdk/src/language_model/tests.rs ::
// executor_rejects_response_format_on_unsupported_outbound`. We don't
// duplicate it here because the gate fires inside `HttpExecutor` before
// any HTTP-level transport detail (URL, auth) matters.
// ============================================================================

/// The schema attached to every matrix request. Small but recognisable so
/// the per-outbound assertions can compare on `properties.city.type`.
fn matrix_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "city": { "type": "string" } },
        "required": ["city"],
        "additionalProperties": false,
    })
}

/// Model id → outbound `api_protocol`. Same upstream serves all four; the
/// chosen model id picks the provider (and therefore the outbound protocol).
const MODEL_VIA_OPENAI: &str = "model-via-openai";
const MODEL_VIA_ANTHROPIC: &str = "model-via-anthropic";
const MODEL_VIA_RESPONSES: &str = "model-via-responses";
const MODEL_VIA_GOOGLE: &str = "model-via-google";

/// Stand up one MockServer that speaks all four outbound wire formats on the
/// path each provider's transport actually hits.
async fn upstream_for_all_protocols() -> MockServer {
    let server = MockServer::start().await;

    // Chat Completions — POST /chat/completions
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": "test",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "{\"city\":\"sf\"}" },
                "finish_reason": "stop",
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
        })))
        .mount(&server)
        .await;

    // Responses — POST /responses
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp-1",
            "object": "response",
            "status": "completed",
            "model": "test",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "{\"city\":\"sf\"}" }],
            }],
            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 },
        })))
        .mount(&server)
        .await;

    // Messages — POST /messages
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg-1",
            "type": "message",
            "role": "assistant",
            "model": "test",
            "content": [{ "type": "text", "text": "{\"city\":\"sf\"}" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 },
        })))
        .mount(&server)
        .await;

    // Generate Content — POST /models/{model}:generateContent (model id varies per
    // test; match on path prefix + suffix).
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex(
            r"^/models/[^/]+:generateContent$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": "{\"city\":\"sf\"}" }] },
                "finishReason": "STOP",
            }],
            "usageMetadata": {
                "promptTokenCount": 1,
                "candidatesTokenCount": 1,
                "totalTokenCount": 2,
            },
        })))
        .mount(&server)
        .await;

    server
}

/// Build a config with four providers, each speaking one outbound protocol
/// and each routing one named model.
fn config_for_matrix(upstream: &str) -> config::Config {
    let yaml = format!(
        r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: true
database:
  url: "sqlite::memory:"
providers:
  via_openai:
    api_base: {upstream}
    api_key: test-key
    api_protocol:
      - "*": chat_completions
    models:
      - id: {MODEL_VIA_OPENAI}
  via_anthropic:
    api_base: {upstream}
    api_key: test-key
    api_protocol:
      - "*": messages
    models:
      - id: {MODEL_VIA_ANTHROPIC}
  via_responses:
    api_base: {upstream}
    api_key: test-key
    api_protocol:
      - "*": responses
    models:
      - id: {MODEL_VIA_RESPONSES}
  via_google:
    api_base: {upstream}
    api_key: test-key
    api_protocol:
      - "*": generate_content
    models:
      - id: {MODEL_VIA_GOOGLE}
"#
    );
    config::parse_with(&yaml, |_| None).expect("config parses")
}

/// Assemble app + router + axum_test server with the matrix config.
async fn matrix_server() -> (TestServer, MockServer) {
    let upstream = upstream_for_all_protocols().await;
    let cfg = config_for_matrix(&upstream.uri());
    let assembled = bitrouter::build_app(&cfg).await.expect("app assembles");
    let state = AppState {
        language_model: assembled.app.language_model().unwrap().clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
    };
    (TestServer::new(build_router(state)), upstream)
}

// ----- inbound request builders (one per inbound protocol) -----

fn inbound_chat_completions(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": "weather?" }],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "weather",
                "strict": true,
                "schema": matrix_schema(),
            },
        },
    })
}

fn inbound_anthropic(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 256,
        "messages": [{ "role": "user", "content": "weather?" }],
        "output_config": {
            "format": { "type": "json_schema", "schema": matrix_schema() },
        },
    })
}

fn inbound_responses(model: &str) -> Value {
    json!({
        "model": model,
        "input": "weather?",
        "text": {
            "format": {
                "type": "json_schema",
                "name": "weather",
                "strict": true,
                "schema": matrix_schema(),
            },
        },
    })
}

fn inbound_google() -> Value {
    // Generate Content carries the model in the URL, not the body.
    json!({
        "contents": [{ "role": "user", "parts": [{ "text": "weather?" }] }],
        "generationConfig": {
            "responseMimeType": "application/json",
            "responseSchema": matrix_schema(),
        },
    })
}

#[derive(Clone, Copy)]
enum Inbound {
    ChatCompletions,
    Messages,
    Responses,
    GenerateContent,
}

#[derive(Clone, Copy)]
enum Outbound {
    ChatCompletions,
    Messages,
    Responses,
    GenerateContent,
}

impl Outbound {
    fn model(self) -> &'static str {
        match self {
            Outbound::ChatCompletions => MODEL_VIA_OPENAI,
            Outbound::Messages => MODEL_VIA_ANTHROPIC,
            Outbound::Responses => MODEL_VIA_RESPONSES,
            Outbound::GenerateContent => MODEL_VIA_GOOGLE,
        }
    }

    fn path_segment(self) -> &'static str {
        match self {
            Outbound::ChatCompletions => "/chat/completions",
            Outbound::Messages => "/messages",
            Outbound::Responses => "/responses",
            // Generate Content's path contains the model id; matched via prefix below.
            Outbound::GenerateContent => "/models/",
        }
    }
}

/// POST `body` to the inbound route matching `inbound`.
async fn post_inbound(server: &TestServer, inbound: Inbound, model: &str, body: &Value) {
    let response = match inbound {
        Inbound::ChatCompletions => server.post("/v1/chat/completions").json(body).await,
        Inbound::Messages => server.post("/v1/messages").json(body).await,
        Inbound::Responses => server.post("/v1/responses").json(body).await,
        Inbound::GenerateContent => {
            server
                .post(&format!("/v1beta/models/{model}:generateContent"))
                .json(body)
                .await
        }
    };
    response.assert_status_ok();
}

/// Pull the single upstream request that landed at the path expected for the
/// outbound protocol. Panics with a useful message if zero or more than one
/// matched — a wrong-path call would otherwise look like a successful test.
async fn captured_outbound(upstream: &MockServer, outbound: Outbound) -> Value {
    let received = upstream.received_requests().await.unwrap_or_default();
    let suffix = match outbound {
        Outbound::GenerateContent => ":generateContent",
        _ => "",
    };
    let matches: Vec<_> = received
        .iter()
        .filter(|r| {
            let p = r.url.path();
            p.starts_with(outbound.path_segment()) && p.ends_with(suffix)
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one upstream request to outbound {:?} (path starts with {:?}); saw {} \
         total requests, paths: {:?}",
        outbound.path_segment(),
        outbound.path_segment(),
        received.len(),
        received.iter().map(|r| r.url.path()).collect::<Vec<_>>(),
    );
    serde_json::from_slice(&matches[0].body).expect("upstream body is JSON")
}

/// Assert the upstream body carries the schema at the outbound protocol's
/// native location. Encodes the per-protocol contract from PR #472.
fn assert_native_schema(outbound: Outbound, body: &Value) {
    let want = matrix_schema();
    match outbound {
        Outbound::ChatCompletions => {
            assert_eq!(
                body["response_format"]["type"], "json_schema",
                "openai chat outbound must set response_format.type=json_schema; body: {body}",
            );
            assert_eq!(
                body["response_format"]["json_schema"]["schema"], want,
                "openai chat outbound must carry the schema under \
                 response_format.json_schema.schema; body: {body}",
            );
            // Chat Completions requires `name`; the renderer must supply a default
            // when the inbound (Anthropic, Google) didn't carry one. Either
            // the caller-supplied name or the default `"response"` is fine.
            assert!(
                body["response_format"]["json_schema"]["name"].is_string(),
                "openai chat outbound must always set a name; body: {body}",
            );
        }
        Outbound::Messages => {
            assert_eq!(
                body["output_config"]["format"]["type"], "json_schema",
                "anthropic outbound must set output_config.format.type=json_schema; body: {body}",
            );
            assert_eq!(
                body["output_config"]["format"]["schema"], want,
                "anthropic outbound must carry the schema under \
                 output_config.format.schema; body: {body}",
            );
            // Messages' GA shape doesn't carry name/strict — the renderer
            // must drop them, not forward them as unknown fields.
            assert!(
                body["output_config"]["format"].get("name").is_none(),
                "anthropic outbound must NOT carry `name` (not in GA shape); body: {body}",
            );
            assert!(
                body["output_config"]["format"].get("strict").is_none(),
                "anthropic outbound must NOT carry `strict` (not in GA shape); body: {body}",
            );
            // The deprecated flat alias must never appear on the outbound.
            assert!(
                body.get("output_format").is_none(),
                "anthropic outbound must not emit the deprecated `output_format` alias; \
                 body: {body}",
            );
        }
        Outbound::Responses => {
            assert_eq!(
                body["text"]["format"]["type"], "json_schema",
                "responses outbound must set text.format.type=json_schema; body: {body}",
            );
            assert_eq!(
                body["text"]["format"]["schema"], want,
                "responses outbound must carry the schema under text.format.schema; body: {body}",
            );
            assert!(
                body["text"]["format"]["name"].is_string(),
                "responses outbound must always set a name; body: {body}",
            );
        }
        Outbound::GenerateContent => {
            assert_eq!(
                body["generationConfig"]["responseMimeType"], "application/json",
                "google outbound must set generationConfig.responseMimeType=application/json; \
                 body: {body}",
            );
            assert_eq!(
                body["generationConfig"]["responseSchema"], want,
                "google outbound must carry the schema under \
                 generationConfig.responseSchema; body: {body}",
            );
        }
    }
}

/// Drive one matrix cell end-to-end.
async fn run_cell(inbound: Inbound, outbound: Outbound) {
    let (server, upstream) = matrix_server().await;
    let model = outbound.model();
    let body = match inbound {
        Inbound::ChatCompletions => inbound_chat_completions(model),
        Inbound::Messages => inbound_anthropic(model),
        Inbound::Responses => inbound_responses(model),
        Inbound::GenerateContent => inbound_google(),
    };
    post_inbound(&server, inbound, model, &body).await;
    let upstream_body = captured_outbound(&upstream, outbound).await;
    assert_native_schema(outbound, &upstream_body);
}

// ----- 4×4 matrix -----

#[tokio::test]
async fn e2e_response_format_chat_completions_in_to_chat_completions_out() {
    run_cell(Inbound::ChatCompletions, Outbound::ChatCompletions).await;
}

#[tokio::test]
async fn e2e_response_format_chat_completions_in_to_messages_out() {
    run_cell(Inbound::ChatCompletions, Outbound::Messages).await;
}

#[tokio::test]
async fn e2e_response_format_chat_completions_in_to_responses_out() {
    run_cell(Inbound::ChatCompletions, Outbound::Responses).await;
}

#[tokio::test]
async fn e2e_response_format_chat_completions_in_to_generate_content_out() {
    run_cell(Inbound::ChatCompletions, Outbound::GenerateContent).await;
}

#[tokio::test]
async fn e2e_response_format_messages_in_to_chat_completions_out() {
    run_cell(Inbound::Messages, Outbound::ChatCompletions).await;
}

#[tokio::test]
async fn e2e_response_format_messages_in_to_messages_out() {
    run_cell(Inbound::Messages, Outbound::Messages).await;
}

#[tokio::test]
async fn e2e_response_format_messages_in_to_responses_out() {
    run_cell(Inbound::Messages, Outbound::Responses).await;
}

#[tokio::test]
async fn e2e_response_format_messages_in_to_generate_content_out() {
    run_cell(Inbound::Messages, Outbound::GenerateContent).await;
}

#[tokio::test]
async fn e2e_response_format_responses_in_to_chat_completions_out() {
    run_cell(Inbound::Responses, Outbound::ChatCompletions).await;
}

#[tokio::test]
async fn e2e_response_format_responses_in_to_messages_out() {
    run_cell(Inbound::Responses, Outbound::Messages).await;
}

#[tokio::test]
async fn e2e_response_format_responses_in_to_responses_out() {
    run_cell(Inbound::Responses, Outbound::Responses).await;
}

#[tokio::test]
async fn e2e_response_format_responses_in_to_generate_content_out() {
    run_cell(Inbound::Responses, Outbound::GenerateContent).await;
}

#[tokio::test]
async fn e2e_response_format_generate_content_in_to_chat_completions_out() {
    run_cell(Inbound::GenerateContent, Outbound::ChatCompletions).await;
}

#[tokio::test]
async fn e2e_response_format_generate_content_in_to_messages_out() {
    run_cell(Inbound::GenerateContent, Outbound::Messages).await;
}

#[tokio::test]
async fn e2e_response_format_generate_content_in_to_responses_out() {
    run_cell(Inbound::GenerateContent, Outbound::Responses).await;
}

#[tokio::test]
async fn e2e_response_format_generate_content_in_to_generate_content_out() {
    run_cell(Inbound::GenerateContent, Outbound::GenerateContent).await;
}
