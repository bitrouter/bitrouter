//! Storage/codec fixtures confer no provider or admission authority.
use super::*;
use crate::tenancy::receipts::store::{Candidate, Disposition};
use sea_orm::{Statement, TransactionTrait};
use serde_json::json;

fn original() -> Result<Terminal> {
    let usage = json!({"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"provenance":"unknown","complete":false});
    let receipt = serde_json::from_value(
        json!({"dispatch":{"mode":"strict"},"installation_id":"store","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","correlation_id":"correlation","execution_revision":"revision","state":"incomplete","outcome":"failed","attempts":[{"attempt_id":"attempt","candidate_index":null,"observed_http_status":503,"target":null,"outcome":"failed","usage":usage,"native_usage":[]}],"usage":usage,"recorded_at":"2026-01-01T00:00:00Z"}),
    )?;
    Ok(Terminal::V1(std::sync::Arc::new(Candidate::new(
        receipt,
        Disposition {
            dispatch: bitrouter_ai::tenancy::operation::DispatchState::Started,
            statuses: vec![Some(503)],
            auxiliary_digest: None,
        },
    )?)))
}

fn facts() -> Result<ProviderTechnicalFacts> {
    Ok(serde_json::from_value(json!({
        "schema_version":3,"installation_id":"store","tenant_id":"tenant",
        "principal_id":"principal","execution_id":"execution","execution_revision":"revision",
        "operations":[{"attempt_ids":["attempt"],"selected_attempt_id":null,"streamed":false,
            "upstream_duration_ms":0,"first_semantic":null,"generation_duration_ms":null,"partial":false}],
        "pipeline_scope":{"profile":"core_pipeline_finalized_v1","context_request_id":"context",
            "request_duration_ms":1,"streamed":false,"stream":null},
        "hop_observations":[{"profile":"core_json_hop_callbacks_v1","attempt_ids":["attempt"],
            "closing_attempt_id":"attempt","duration_ms":77,"end":"failed","partial":false}],
        "observer_hops":[{"profile":"core_json_hop_observer_floor_v1","attempt_ids":["attempt"],
            "closing_attempt_id":"attempt","duration_ms":0,"end":"failed"}]
    }))?)
}

/// UT for current framing, immutable readback and parent-scoped corruption isolation.
/// # Brief
/// Persist all categories, reopen exact bytes, reject forged content and cascade cleanup.
#[tokio::test]
async fn ut_current_payload_reopens_and_corrupt_observers_do_not_poison_receipts() -> Result<()> {
    let home = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        home.path().join("facts.sqlite").display()
    );
    let db = crate::db::connect(&url).await?;
    let checked: Result<_> = async {
        crate::db::run_migrations(&db).await?;
        crate::db::migration::provider_technical::install(&db).await?;
        let terminal = original()?;
        let tx = db.begin().await?;
        ensure!(crate::tenancy::receipts::store::insert_terminal(&tx, &terminal).await?);
        tx.commit().await?;
        let data = facts()?;
        let prepared = Prepared::capture(&data)?;
        ensure!(insert(&db, &terminal, &prepared).await?);
        ensure!(!insert(&db, &terminal, &prepared).await?);
        Ok((
            data,
            prepared,
            terminal.json().to_owned(),
            terminal.digest().to_owned(),
        ))
    }
    .await;
    let closed = db.close().await;
    let (data, prepared, original_json, original_digest) = checked?;
    closed?;
    let db = crate::db::connect(&url).await?;
    let checked: Result<()> = async {
        let terminal = crate::tenancy::receipts::store::lookup_terminal(
            &db, &"store".try_into()?, &"tenant".try_into()?, &"execution".try_into()?,
        ).await?.context("reopened original")?;
        ensure!(terminal.json() == original_json && terminal.digest() == original_digest);
        ensure!(read(&db, &terminal).await? == Some(data.clone()));
        let exact = read_prepared(&db, &terminal).await?.context("retained current bytes")?;
        ensure!(exact.version == 3 && exact.json == prepared.json && exact.digest == prepared.digest);
        ensure!(retained_bytes(&db).await? == u64::try_from(prepared.json.len())?
            + crate::tenancy::receipts::capacity::ROW_OVERHEAD);
        for case in ["observer_shape", "observer_group", "unarmed_operation", "old_version", "receipt_format", "envelope"] {
            let mut value = serde_json::to_value(&data)?;
            match case {
                "observer_shape" => value["observer_hops"] = json!({"not":"an array"}),
                "observer_group" => value["observer_hops"][0]["closing_attempt_id"] = json!("foreign"),
                "unarmed_operation" => value["operations"][0]["attempt_ids"] = json!(["never-armed"]),
                "old_version" => value["schema_version"] = json!(2),
                "receipt_format" => value["receipt_format"] = json!(1),
                "envelope" => value = json!({"schema_version":3,"scope":value,"observer_hops_json":"[]"}),
                _ => anyhow::bail!("unknown storage case"),
            }
            if case == "unarmed_operation" {
                // A valid standalone object still cannot invent a receipt arm.
                serde_json::from_value::<ProviderTechnicalFacts>(value.clone())?.validate()?;
            }
            let json = serde_json::to_string(&value)?;
            let tx = db.begin().await?;
            // A valid digest and length must not hide an invalid current object.
            tx.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "UPDATE tenancy_provider_technical_facts SET technical_json=?,technical_digest=?,byte_count=?",
                [json.clone().into(), digest(&json).into(), i64::try_from(json.len())?.into()])).await?;
            ensure!(read(&tx, &terminal).await.is_err(), "{case}");
            ensure!(read_prepared(&tx, &terminal).await.is_err(), "{case}");
            let parent = crate::tenancy::receipts::store::lookup_terminal(
                &tx, &"store".try_into()?, &"tenant".try_into()?, &"execution".try_into()?,
            ).await?.context("original remains readable")?;
            ensure!(parent.json() == original_json && parent.digest() == original_digest);
            tx.rollback().await?;
        }
        for version in [1, 2] {
            let tx = db.begin().await?;
            tx.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "UPDATE tenancy_provider_technical_facts SET schema_version=?", [version.into()])).await?;
            ensure!(read(&tx, &terminal).await.is_err());
            tx.rollback().await?;
        }
        db.execute_unprepared("DELETE FROM tenancy_execution_receipts WHERE installation_id='store' AND tenant_id='tenant' AND execution_id='execution'").await?;
        ensure!(retained_bytes(&db).await? == 0);
        Ok(())
    }.await;
    let closed = db.close().await;
    checked?;
    closed?;
    Ok(())
}

/// UT for optional observer refusal and the shared encoded byte boundary.
/// # Brief
/// Omit invalid or over-budget observers while retaining every valid base category.
#[test]
fn ut_observer_refusal_and_shared_byte_limit_preserve_all_other_categories() -> Result<()> {
    let data = facts()?;
    let mut expected = data.clone();
    expected.observer_hops.clear();
    let mut invalid = data.clone();
    invalid.observer_hops[0].closing_attempt_id = "foreign".try_into()?;
    let prepared = Prepared::capture(&invalid)?;
    ensure!(prepared.version == 3);
    ensure!(serde_json::from_str::<ProviderTechnicalFacts>(&prepared.json)? == expected);

    expected
        .pipeline_scope
        .as_mut()
        .context("scope")?
        .context_request_id
        .clear();
    let base = serde_json::to_vec(&expected)?.len();
    expected
        .pipeline_scope
        .as_mut()
        .context("scope")?
        .context_request_id = "x".repeat(MAX_BYTES - base);
    expected.validate()?;
    ensure!(serde_json::to_vec(&expected)?.len() == MAX_BYTES);
    let mut oversized = expected.clone();
    oversized.observer_hops = data.observer_hops;
    ensure!(oversized.validate().is_err());
    let prepared = Prepared::capture(&oversized)?;
    ensure!(prepared.version == 3 && prepared.json.len() == MAX_BYTES);
    ensure!(serde_json::from_str::<ProviderTechnicalFacts>(&prepared.json)? == expected);
    oversized
        .pipeline_scope
        .as_mut()
        .context("scope")?
        .context_request_id
        .push('x');
    ensure!(
        Prepared::capture(&oversized).is_err(),
        "oversized base must refuse capture"
    );
    Ok(())
}
