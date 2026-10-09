//! Opt-in credentialed classifier gateway and SQLite settlement conformance.

use std::path::Path;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use bitrouter_ai::protocol::classifier::codec_for;
use bitrouter_ai::protocol::decisions::DecisionsCodec;
use bitrouter_ai::types::ApiProtocol;
use bitrouter_sdk::model_call::builder::PipelineBuilder;
use bitrouter_sdk::model_call::executor::HttpExecutor;
use bitrouter_sdk::model_call::operations::OperationScope;
use bitrouter_sdk::model_call::routing::StaticRoutingTable;
use bitrouter_sdk::model_call::types::RoutingTarget;
use bitrouter_sdk::server::{AppState, build_router};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::pricing::{ModelPricing, PricingTable};
use super::recorder::MeteringRecorder;
use super::store::{MeteringStore, TimeWindow};

struct LiveTarget {
    provider: &'static str,
    base: &'static str,
    file_var: &'static str,
    model: String,
    input_price: f64,
    outbound: ApiProtocol,
}

async fn smoke(outbound: ApiProtocol, inbound: ApiProtocol, case: &str) -> anyhow::Result<()> {
    let (provider, base, file_var, model_var, default_model, input_price) = match outbound {
        ApiProtocol::SystemOne => (
            "typesafe",
            "https://api.typesafe.ai/v1",
            "TYPESAFE_API_KEY_FILE",
            "TYPESAFE_CLASSIFIER_MODEL",
            "jev-1.13.0",
            0.042,
        ),
        ApiProtocol::Decisions => (
            "openai",
            "https://api.openai.com/v1",
            "OPENAI_API_KEY_FILE",
            "OPENAI_CLASSIFIER_MODEL",
            "gpt-6-luna",
            0.10,
        ),
        _ => anyhow::bail!("unsupported live classifier protocol"),
    };
    let model = std::env::var(model_var).unwrap_or_else(|_| default_model.into());
    smoke_target(
        LiveTarget {
            provider,
            base,
            file_var,
            model,
            input_price,
            outbound,
        },
        inbound,
        case,
    )
    .await
}

async fn smoke_target(target: LiveTarget, inbound: ApiProtocol, case: &str) -> anyhow::Result<()> {
    let LiveTarget {
        provider,
        base,
        file_var,
        model,
        input_price,
        outbound,
    } = target;
    let key_path = std::env::var(file_var)
        .map_err(|_| anyhow::anyhow!("{file_var} must name a local credential file"))?;
    let key = std::fs::read_to_string(key_path)
        .map_err(|_| anyhow::anyhow!("cannot read classifier credential file"))?;
    let key = key.trim().to_owned();
    anyhow::ensure!(!key.is_empty(), "classifier credential file is empty");
    let target = RoutingTarget {
        provider_name: provider.into(),
        service_id: model.clone(),
        api_base: base.into(),
        api_key: key.clone(),
        api_protocol: outbound.clone(),
        chat_token_limit_field: None,
        chat_supports_store: None,
        chat_supports_stream_options: None,
        chat_google_extensions: false,
        reasoning_effort: None,
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: Default::default(),
        headers: Vec::new(),
    };
    let routes = Arc::new(StaticRoutingTable::new());
    routes.insert("live-classifier", vec![target]);
    let mut pricing = PricingTable::new();
    pricing.configure_endpoint(provider, Some(outbound.clone()), base);
    pricing.insert_for_protocol(
        provider,
        &model,
        outbound.clone(),
        ModelPricing::cache_aware(Some(input_price), Some(0.0), Some(0.0), Some(0.0)),
    );
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = MeteringStore::new(db);
    let recorder = MeteringRecorder::new(store.clone(), Arc::new(pricing));
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(routes)
        .executor(Arc::new(HttpExecutor::with_defaults()?))
        .served_operations(OperationScope::Both)
        .route_hook_for(recorder.tariff_capture(false), OperationScope::Both)
        .settlement_recorder_for(recorder, OperationScope::Both);
    let pipeline = Arc::new(builder.build()?);
    let router = build_router(AppState {
        model_call: pipeline.clone(),
        mcp: None,
        skip_auth: true,
        metrics_renderer: None,
        prompt_transforms: Vec::new(),
    });
    let canonical = DecisionsCodec::parse_request(json!({
        "model":"live-classifier", "input":"The build completed and all tests passed.",
        "questions":[
            {"type":"predicate","name":"passed/key","instructions":"Did all tests pass?"},
            {"type":"choice","name":"status","instructions":"Choose the build status.",
                "choices":[{"value":"passed","description":"All tests passed"},{"value":"failed","description":"A test failed"}]},
            {"type":"score","name":"quality","instructions":"Rate the build result.","levels":[{"label":"failed"},{"label":"passed"}]}
        ]
    }))?;
    let codec = codec_for(&inbound).ok_or_else(|| anyhow::anyhow!("missing classifier codec"))?;
    let body = codec.render_request(&canonical)?;
    let request = codec.parse_request(body.clone())?;
    let endpoint = format!("/v1/{}", inbound.as_str());
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(endpoint)
                .header("content-type", "application/json")
                .header("x-bitrouter-request-id", case)
                .body(Body::from(serde_json::to_vec(&body)?))?,
        )
        .await?;
    let status = response.status();
    let response_bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
    pipeline.drain_required_pending_settlements().await?;
    // Do not include upstream diagnostic bodies in a failing credentialed test.
    anyhow::ensure!(
        status == StatusCode::OK,
        "live classifier returned HTTP {status}"
    );
    let response_body: Value = serde_json::from_slice(&response_bytes)?;
    let decoded = codec.parse_response(response_body.clone(), &request)?;
    anyhow::ensure!(decoded.answers.len() == 3, "live answer correlation failed");
    let rows = store.export_usage(TimeWindow::ThisMonth).await?;
    anyhow::ensure!(rows.len() == 1, "live request did not settle exactly once");
    let row = rows
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing live settlement"))?;
    anyhow::ensure!(
        row.request_id.as_deref() == Some(case),
        "request identity mismatch"
    );
    anyhow::ensure!(
        row.provider_id == provider && row.model_id == model,
        "selected target mismatch"
    );
    anyhow::ensure!(
        row.status.as_deref() == Some("completed"),
        "live settlement did not complete"
    );
    anyhow::ensure!(
        row.prompt_tokens == decoded.usage.prompt_tokens
            && row.completion_tokens == decoded.usage.completion_tokens,
        "live usage did not match settlement"
    );
    let native_usage = row
        .raw_usage
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing native settlement usage"))?;
    anyhow::ensure!(
        native_usage.get("input_tokens").and_then(Value::as_u64) == Some(row.prompt_tokens)
            && native_usage.get("output_tokens").and_then(Value::as_u64)
                == Some(row.completion_tokens),
        "native usage counters did not survive settlement"
    );
    let tariff = row
        .charge_evidence
        .as_ref()
        .and_then(|evidence| evidence.tariff_snapshot.as_ref())
        .ok_or_else(|| anyhow::anyhow!("missing frozen live tariff"))?;
    anyhow::ensure!(tariff.protocol == outbound, "frozen wire mismatch");
    if outbound == ApiProtocol::SystemOne {
        anyhow::ensure!(
            row.final_charge_micro_usd
                == Some((decoded.usage.prompt_tokens as f64 * input_price).round() as u64),
            "input-only charge did not match the selected tariff"
        );
        let native_usage = response_body
            .get("usage")
            .ok_or_else(|| anyhow::anyhow!("missing caller usage"))?;
        anyhow::ensure!(
            row.raw_usage.as_ref() == Some(native_usage),
            "native usage did not survive settlement"
        );
        if let Some(cost) = native_usage.get("cost").and_then(Value::as_f64) {
            let expected_cost = decoded.usage.prompt_tokens as f64 * input_price / 1_000_000.0;
            anyhow::ensure!(
                (cost - expected_cost).abs() < 1e-10,
                "reported upstream cost did not match the selected tariff"
            );
        }
    }
    let evidence = json!({"case":case,"recorded_at":chrono::Utc::now(),"selected_model":model,
        "actual_reported_model":decoded.model,"inbound_protocol":inbound,"outbound_protocol":outbound,
        "response":response_body,"settlement":row,"scope":"local credentialed gateway; no invoice or production proof"});
    let encoded = serde_json::to_string_pretty(&evidence)?;
    anyhow::ensure!(!encoded.contains(&key), "credential leaked into evidence");
    if let Ok(directory) = std::env::var("BITROUTER_CLASSIFIER_SMOKE_EVIDENCE_DIR") {
        std::fs::create_dir_all(&directory)?;
        std::fs::write(Path::new(&directory).join(format!("{case}.json")), encoded)?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires TYPESAFE_API_KEY_FILE; performs a paid provider call"]
async fn live_systemone_native_gateway_and_settlement() -> anyhow::Result<()> {
    smoke(
        ApiProtocol::SystemOne,
        ApiProtocol::SystemOne,
        "classifier-live-systemone",
    )
    .await
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY_FILE; performs a paid provider call"]
async fn live_decisions_native_gateway_and_settlement() -> anyhow::Result<()> {
    smoke(
        ApiProtocol::Decisions,
        ApiProtocol::Decisions,
        "classifier-live-decisions",
    )
    .await
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY_FILE; performs a paid provider call"]
async fn live_systemone_caller_decisions_upstream_and_settlement() -> anyhow::Result<()> {
    smoke(
        ApiProtocol::Decisions,
        ApiProtocol::SystemOne,
        "classifier-live-conversion",
    )
    .await
}

async fn openrouter_smoke(model: &str, input_price: f64, case: &str) -> anyhow::Result<()> {
    smoke_target(
        LiveTarget {
            provider: "openrouter",
            base: "https://openrouter.ai/api/v1",
            file_var: "OPENROUTER_API_KEY_FILE",
            model: model.into(),
            input_price,
            outbound: ApiProtocol::SystemOne,
        },
        ApiProtocol::SystemOne,
        case,
    )
    .await
}

#[tokio::test]
#[ignore = "requires OPENROUTER_API_KEY_FILE; performs a paid provider call"]
async fn live_openrouter_jev_systemone_gateway_and_settlement() -> anyhow::Result<()> {
    openrouter_smoke("typesafe/jev-1.13", 0.042, "classifier-live-openrouter-jev").await
}

#[tokio::test]
#[ignore = "requires OPENROUTER_API_KEY_FILE; performs a paid provider call"]
async fn live_openrouter_openai_systemone_gateway_and_settlement() -> anyhow::Result<()> {
    openrouter_smoke(
        "openai/gpt-6-luna-decisions",
        0.10,
        "classifier-live-openrouter-openai",
    )
    .await
}
