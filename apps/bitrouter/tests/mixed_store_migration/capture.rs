//! The complete historical journal is independent of the mock provider's later bills.
use super::product;
use anyhow::{Context, Result};
use product::cloud::settlement::{SettlementReceipt, SettlementState, SettlementUsage};
use product::metering::store::MeteringStore;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use std::{fs, path::Path};

pub(super) async fn install(dir: &Path, db: &DatabaseConnection) -> Result<()> {
    const CAPTURE: &[u8] = include_bytes!("../fixtures/baseline-runtime-v7/database.sqlite.zst");
    let capture = dir.join("captured.sqlite");
    fs::write(&capture, zstd::stream::decode_all(CAPTURE)?)?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "ATTACH ? AS captured",
        [capture.to_str().context("fixture path")?.into()],
    ))
    .await?;
    for table in [
        "bro_executions",
        "bro_execution_index",
        "bro_execution_records",
        "bro_acceptance_keys",
        "bro_runtime_owners",
    ] {
        db.execute_unprepared(&format!(
            "INSERT INTO main.{table} SELECT * FROM captured.{table}"
        ))
        .await?;
    }
    db.execute_unprepared("UPDATE main.bro_runtime_ownership SET (generation,server_instance_id,stopped_at_ms)=(SELECT generation,server_instance_id,stopped_at_ms FROM captured.bro_runtime_ownership WHERE id=1)").await?;
    db.execute_unprepared("DETACH DATABASE captured").await?;
    Ok(())
}

pub(super) async fn settle(db: &DatabaseConnection, id: &str, amount: i64) -> Result<()> {
    db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO requests(request_id,user_id,api_key_id,model_id,provider_id,prompt_tokens,completion_tokens,reasoning_tokens,created_at,charge_status,reconciliation_status) VALUES(?,'local','local','test-model','mock',11,7,0,'now','unknown','pending')",[id.into()])).await?;
    MeteringStore::new(db.clone())
        .apply_authoritative_receipt_charge(&SettlementReceipt {
            request_id: id.into(),
            state: SettlementState::Computed,
            model_id: Some("test-model".into()),
            provider_id: Some("mock".into()),
            usage: SettlementUsage {
                uncached_input_tokens: 11,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 7,
                reasoning_tokens: 0,
            },
            final_charge_micro_usd: Some(amount),
        })
        .await?;
    Ok(())
}
