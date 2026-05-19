//! Metering integration tests against an in-memory SQLite pool.

use sqlx::SqlitePool;
use std::sync::Arc;

use bitrouter_sdk::Result;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{SettlementContext, SettlementRecorder};

use super::{MeteringRecorder, MeteringStore, ModelPricing, PricingTable, TimeWindow, migrate};

async fn pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    migrate(&pool).await.unwrap();
    pool
}

fn ctx(api_key: &str, prompt: u64, completion: u64) -> SettlementContext {
    SettlementContext {
        request_id: format!("r-{api_key}-{prompt}-{completion}"),
        caller: CallerContext::new(api_key, format!("u-{api_key}")),
        target: None,
        model_id: "gpt-5".into(),
        provider_id: "openai".into(),
        prompt_tokens: prompt,
        completion_tokens: completion,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        streamed: false,
        latency_ms: 100,
        generation_time_ms: 80,
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
    recorder.record(&ctx("k1", 10, 5)).await?;
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
    recorder.record(&ctx("k1", 10, 5)).await?;
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
    recorder.record(&ctx("k1", 10, 5)).await?; // 70
    recorder.record(&ctx("k1", 100, 0)).await?; // 200
    recorder.record(&ctx("k1", 0, 50)).await?; // 500
    let spend = store.get_spend("k1", TimeWindow::ThisMonth).await?;
    assert_eq!(spend, 70 + 200 + 500);
    Ok(())
}

#[tokio::test]
async fn spend_isolates_by_api_key() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone(), pricing());
    recorder.record(&ctx("k1", 10, 5)).await?;
    recorder.record(&ctx("k2", 100, 0)).await?;
    assert_eq!(store.get_spend("k1", TimeWindow::ThisMonth).await?, 70);
    assert_eq!(store.get_spend("k2", TimeWindow::ThisMonth).await?, 200);
    Ok(())
}

#[tokio::test]
async fn failed_request_still_records_with_error() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone(), pricing());
    let mut c = ctx("k1", 5, 0);
    c.error = Some(bitrouter_sdk::BitrouterError::internal("boom"));
    recorder.record(&c).await?;
    let count = store.get_request_count("k1", TimeWindow::ThisMonth).await?;
    assert_eq!(count, 1, "failed requests still get a metering row");
    Ok(())
}
