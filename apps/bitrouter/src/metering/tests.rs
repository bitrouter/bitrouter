//! Metering integration tests against an in-memory SQLite database.

use std::sync::Arc;
use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

use bitrouter_cloud_sdk::settlement::{
    SettlementClient, SettlementReceipt, SettlementState, SettlementUsage,
};
use bitrouter_sdk::Result;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{SettlementContext, SettlementRecorder};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{MeteringRecorder, MeteringStore, ModelPricing, PricingTable, TimeWindow};
use crate::db;

async fn pool() -> DatabaseConnection {
    let db = db::connect("sqlite::memory:").await.unwrap();
    db::run_migrations(&db).await.unwrap();
    db
}

fn ctx(api_key: &str, prompt: u64, completion: u64) -> SettlementContext {
    SettlementContext {
        request_id: format!("r-{api_key}-{prompt}-{completion}"),
        caller: CallerContext::new(api_key, format!("u-{api_key}")),
        target: None,
        model_id: "gpt-5".into(),
        provider_id: "openai".into(),
        account_label: None,
        prompt_tokens: prompt,
        completion_tokens: completion,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        usage_origin: bitrouter_sdk::language_model::UsageOrigin::ProviderReported,
        raw_usage: None,
        web_search_count: 0,
        media_input_count: 0,
        media_output_count: 0,
        server_tool_calls: Vec::new(),
        streamed: false,
        latency_ms: 100,
        generation_time_ms: 80,
        first_token_latency_ms: None,
        first_token_kind: None,
        error: None,
        events: bitrouter_sdk::EventBus::new(),
    }
}

fn pricing() -> Arc<PricingTable> {
    let mut t = PricingTable::new();
    // 2 µ$/prompt token, 10 µ$/completion token
    t.insert("openai", "gpt-5", ModelPricing::new(2.0, 10.0));
    Arc::new(t)
}

#[tokio::test]
async fn recorder_writes_estimated_charge_from_pricing() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone(), pricing());
    // 10 prompt × 2 + 5 completion × 10 = 70 µ$
    recorder.record(&mut ctx("k1", 10, 5)).await?;
    let spend = store.get_spend("k1", TimeWindow::ThisMonth).await?;
    assert_eq!(spend, 70);
    Ok(())
}

#[tokio::test]
async fn recorder_persists_cache_aware_charge_evidence() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let mut table = PricingTable::new();
    table.insert(
        "openai",
        "gpt-5",
        ModelPricing::cache_aware(Some(2.0), Some(0.2), Some(2.5), Some(10.0)),
    );
    let recorder = MeteringRecorder::new(store.clone(), Arc::new(table));
    let raw = serde_json::json!({
        "prompt_tokens": 100,
        "completion_tokens": 30,
        "prompt_tokens_details": { "cached_tokens": 40 },
        "cache_write_tokens": 20
    });
    let mut settlement = ctx("cache", 100, 30);
    settlement.reasoning_tokens = 10;
    settlement.cache_read_tokens = 40;
    settlement.cache_write_tokens = 20;
    settlement.raw_usage = Some(raw.clone());

    recorder.record(&mut settlement).await?;
    let records = store.export_usage(TimeWindow::ThisMonth).await?;
    let record = records.first().expect("one usage record");

    assert_eq!(record.uncached_input_tokens, 40);
    assert_eq!(record.cache_read_tokens, 40);
    assert_eq!(record.cache_write_tokens, 20);
    assert_eq!(record.output_tokens, 20);
    assert_eq!(record.reasoning_tokens, 10);
    assert_eq!(record.final_charge_micro_usd, Some(438));
    assert_eq!(record.charge_status, super::ChargeStatus::Computed);
    assert_eq!(record.raw_usage.as_ref(), Some(&raw));
    let evidence = record.charge_evidence.as_ref().expect("charge evidence");
    assert_eq!(evidence.charge_micro_usd, Some(438));
    assert!(evidence.pricing_version.starts_with("sha256:"));
    let cloud: crate::workflow_state::archive::CloudUsageRecord =
        serde_json::from_value(serde_json::to_value(record).unwrap()).unwrap();
    assert_eq!(cloud.cache_read_tokens, 40);
    assert_eq!(cloud.cache_write_tokens, 20);
    assert_eq!(cloud.charge_status, super::ChargeStatus::Computed);
    assert_eq!(cloud.charge_evidence.unwrap().charge_micro_usd, Some(438));
    Ok(())
}

#[tokio::test]
async fn recorder_marks_charge_unknown_when_pricing_is_missing() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let empty = Arc::new(PricingTable::new());
    let recorder = MeteringRecorder::new(store.clone(), empty);
    recorder.record(&mut ctx("k1", 10, 5)).await?;
    let spend = store.get_spend("k1", TimeWindow::ThisMonth).await?;
    assert_eq!(spend, 0);
    // The row was still written — count is 1.
    let count = store.get_request_count("k1", TimeWindow::ThisMonth).await?;
    assert_eq!(count, 1);
    let records = store.export_usage(TimeWindow::ThisMonth).await?;
    assert_eq!(records[0].final_charge_micro_usd, None);
    assert_eq!(records[0].charge_status, super::ChargeStatus::Unknown);
    assert_eq!(
        records[0]
            .charge_evidence
            .as_ref()
            .and_then(|evidence| evidence.unknown_reason.as_deref()),
        Some("pricing_not_found")
    );
    Ok(())
}

#[tokio::test]
async fn hosted_provider_rows_start_pending_authoritative_reconciliation() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder =
        MeteringRecorder::new(store.clone(), pricing()).with_reconciliation_provider("bitrouter");
    let mut settlement = ctx("hosted", 10, 5);
    settlement.provider_id = "bitrouter".to_string();

    recorder.record(&mut settlement).await?;
    let records = store.export_usage(TimeWindow::ThisMonth).await?;

    assert_eq!(
        records[0].reconciliation_status,
        super::ReconciliationStatus::Pending
    );
    Ok(())
}

#[tokio::test]
async fn computed_receipt_replaces_local_usage_only_when_charge_matches() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder =
        MeteringRecorder::new(store.clone(), pricing()).with_reconciliation_provider("bitrouter");
    let mut settlement = ctx("reconcile", 1, 1);
    settlement.provider_id = "bitrouter".to_string();
    recorder.record(&mut settlement).await?;
    let receipt = SettlementReceipt {
        request_id: settlement.request_id.clone(),
        state: SettlementState::Computed,
        model_id: Some("model-a".to_string()),
        provider_id: Some("provider-a".to_string()),
        usage: SettlementUsage {
            uncached_input_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 5,
            output_tokens: 7,
            reasoning_tokens: 11,
        },
        final_charge_micro_usd: Some(164),
    };
    let prices = [super::UsagePriceOverride::parse(
        "provider-a:model-a=2,3,5,7",
    )?];

    let status = store.apply_authoritative_receipt(&receipt, &prices).await?;
    let records = store.export_usage(TimeWindow::ThisMonth).await?;
    let record = &records[0];

    assert_eq!(status, super::ReconciliationStatus::Computed);
    assert_eq!(record.provider_id, "provider-a");
    assert_eq!(record.model_id, "model-a");
    assert_eq!(record.prompt_tokens, 10);
    assert_eq!(record.completion_tokens, 18);
    assert_eq!(record.final_charge_micro_usd, Some(164));
    assert_eq!(record.charge_status, super::ChargeStatus::Computed);
    assert_eq!(
        record.usage_origin,
        bitrouter_sdk::language_model::UsageOrigin::AuthoritativeReceipt
    );
    assert!(record.authoritative_receipt.is_some());
    Ok(())
}

#[tokio::test]
async fn authoritative_charge_mismatch_fails_closed() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder =
        MeteringRecorder::new(store.clone(), pricing()).with_reconciliation_provider("bitrouter");
    let mut settlement = ctx("mismatch", 1, 1);
    settlement.provider_id = "bitrouter".to_string();
    recorder.record(&mut settlement).await?;
    let receipt = SettlementReceipt {
        request_id: settlement.request_id.clone(),
        state: SettlementState::Computed,
        model_id: Some("model-a".to_string()),
        provider_id: Some("provider-a".to_string()),
        usage: SettlementUsage {
            uncached_input_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 1,
            reasoning_tokens: 0,
        },
        final_charge_micro_usd: Some(999),
    };
    let prices = [super::UsagePriceOverride::parse(
        "provider-a:model-a=2,0,0,10",
    )?];

    let status = store.apply_authoritative_receipt(&receipt, &prices).await?;
    let records = store.export_usage(TimeWindow::ThisMonth).await?;

    assert_eq!(status, super::ReconciliationStatus::Unknown);
    assert_eq!(records[0].reconciliation_status, status);
    assert_eq!(records[0].charge_status, super::ChargeStatus::Unknown);
    assert_eq!(records[0].final_charge_micro_usd, None);
    Ok(())
}

#[tokio::test]
async fn not_charged_receipt_is_terminal_but_never_a_computed_zero() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder =
        MeteringRecorder::new(store.clone(), pricing()).with_reconciliation_provider("bitrouter");
    let mut settlement = ctx("not-charged", 0, 0);
    settlement.provider_id = "bitrouter".to_string();
    recorder.record(&mut settlement).await?;
    let receipt = SettlementReceipt {
        request_id: settlement.request_id.clone(),
        state: SettlementState::NotCharged,
        model_id: None,
        provider_id: None,
        usage: SettlementUsage {
            uncached_input_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
        },
        final_charge_micro_usd: None,
    };

    let status = store.apply_authoritative_receipt(&receipt, &[]).await?;
    let records = store.export_usage(TimeWindow::ThisMonth).await?;

    assert_eq!(status, super::ReconciliationStatus::NotCharged);
    assert_eq!(records[0].charge_status, super::ChargeStatus::NotCharged);
    assert_eq!(records[0].final_charge_micro_usd, None);
    Ok(())
}

#[tokio::test]
async fn bounded_reconciler_fetches_only_selected_request_ids() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder =
        MeteringRecorder::new(store.clone(), pricing()).with_reconciliation_provider("bitrouter");
    let mut settlement = ctx("poll", 0, 0);
    settlement.provider_id = "bitrouter".to_string();
    recorder.record(&mut settlement).await?;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/requests/{}/settlement",
            settlement.request_id
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "request_id": settlement.request_id,
            "state": "computed",
            "model_id": "model-a",
            "provider_id": "provider-a",
            "usage": {
                "uncached_input_tokens": 1,
                "cache_read_tokens": 0,
                "cache_write_tokens": 0,
                "output_tokens": 1,
                "reasoning_tokens": 0
            },
            "final_charge_micro_usd": 12
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = SettlementClient::new(format!("{}/v1", server.uri()), "brk_test")
        .expect("settlement client");
    let prices = [super::UsagePriceOverride::parse(
        "provider-a:model-a=2,0,0,10",
    )?];

    let summary = super::reconcile_requests(
        &store,
        &client,
        &[settlement.request_id],
        &prices,
        3,
        Duration::ZERO,
    )
    .await?;

    assert!(summary.accepted());
    assert_eq!(summary.computed, 1);
    assert_eq!(summary.attempts, 1);
    Ok(())
}

#[tokio::test]
async fn bounded_reconciler_exhausts_absent_receipt_without_looping() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder =
        MeteringRecorder::new(store.clone(), pricing()).with_reconciliation_provider("bitrouter");
    let mut settlement = ctx("absent", 0, 0);
    settlement.provider_id = "bitrouter".to_string();
    recorder.record(&mut settlement).await?;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": "not_found",
            "error_description": "request not found"
        })))
        .expect(2)
        .mount(&server)
        .await;
    let client = SettlementClient::new(format!("{}/v1", server.uri()), "brk_test")
        .expect("settlement client");

    let summary = super::reconcile_requests(
        &store,
        &client,
        &[settlement.request_id],
        &[],
        2,
        Duration::ZERO,
    )
    .await?;

    assert!(!summary.accepted());
    assert_eq!(summary.unknown, 1);
    assert_eq!(summary.attempts, 2);
    Ok(())
}

#[tokio::test]
async fn a_later_reconciliation_invocation_retries_authoritative_unknown() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder =
        MeteringRecorder::new(store.clone(), pricing()).with_reconciliation_provider("bitrouter");
    let mut settlement = ctx("eventually-computed", 0, 0);
    settlement.provider_id = "bitrouter".to_string();
    recorder.record(&mut settlement).await?;

    let unknown_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/requests/{}/settlement",
            settlement.request_id
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "request_id": settlement.request_id,
            "state": "unknown",
            "model_id": null,
            "provider_id": null,
            "usage": {
                "uncached_input_tokens": 0,
                "cache_read_tokens": 0,
                "cache_write_tokens": 0,
                "output_tokens": 0,
                "reasoning_tokens": 0
            },
            "final_charge_micro_usd": null
        })))
        .expect(1)
        .mount(&unknown_server)
        .await;
    let unknown_client = SettlementClient::new(format!("{}/v1", unknown_server.uri()), "brk_test")
        .expect("settlement client");
    let first = super::reconcile_requests(
        &store,
        &unknown_client,
        &[settlement.request_id.clone()],
        &[],
        3,
        Duration::ZERO,
    )
    .await?;
    assert_eq!(first.unknown, 1);
    assert_eq!(first.attempts, 1);

    let computed_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/requests/{}/settlement",
            settlement.request_id
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "request_id": settlement.request_id,
            "state": "computed",
            "model_id": "model-a",
            "provider_id": "provider-a",
            "usage": {
                "uncached_input_tokens": 1,
                "cache_read_tokens": 0,
                "cache_write_tokens": 0,
                "output_tokens": 1,
                "reasoning_tokens": 0
            },
            "final_charge_micro_usd": 12
        })))
        .expect(1)
        .mount(&computed_server)
        .await;
    let computed_client =
        SettlementClient::new(format!("{}/v1", computed_server.uri()), "brk_test")
            .expect("settlement client");
    let prices = [super::UsagePriceOverride::parse(
        "provider-a:model-a=2,0,0,10",
    )?];
    let second = super::reconcile_requests(
        &store,
        &computed_client,
        &[settlement.request_id],
        &prices,
        3,
        Duration::ZERO,
    )
    .await?;

    assert!(second.accepted());
    assert_eq!(second.computed, 1);
    assert_eq!(second.attempts, 2);
    Ok(())
}

#[tokio::test]
async fn recorder_never_computes_zero_charge_from_unknown_usage() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone(), pricing());
    let mut settlement = ctx("unknown", 0, 0);
    settlement.usage_origin = bitrouter_sdk::language_model::UsageOrigin::Unknown;

    recorder.record(&mut settlement).await?;
    let records = store.export_usage(TimeWindow::ThisMonth).await?;

    assert_eq!(records[0].final_charge_micro_usd, None);
    assert_eq!(records[0].charge_status, super::ChargeStatus::Unknown);
    assert_eq!(
        records[0]
            .charge_evidence
            .as_ref()
            .and_then(|evidence| evidence.unknown_reason.as_deref()),
        Some("usage_unavailable")
    );
    Ok(())
}

#[tokio::test]
async fn spend_aggregates_across_requests() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone(), pricing());
    recorder.record(&mut ctx("k1", 10, 5)).await?; // 70
    recorder.record(&mut ctx("k1", 100, 0)).await?; // 200
    recorder.record(&mut ctx("k1", 0, 50)).await?; // 500
    let spend = store.get_spend("k1", TimeWindow::ThisMonth).await?;
    assert_eq!(spend, 70 + 200 + 500);
    Ok(())
}

#[tokio::test]
async fn spend_isolates_by_api_key() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone(), pricing());
    recorder.record(&mut ctx("k1", 10, 5)).await?;
    recorder.record(&mut ctx("k2", 100, 0)).await?;
    assert_eq!(store.get_spend("k1", TimeWindow::ThisMonth).await?, 70);
    assert_eq!(store.get_spend("k2", TimeWindow::ThisMonth).await?, 200);
    Ok(())
}

/// Regression for the OSS-refactor column rename: a database that
/// was created by the pre-refactor code has `final_charge_micro_usd`.
/// After `migrate()` runs on it, the column is renamed in place and
/// the new recorder writes work end-to-end.
#[tokio::test]
async fn migrate_renames_legacy_final_charge_column() -> Result<()> {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    // Stand up the pre-refactor schema (final_charge_micro_usd, no
    // cache_read/write columns — the renamer only handles the legacy
    // column; missing columns elsewhere would require a full v2
    // migration which v1.0 doesn't ship). Raw DDL is used here only to
    // fabricate a *pre-migration* database; production schema is all
    // sea-orm migration code.
    pool.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "CREATE TABLE requests (\
            request_id                      TEXT PRIMARY KEY,\
            user_id                         TEXT NOT NULL,\
            api_key_id                      TEXT NOT NULL,\
            model_id                        TEXT NOT NULL,\
            provider_id                     TEXT NOT NULL,\
            prompt_tokens                   INTEGER NOT NULL,\
            completion_tokens               INTEGER NOT NULL,\
            reasoning_tokens                INTEGER NOT NULL,\
            cache_read_tokens               INTEGER NOT NULL DEFAULT 0,\
            cache_write_tokens              INTEGER NOT NULL DEFAULT 0,\
            final_charge_micro_usd          INTEGER NOT NULL DEFAULT 0,\
            streamed                        INTEGER NOT NULL DEFAULT 0,\
            latency_ms                      INTEGER NOT NULL DEFAULT 0,\
            generation_time_ms              INTEGER NOT NULL DEFAULT 0,\
            error                           TEXT,\
            created_at                      TEXT NOT NULL\
         )",
    ))
    .await
    .unwrap();

    // Seed one row using the legacy column.
    pool.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO requests VALUES (\
            'r-legacy', 'u', 'k', 'm', 'p', 1, 2, 0, 0, 0, 42, 0, 0, 0, NULL, ?\
         )",
        [chrono::Utc::now().to_rfc3339().into()],
    ))
    .await
    .unwrap();

    // Run the migrations — migration 3 should detect the legacy column and
    // rename it in place without losing the row.
    db::run_migrations(&pool).await?;

    // Verify the row is reachable through the new column name.
    let store = MeteringStore::new(pool);
    let spend = store.get_spend("k", TimeWindow::ThisMonth).await?;
    assert_eq!(spend, 42, "legacy row's spend is preserved across rename");
    let records = store.export_usage(TimeWindow::ThisMonth).await?;
    assert_eq!(records[0].final_charge_micro_usd, None);
    assert_eq!(records[0].charge_status, super::ChargeStatus::LegacyUnknown);
    Ok(())
}

#[tokio::test]
async fn failed_request_still_records_with_error() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone(), pricing());
    let mut c = ctx("k1", 5, 0);
    c.error = Some(bitrouter_sdk::BitrouterError::internal("boom"));
    recorder.record(&mut c).await?;
    let count = store.get_request_count("k1", TimeWindow::ThisMonth).await?;
    assert_eq!(count, 1, "failed requests still get a metering row");
    Ok(())
}

#[tokio::test]
async fn usage_export_records_are_cloud_usage_compatible() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone(), pricing());

    recorder.record(&mut ctx("k1", 10, 5)).await?;
    let mut failed = ctx("k1", 20, 1);
    failed.request_id = "r-k1-failed".to_string();
    failed.error = Some(bitrouter_sdk::BitrouterError::internal("upstream timeout"));
    recorder.record(&mut failed).await?;

    let records = store.export_usage(TimeWindow::ThisMonth).await?;

    assert_eq!(records.len(), 2);
    let first = records
        .iter()
        .find(|record| record.request_id.as_deref() == Some("r-k1-10-5"))
        .expect("successful request exported");
    assert_eq!(first.id.as_deref(), Some("r-k1-10-5"));
    assert_eq!(first.provider_id, "openai");
    assert_eq!(first.model_id, "gpt-5");
    assert_eq!(first.prompt_tokens, 10);
    assert_eq!(first.completion_tokens, 5);
    assert_eq!(first.final_charge_micro_usd, Some(70));
    assert_eq!(first.status.as_deref(), Some("completed"));

    let failed = records
        .iter()
        .find(|record| record.request_id.as_deref() == Some("r-k1-failed"))
        .expect("failed request exported");
    assert_eq!(failed.prompt_tokens, 20);
    assert_eq!(failed.completion_tokens, 1);
    assert_eq!(failed.final_charge_micro_usd, Some(50));
    assert_eq!(failed.status.as_deref(), Some("failed"));

    let json = serde_json::to_value(first).unwrap();
    serde_json::from_value::<crate::workflow_state::archive::CloudUsageRecord>(json)
        .expect("metering usage export parses as CloudUsageRecord");
    Ok(())
}

#[tokio::test]
async fn usage_export_writes_cloud_usage_compatible_jsonl() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone(), pricing());
    recorder.record(&mut ctx("k1", 12, 3)).await?;

    let path = std::env::temp_dir().join(format!(
        "bitrouter-metering-usage-{}.jsonl",
        uuid::Uuid::new_v4()
    ));
    let records = store.export_usage(TimeWindow::ThisMonth).await?;
    super::MeteringUsageRecord::write_jsonl(&path, &records)?;

    let parsed = crate::workflow_state::archive::CloudUsageRecord::load_snapshot_jsonl(&path)?;
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].request_id.as_deref(), Some("r-k1-12-3"));
    assert_eq!(parsed[0].final_charge_micro_usd, Some(54));

    let _ = std::fs::remove_file(path);
    Ok(())
}

#[test]
fn usage_price_override_imputes_missing_charges() {
    let mut records = vec![super::MeteringUsageRecord {
        id: Some("r1".to_string()),
        request_id: Some("r1".to_string()),
        provider_id: "openai-codex".to_string(),
        model_id: "gpt-5.5".to_string(),
        prompt_tokens: 21,
        completion_tokens: 17,
        final_charge_micro_usd: Some(0),
        status: Some("completed".to_string()),
        ..Default::default()
    }];
    let price = super::UsagePriceOverride::parse("openai-codex:gpt-5.5=5,25").unwrap();

    super::MeteringUsageRecord::apply_price_overrides(&mut records, &[price]);

    assert_eq!(records[0].final_charge_micro_usd, Some(530));
    assert_eq!(records[0].charge_status, super::ChargeStatus::Computed);
    assert_eq!(
        records[0]
            .charge_evidence
            .as_ref()
            .map(|evidence| evidence.pricing_source),
        Some(super::PricingSource::Override)
    );
}

#[test]
fn price_override_preserves_authoritative_reconciliation_records() -> Result<()> {
    let mut records = [
        super::ReconciliationStatus::Pending,
        super::ReconciliationStatus::NotCharged,
        super::ReconciliationStatus::Unknown,
    ]
    .into_iter()
    .map(|reconciliation_status| super::MeteringUsageRecord {
        provider_id: "mock-provider".to_string(),
        model_id: "mock-weak".to_string(),
        prompt_tokens: 12,
        completion_tokens: 5,
        charge_status: if reconciliation_status == super::ReconciliationStatus::NotCharged {
            super::ChargeStatus::NotCharged
        } else {
            super::ChargeStatus::Unknown
        },
        reconciliation_status,
        ..Default::default()
    })
    .collect::<Vec<_>>();
    let original = records.clone();
    let price = super::UsagePriceOverride::parse("mock-provider:mock-weak=1,1")?;

    super::MeteringUsageRecord::apply_price_overrides(&mut records, &[price]);

    assert_eq!(records, original);
    Ok(())
}

#[test]
fn four_rate_override_prices_cache_buckets() {
    let mut records = vec![super::MeteringUsageRecord {
        provider_id: "anthropic".to_string(),
        model_id: "claude-test".to_string(),
        prompt_tokens: 100,
        completion_tokens: 30,
        reasoning_tokens: 10,
        cache_read_tokens: 40,
        cache_write_tokens: 20,
        ..Default::default()
    }];
    let price = super::UsagePriceOverride::parse("anthropic:claude-test=2,0.2,2.5,10")
        .expect("four-rate override");

    super::MeteringUsageRecord::apply_price_overrides(&mut records, &[price]);

    assert_eq!(records[0].final_charge_micro_usd, Some(438));
    assert_eq!(records[0].uncached_input_tokens, 40);
    assert_eq!(records[0].output_tokens, 20);
    assert_eq!(records[0].charge_status, super::ChargeStatus::Computed);
}

#[test]
fn legacy_two_rate_override_refuses_cached_usage() {
    let mut records = vec![super::MeteringUsageRecord {
        provider_id: "anthropic".to_string(),
        model_id: "claude-test".to_string(),
        prompt_tokens: 100,
        completion_tokens: 30,
        cache_read_tokens: 40,
        ..Default::default()
    }];
    let price = super::UsagePriceOverride::parse("anthropic:claude-test=2,10")
        .expect("legacy override remains parseable");

    super::MeteringUsageRecord::apply_price_overrides(&mut records, &[price]);

    assert_eq!(records[0].final_charge_micro_usd, None);
    assert_eq!(records[0].charge_status, super::ChargeStatus::LegacyUnknown);
}

#[test]
fn price_override_rejects_negative_or_non_finite_rates() {
    for value in [
        "anthropic:claude-test=-1,0.2,2.5,10",
        "anthropic:claude-test=2,NaN,2.5,10",
        "anthropic:claude-test=2,0.2,inf,10",
    ] {
        assert!(
            super::UsagePriceOverride::parse(value).is_err(),
            "override should reject {value}"
        );
    }
}
