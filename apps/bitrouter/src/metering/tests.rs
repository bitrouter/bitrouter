//! Metering integration tests against an in-memory SQLite database.

use std::sync::Arc;

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

use bitrouter_sdk::Result;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{SettlementContext, SettlementRecorder};

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
async fn recorder_writes_zero_when_pricing_missing() -> Result<()> {
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
    }];
    let price = super::UsagePriceOverride::parse("openai-codex:gpt-5.5=5,25").unwrap();

    super::MeteringUsageRecord::apply_price_overrides(&mut records, &[price]);

    assert_eq!(records[0].final_charge_micro_usd, Some(530));
}
