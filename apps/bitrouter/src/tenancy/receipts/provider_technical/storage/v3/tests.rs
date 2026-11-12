//! Storage/codec unit corpus, never provider/admission authority.
use super::*;
use crate::tenancy::receipts::store::{Candidate, Disposition};
use sea_orm::TransactionTrait;
use serde_json::json;

fn original() -> Result<Terminal> {
    let usage = json!({"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"provenance":"unknown","complete":false});
    let receipt = serde_json::from_value(
        json!({"installation_id":"store","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","correlation_id":"correlation","execution_revision":"revision","state":"incomplete","outcome":"failed","attempts":[{"attempt_id":"attempt","target":null,"outcome":"failed","usage":usage,"native_usage":[]}],"usage":usage,"recorded_at":"2026-01-01T00:00:00Z"}),
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
fn facts() -> Result<ProviderTechnicalFactsV3> {
    Ok(serde_json::from_value(
        json!({"schema_version":3,"scope":{"schema_version":2,"receipt_format":1,"installation_id":"store","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","execution_revision":"revision","operations":[],"pipeline_scope":null,"hop_observations":[{"profile":"core_json_hop_callbacks_v1","attempt_ids":["attempt"],"closing_attempt_id":"attempt","duration_ms":77,"end":"failed","partial":false}]},"observer_hops":[{"profile":"core_json_hop_observer_floor_v1","attempt_ids":["attempt"],"closing_attempt_id":"attempt","duration_ms":0,"end":"failed"}]}),
    )?)
}

#[tokio::test]
async fn retained_v3_reopen_old_views_observer_corruption_and_original_parent_cleanup() -> Result<()>
{
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
        match &terminal {
            Terminal::V1(v) => data.validate_against(&v.receipt)?,
            _ => anyhow::bail!("unit V1"),
        }
        let prepared = Prepared::capture(&data)?;
        ensure!(prepared.version == 3);
        ensure!(super::super::insert(&db, &terminal, &prepared).await?);
        ensure!(!super::super::insert(&db, &terminal, &prepared).await?);
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
        let terminal = crate::tenancy::receipts::store::lookup_terminal(&db, &"store".try_into()?, &"tenant".try_into()?, &"execution".try_into()?).await?.context("reopened original")?;
        ensure!(terminal.json() == original_json && terminal.digest() == original_digest);
        ensure!(super::super::read_v3(&db, &terminal).await? == Some(data.clone()));
        ensure!(super::super::read_v2(&db, &terminal).await? == Some(data.scope.clone()));
        let exact = super::super::read_prepared(&db, &terminal).await?.context("retained envelope")?;
        ensure!(exact.version == 3 && exact.json == prepared.json && exact.digest == prepared.digest);
        ensure!(super::super::retained_bytes(&db).await? == u64::try_from(prepared.json.len())? + crate::tenancy::receipts::capacity::ROW_OVERHEAD);
        let mut bad: Retained = serde_json::from_str(&prepared.json)?;
        bad.observer_hops_json = "{\"not\":\"an observer array\"}".into();
        let json = serde_json::to_string(&bad)?;
        let digest = super::super::digest(3, &json)?;
        let tx = db.begin().await?;
        tx.execute(sea_orm::Statement::from_sql_and_values(sea_orm::DatabaseBackend::Sqlite,
            "UPDATE tenancy_provider_technical_facts SET technical_json=?,technical_digest=?,byte_count=?",
            [json.clone().into(), digest.into(), i64::try_from(json.len())?.into()])).await?;
        ensure!(super::super::read_v3(&tx, &terminal).await.is_err());
        ensure!(super::super::read_v2(&tx, &terminal).await? == Some(data.scope.clone()), "bad observer poisoned scope");
        ensure!(super::super::read(&tx, &terminal).await? == data.scope.operation_view());
        tx.rollback().await?;
        for version in [1, 2] {
            let tx = db.begin().await?;
            tx.execute_unprepared("DELETE FROM tenancy_provider_technical_facts").await?;
            let old = if version == 2 { Prepared::new_v2(&data.scope)? } else {
                let mut scope = data.scope.clone();
                scope.operations = serde_json::from_value(json!([{"attempt_ids":["attempt"],"selected_attempt_id":null,"streamed":false,"upstream_duration_ms":0,"first_semantic":null,"generation_duration_ms":null,"partial":true}]))?;
                Prepared::new(&scope.operation_view().context("unit operation view")?)?
            };
            ensure!(super::super::insert(&tx, &terminal, &old).await?);
            ensure!(super::super::read_v3(&tx, &terminal).await?.is_none());
            let retained = super::super::read_prepared(&tx, &terminal).await?.context("old bytes")?;
            ensure!(retained.version == version && retained.json == old.json && retained.digest == old.digest);
            tx.rollback().await?;
        }
        db.execute_unprepared("DELETE FROM tenancy_execution_receipts WHERE installation_id='store' AND tenant_id='tenant' AND execution_id='execution'").await?;
        ensure!(super::super::retained_bytes(&db).await? == 0);
        Ok(())
    }.await;
    let closed = db.close().await;
    checked?;
    closed?;
    Ok(())
}

#[test]
fn observer_refusal_and_shared_size_limit_keep_scope_only_without_a_second_write() -> Result<()> {
    let mut data = facts()?;
    data.observer_hops[0].closing_attempt_id = "foreign".try_into()?;
    let prepared = Prepared::capture(&data)?;
    ensure!(
        prepared.version == 2
            && serde_json::from_str::<ProviderTechnicalFactsV2>(&prepared.json)? == data.scope
    );
    let mut data = facts()?;
    let mut scope = serde_json::to_value(&data.scope)?;
    scope["pipeline_scope"] = json!({"profile":"core_pipeline_finalized_v1","context_request_id":"","request_duration_ms":1,"streamed":false,"stream":null});
    let base = serde_json::to_vec(&scope)?.len();
    scope["pipeline_scope"]["context_request_id"] = json!("x".repeat(MAX_BYTES - base));
    data.scope = serde_json::from_value(scope)?;
    data.scope.validate()?;
    ensure!(serde_json::to_vec(&data.scope)?.len() == MAX_BYTES);
    let prepared = Prepared::capture(&data)?;
    ensure!(prepared.version == 2 && prepared.json.len() == MAX_BYTES);
    for raw in [
        "[3,{},\"[]\"]",
        "{\"schema_version\":3,\"scope\":{},\"observer_hops_json\":\"[]\",\"unknown\":0}",
    ] {
        ensure!(serde_json::from_str::<Retained>(raw).is_err());
    }
    Ok(())
}
