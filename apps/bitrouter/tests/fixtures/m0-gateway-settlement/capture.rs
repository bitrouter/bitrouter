//! Run as an example in the frozen M0 checkout. The application, local pricing,
//! request inventory, fallback, and authoritative receipt writers all come from M0.
use anyhow::{Context, Result, ensure};
use axum::{Json, Router, http::StatusCode, routing::post};
use bitrouter::harness::{
    acp::capture::CapturePort,
    trajectory::{Recorder, RecordingScope},
};
use bitrouter::storage::capture::{CaptureDirection, CaptureEvent, CaptureKind};
use bitrouter::{
    cloud::settlement::{SettlementReceipt, SettlementState, SettlementUsage},
    gateway::{caller::CallerContext, pipeline::types::PipelineRequest},
    metering::store::MeteringStore,
};
use bitrouter_ai::types::{Message, Prompt, Role};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde_json::json;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let dir =
        PathBuf::from(std::env::args_os().nth(1).context("output directory")?).canonicalize()?;
    let path = dir.join("source.sqlite");
    ensure!(!path.exists(), "output already exists");
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = server.local_addr()?;
    let upstream = Router::new().route("/first/chat/completions", post(|| async {
        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":{"message":"unavailable"}})))
    })).route("/last/chat/completions", post(|| async {
        Json(json!({"id":"reply","object":"chat.completion","model":"final-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}))
    }));
    let task = tokio::spawn(async move { axum::serve(server, upstream).await });
    let config = bitrouter::config::parse_with(
        &format!(
            r#"
inherit_defaults: false
server: {{skip_auth: true}}
registry: {{enabled: false}}
database: {{url: 'sqlite://{}?mode=rwc'}}
providers:
  first:
    api_base: 'http://{addr}/first'
    api_key: fixture
    api_protocol: [{{'*': chat_completions}}]
    models: [{{id: first-model}}]
  bitrouter:
    api_base: 'http://{addr}/last'
    api_key: fixture
    api_protocol: [{{'*': chat_completions}}]
    models: [{{id: final-model, pricing: {{input_micro_usd_per_token: 2.0, output_micro_usd_per_token: 5.0}}}}]
models:
  direct:
    endpoints: [{{provider: bitrouter, service_id: final-model}}]
  fallback:
    strategy: priority
    endpoints: [{{provider: first, service_id: first-model}}, {{provider: bitrouter, service_id: final-model}}]
"#,
            path.display()
        ),
        |_| None,
    )?;
    let app = bitrouter::build_app(&config).await?;
    let recorder = Recorder::new(
        app.db.clone(),
        RecordingScope {
            owner: "local".into(),
            source: "m0-gateway-capture".into(),
            controller_instance_id: Some("old-controller".into()),
            route_scope_id: Some("local".into()),
        },
    )
    .await?;
    app.gateway_requests
        .inventory()
        .register_capture(recorder.connection_id(), "local", "old-controller")
        .await?;
    let lifecycle = |kind, method: &str, payload| CaptureEvent {
        direction: CaptureDirection::Controller,
        kind,
        method: method.into(),
        call_id: None,
        payload,
    };
    recorder
        .record(lifecycle(
            CaptureKind::Connected,
            "controller/connect",
            json!({}),
        ))
        .await?;
    let pipeline = app.app.language_model().context("language model")?;
    let cases = [
        ("m0-single-paid", "direct", Some(47)),
        ("ordinary-terminal-unknown", "direct", None),
        ("m0-fallback-paid", "fallback", Some(61)),
    ];
    for (id, model, _) in cases {
        let prompt = Prompt {
            model: model.into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "hello")],
            tools: vec![],
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let mut req = PipelineRequest::new(model, CallerContext::local(), prompt);
        req.request_id = id.into();
        req.headers
            .insert("x-bitrouter-controller-id", "old-controller".parse()?);
        pipeline.execute(req).await?;
        pipeline.drain_required_pending_settlements().await?;
    }
    recorder
        .record(lifecycle(
            CaptureKind::Disconnected,
            "controller/disconnect",
            json!({"clean":true}),
        ))
        .await?;
    let snapshot = |name: &str| {
        Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "VACUUM INTO ?",
            [dir.join(name).to_string_lossy().into_owned().into()],
        )
    };
    app.db.execute(snapshot("dispatch.sqlite")).await?;
    let store = MeteringStore::new(app.db.clone());
    for (id, _, amount) in cases {
        store
            .apply_authoritative_receipt_charge(&SettlementReceipt {
                request_id: id.into(),
                state: if amount.is_some() {
                    SettlementState::Computed
                } else {
                    SettlementState::Unknown
                },
                model_id: Some("final-model".into()),
                provider_id: Some("bitrouter".into()),
                usage: SettlementUsage {
                    uncached_input_tokens: 10,
                    output_tokens: 2,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    reasoning_tokens: 0,
                },
                final_charge_micro_usd: amount,
            })
            .await?;
    }
    app.db.execute(snapshot("reconciled.sqlite")).await?;
    task.abort();
    Ok(())
}
