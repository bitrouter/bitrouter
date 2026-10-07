//! Native gateway accounting against a controlled HTTP upstream and real SQLite.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use bitrouter_ai::types::{ApiProtocol, Usage, UsageOrigin};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::builder::PipelineBuilder;
use bitrouter_sdk::language_model::context::PipelineContext;
use bitrouter_sdk::language_model::executor::HttpExecutor;
use bitrouter_sdk::language_model::hooks::RouteHook;
use bitrouter_sdk::language_model::operations::OperationScope;
use bitrouter_sdk::language_model::routing::StaticRoutingTable;
use bitrouter_sdk::language_model::types::RoutingTarget;
use bitrouter_sdk::server::{AppState, build_router};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::pricing::{ContextTier, ModelPricing, PricingSource, PricingTable};
use super::recorder::MeteringRecorder;
use super::store::{MeteringStore, TimeWindow};
use super::tariff::endpoint_profile;

fn target() -> RoutingTarget {
    RoutingTarget {
        provider_name: "fixture".into(),
        service_id: "native-test".into(),
        api_protocol: ApiProtocol::Decisions,
        api_base: "https://api.openai.com/v1".into(),
        api_key: "fixture-key".into(),
        api_key_override: None,
        api_base_override: None,
        chat_token_limit_field: None,
        chat_supports_store: None,
        chat_supports_stream_options: None,
        reasoning_effort: None,
        account_label: None,
        auth_scheme: Default::default(),
        headers: Vec::new(),
    }
}

fn native_prices(base: &str) -> PricingTable {
    let mut prices = PricingTable::new();
    prices.configure_endpoint("fixture", None, base);
    prices.insert("fixture", "native-test", ModelPricing::new(2.0, 10.0));
    let mut native = ModelPricing::cache_aware(Some(0.1), Some(0.0), Some(0.0), Some(0.0));
    native.context_tiers.push(ContextTier {
        above_input_tokens: 272_000,
        input_micro_usd_per_token: Some(0.2),
        cache_read_micro_usd_per_token: None,
        cache_write_micro_usd_per_token: None,
        output_micro_usd_per_token: None,
    });
    prices.insert_for_protocol("fixture", "native-test", ApiProtocol::Decisions, native);
    prices
}

struct OverrideEndpoint(String);
#[async_trait]
impl RouteHook for OverrideEndpoint {
    async fn resolve(
        &self,
        chain: &mut Vec<RoutingTarget>,
        _ctx: &mut PipelineContext,
    ) -> bitrouter_sdk::Result<()> {
        for target in chain {
            target.api_base_override = Some(self.0.clone());
        }
        Ok(())
    }
}

#[tokio::test]
async fn native_gateway_freezes_final_route_and_persists_completed_usage() -> anyhow::Result<()> {
    for (answer, cached, expected_status, expected_cost) in [
        (
            json!({"type":"predicate","name":"q","probability":0.7}),
            0,
            StatusCode::OK,
            Some(10),
        ),
        (
            json!({"type":"predicate","name":"q","probability":2.0}),
            0,
            StatusCode::BAD_GATEWAY,
            Some(10),
        ),
        (
            json!({"type":"refusal","name":"q","refusal":"cannot assess"}),
            0,
            StatusCode::OK,
            Some(10),
        ),
        (
            json!({"type":"predicate","name":"q","probability":0.7}),
            40,
            StatusCode::OK,
            None,
        ),
    ] {
        let upstream = MockServer::start().await;
        let base = format!("{}/v1", upstream.uri());
        Mock::given(method("POST"))
            .and(path("/v1/decisions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model":"native-test", "answers":[answer],
                "usage":{"input_tokens":100,"output_tokens":0,"total_tokens":100,
                    "input_tokens_details":{"cached_tokens":cached,"cache_write_tokens":0},
                    "output_tokens_details":{"reasoning_tokens":0}}
            })))
            .mount(&upstream)
            .await;
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = MeteringStore::new(db.clone());
        let recorder = MeteringRecorder::new(store.clone(), Arc::new(native_prices(&base)));
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("test", vec![target()]);
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .served_operations(OperationScope::Both)
            // Register capture first: even a later mutator must finish before capture.
            .route_hook_for(recorder.tariff_capture(false), OperationScope::Both)
            .route_hook_for(OverrideEndpoint(base), OperationScope::Both)
            .settlement_recorder_for(recorder, OperationScope::Both);
        let pipeline = Arc::new(builder.build()?);
        let router = build_router(AppState {
            language_model: pipeline.clone(),
            mcp: None,
            skip_auth: true,
            metrics_renderer: None,
            prompt_transforms: Vec::new(),
        });
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/decisions")
                    .header("content-type", "application/json")
                    .header("x-bitrouter-request-id", "native-accounting")
                    .body(Body::from(serde_json::to_vec(
                        &json!({"model":"test","input":"evidence",
                "questions":[{"type":"predicate","name":"q","instructions":"check"}]}),
                    )?))?,
            )
            .await?;
        assert_eq!(response.status(), expected_status);
        let _body = to_bytes(response.into_body(), 1024 * 1024).await?;
        pipeline.drain_required_pending_settlements().await?;
        assert!(
            upstream
                .received_requests()
                .await
                .is_some_and(|requests| requests.len() == 1)
        );
        let rows = store.export_usage(TimeWindow::ThisMonth).await?;
        assert_eq!(rows.len(), 1);
        let row = rows
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing settled request"))?;
        assert_eq!(row.final_charge_micro_usd, expected_cost);
        assert_eq!(row.cache_read_tokens, cached);
        let evidence = row
            .charge_evidence
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing evidence"))?;
        let tariff = evidence
            .tariff_snapshot
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing frozen tariff"))?;
        assert_eq!(tariff.protocol, ApiProtocol::Decisions);
        assert!(tariff.endpoint_profile.starts_with("configured:sha256:"));
        assert_eq!(
            tariff
                .pricing
                .as_ref()
                .and_then(|p| p.input_micro_usd_per_token),
            Some(0.1)
        );
        assert_eq!(tariff.pricing_version, evidence.pricing_version);
        assert!(!serde_json::to_string(evidence)?.contains("fixture-key"));
        if cached != 0 {
            assert_eq!(
                evidence.unknown_reason.as_deref(),
                Some("decisions_cache_billing_unverified")
            );
            assert!(row.raw_usage.is_some());
        }
        if cached != 0 {
            let original = tariff.clone();
            let mut exported = rows.clone();
            let override_price = super::store::UsagePriceOverride {
                provider_id: "fixture".into(),
                model_id: "native-test".into(),
                input_micro_usd_per_token: 0.1,
                output_micro_usd_per_token: 0.0,
                cache_read_micro_usd_per_token: Some(0.0),
                cache_write_micro_usd_per_token: Some(0.0),
            };
            super::store::MeteringUsageRecord::apply_price_overrides(
                &mut exported,
                &[override_price],
            );
            let corrected = exported
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing export"))?;
            assert!(corrected.final_charge_micro_usd.is_none());
            assert_eq!(
                corrected
                    .charge_evidence
                    .as_ref()
                    .and_then(|e| e.tariff_snapshot.as_ref()),
                Some(&original)
            );
            // A typed authoritative correction proves only this request's amount.
            // Exercise the real persisted row while retaining its admission tariff.
            use sea_orm::ConnectionTrait;
            let request_id = row
                .request_id
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing request identity"))?;
            db.execute(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Sqlite,
                "UPDATE requests SET reconciliation_status = 'pending' WHERE request_id = ?",
                [request_id.clone().into()],
            ))
            .await?;
            let receipt = crate::cloud::settlement::SettlementReceipt {
                request_id: request_id.clone(),
                state: crate::cloud::settlement::SettlementState::Computed,
                provider_id: Some("fixture".into()),
                model_id: Some("native-test".into()),
                usage: crate::cloud::settlement::SettlementUsage {
                    uncached_input_tokens: 60,
                    cache_read_tokens: 40,
                    cache_write_tokens: 0,
                    output_tokens: 0,
                    reasoning_tokens: 0,
                },
                final_charge_micro_usd: Some(17),
            };
            assert_eq!(
                store.apply_authoritative_receipt_charge(&receipt).await?,
                super::db::ReconciliationStatus::Computed
            );
            let reconciled = store.export_usage(TimeWindow::ThisMonth).await?;
            let record = reconciled
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing corrected row"))?;
            assert_eq!(record.final_charge_micro_usd, Some(17));
            let evidence = record
                .charge_evidence
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing corrected evidence"))?;
            assert_eq!(evidence.pricing_source, PricingSource::AuthoritativeReceipt);
            assert_eq!(evidence.tariff_snapshot.as_ref(), Some(&original));
        }
    }
    Ok(())
}

#[test]
fn frozen_tariffs_keep_rates_and_endpoint_billing_conditions() -> anyhow::Result<()> {
    let target = target();
    let mut prices = native_prices(&target.api_base);
    let snapshot = prices.snapshot(&target);
    let usage = Usage {
        prompt_tokens: 272_000,
        origin: UsageOrigin::ProviderReported,
        ..Default::default()
    };
    assert_eq!(
        snapshot
            .tariff
            .charge_evidence(&usage, PricingSource::Configured)
            .charge_micro_usd,
        Some(27_200)
    );
    let above = Usage {
        prompt_tokens: 272_001,
        ..usage.clone()
    };
    assert_eq!(
        snapshot
            .tariff
            .charge_evidence(&above, PricingSource::Configured)
            .charge_micro_usd,
        Some(54_400)
    );
    assert!(!snapshot.tariff.guarantees_known_price());
    prices.insert_for_protocol(
        "fixture",
        "native-test",
        ApiProtocol::Decisions,
        ModelPricing::new(5.0, 5.0),
    );
    assert_eq!(
        snapshot
            .tariff
            .charge_evidence(&usage, PricingSource::Configured)
            .charge_micro_usd,
        Some(27_200)
    );
    assert_ne!(
        snapshot.tariff.pricing_version,
        prices.snapshot(&target).tariff.pricing_version
    );
    let mut regional = target.clone();
    regional.api_base_override = Some("https://eu.api.openai.com/v1".into());
    let mismatch = prices.snapshot(&regional);
    assert_eq!(
        mismatch.tariff.unavailable_reason.as_deref(),
        Some("endpoint_profile_mismatch")
    );
    assert_eq!(
        mismatch
            .tariff
            .charge_evidence(&usage, PricingSource::Configured)
            .charge_micro_usd,
        None
    );
    prices.configure_endpoint(
        "fixture",
        Some(ApiProtocol::Decisions),
        "https://eu.api.openai.com/v1",
    );
    assert!(
        prices
            .snapshot(&regional)
            .tariff
            .unavailable_reason
            .is_none()
    );
    assert_eq!(
        endpoint_profile("https://user:secret@eu.api.openai.com/v1"),
        "openai_europe"
    );
    assert!(!endpoint_profile("https://unknown.invalid/v1?api_key=secret").contains("secret"));
    let legacy: super::pricing::ChargeEvidence = serde_json::from_value(json!({
        "status":"unknown", "charge_micro_usd":null, "normalized_usage":{"uncached_input_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"output_tokens":0,"reasoning_tokens":0}, "effective_rates":{},
        "pricing_source":"unknown", "pricing_version":"legacy", "unknown_reason":"usage_unavailable"
    }))?;
    assert!(legacy.tariff_snapshot.is_none());
    Ok(())
}

#[tokio::test]
async fn known_price_admission_rejects_native_cache_uncertainty_before_http() -> anyhow::Result<()>
{
    let upstream = MockServer::start().await;
    let base = format!("{}/v1", upstream.uri());
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let recorder = MeteringRecorder::new(MeteringStore::new(db), Arc::new(native_prices(&base)));
    let routes = Arc::new(StaticRoutingTable::new());
    let mut native = target();
    native.api_base = base;
    routes.insert("test", vec![native]);
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(routes)
        .executor(Arc::new(HttpExecutor::with_defaults()?))
        .served_operations(OperationScope::Both)
        .route_hook_for(recorder.tariff_capture(true), OperationScope::Both);
    let pipeline = Arc::new(builder.build()?);
    let request = bitrouter_ai::protocol::decisions::DecisionsCodec::parse_request(json!({
        "model":"test","input":"evidence","questions":[{"type":"predicate","name":"q","instructions":"check"}]
    }))?;
    let outcome = pipeline
        .execute(
            bitrouter_sdk::language_model::types::PipelineRequest::new_decisions(
                "test",
                CallerContext::local(),
                request,
            ),
        )
        .await;
    assert!(matches!(
        outcome,
        Err(bitrouter_sdk::BitrouterError::BadRequest { .. })
    ));
    assert!(
        upstream
            .received_requests()
            .await
            .is_some_and(|requests| requests.is_empty())
    );
    pipeline.drain_required_pending_settlements().await?;
    Ok(())
}
