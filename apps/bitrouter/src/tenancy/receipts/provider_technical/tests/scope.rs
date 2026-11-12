//! Value-only collector controls plus real SQLite read/reopen. No authority fixture.
use super::*;
use bitrouter_ai::tenancy::provider_technical_facts_v2::{
    HopEnd, HopProfile, PipelineProfile, PipelineScope,
};
use tokio_util::sync::CancellationToken;
fn own_scope() -> PipelineScope {
    PipelineScope {
        profile: PipelineProfile::CorePipelineFinalizedV1,
        context_request_id: "own-context-not-correlation".into(),
        request_duration_ms: 321,
        streamed: false,
        stream: None,
    }
}
fn observed() -> Result<Arc<Capture>> {
    let capture = Arc::new(Capture::default());
    capture.register_context("own-context-not-correlation");
    let hop = capture
        .begin_hop(HopProfile::CoreJsonHopCallbacksV1)
        .context("hop")?;
    let operation = capture.begin(Instant::now(), false);
    let id = "attempt".try_into()?;
    hop.armed(&id);
    operation.armed(&id);
    operation.finish(false);
    operation.selected();
    hop.selected(false);
    hop.finish(
        HopProfile::CoreJsonHopCallbacksV1,
        HopEnd::Completed,
        9,
        false,
    );
    capture.freeze();
    Ok(capture)
}
#[tokio::test]
async fn actual_local_finalize_notification_can_race_receipt_without_refreezing_it() -> Result<()> {
    let capture = observed()?;
    let terminal = terminal()?;
    let original = (
        terminal.json().to_owned(),
        terminal.private_json().to_owned(),
        terminal.digest().to_owned(),
    );
    let pending = capture.pending().context("one optional descriptor")?;
    ensure!(capture.pending().is_none(), "second writer descriptor");
    let cancel = CancellationToken::new();
    let finish = pending.finish(
        &terminal,
        tokio::time::Instant::now() + Duration::from_secs(1),
        &cancel,
    );
    tokio::pin!(finish);
    tokio::select! {biased;_=&mut finish=>anyhow::bail!("unfinalized owning context invented a value"),()=tokio::task::yield_now()=>{}}
    capture.finalize_scope(own_scope());
    let value = finish.await.context("notified scope")?;
    ensure!(value.pipeline_scope == Some(own_scope()));
    ensure!(value.operations.len() == 1 && value.hop_observations[0].duration_ms == Some(9));
    ensure!(
        terminal.json() == original.0
            && terminal.private_json() == original.1
            && terminal.digest() == original.2
    );
    Ok(())
}
#[tokio::test]
async fn optional_scope_cancel_deadline_and_unfinished_hop_keep_nulls_and_no_second_snapshot()
-> Result<()> {
    let capture = observed()?;
    let cancel = CancellationToken::new();
    cancel.cancel();
    ensure!(
        capture
            .pending()
            .context("pending")?
            .finish(
                &terminal()?,
                tokio::time::Instant::now() + Duration::from_secs(1),
                &cancel
            )
            .await
            .is_none()
    );
    let capture = observed()?;
    let value = capture
        .pending()
        .context("pending")?
        .finish(
            &terminal()?,
            tokio::time::Instant::now(),
            &CancellationToken::new(),
        )
        .await
        .context("observed operation prefix")?;
    ensure!(value.pipeline_scope.is_none() && value.operations.len() == 1);
    // A late finalizer cannot rewrite the one already-taken optional snapshot.
    capture.finalize_scope(own_scope());
    ensure!(value.pipeline_scope.is_none() && capture.pending().is_none());
    let capture = Arc::new(Capture::default());
    let hop = capture
        .begin_hop(HopProfile::CoreJsonHopCallbacksV1)
        .context("unended hop")?;
    hop.armed(&"attempt".try_into()?);
    capture.freeze();
    let value = capture
        .pending()
        .context("hop descriptor")?
        .finish(
            &terminal()?,
            tokio::time::Instant::now() + Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await
        .context("hop prefix")?;
    ensure!(value.operations.is_empty() && value.pipeline_scope.is_none());
    ensure!(
        value.hop_observations[0].end == HopEnd::Unobserved
            && value.hop_observations[0].partial
            && value.hop_observations[0].duration_ms.is_none()
    );
    Ok(())
}
#[tokio::test]
async fn payload_versions_keep_original_bytes_and_exact_storage_accounting_across_reopen()
-> Result<()> {
    use sea_orm::{ConnectionTrait, TransactionTrait};
    let home = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        home.path().join("scope.sqlite").display()
    );
    let db = crate::db::connect(&url).await?;
    let checked: Result<_> = async {
        crate::db::run_migrations(&db).await?;
        crate::db::migration::provider_technical::install(&db).await?;
        let original = terminal()?;
        let tx = db.begin().await?;
        ensure!(super::super::super::store::insert_terminal(&tx, &original).await?);
        tx.commit().await?;
        let legacy = storage::Prepared::new(&facts()?)?;
        ensure!(storage::insert(&db, &original, &legacy).await?);
        ensure!(
            storage::read_v2(&db, &original).await?.is_none(),
            "legacy bytes were relabeled"
        );
        let retained = storage::read_prepared(&db, &original)
            .await?
            .context("legacy retained")?;
        ensure!(
            retained.version == 1
                && retained.json == legacy.json
                && retained.digest == legacy.digest
        );
        let capture = observed()?;
        capture.finalize_scope(own_scope());
        let value = capture
            .pending()
            .context("pending")?
            .finish(
                &original,
                tokio::time::Instant::now() + Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .context("new observed scope")?;
        let prepared = storage::Prepared::new_v2(&value)?;
        ensure!(
            storage::insert(&db, &original, &prepared).await.is_err(),
            "new writer overwrote payload1"
        );
        // Separate transaction models a distinct empty optional slot, never a production backfill.
        let tx = db.begin().await?;
        tx.execute_unprepared("DELETE FROM tenancy_provider_technical_facts")
            .await?;
        ensure!(storage::insert(&tx, &original, &prepared).await?);
        tx.commit().await?;
        let original_bytes = (
            original.json().to_owned(),
            original.private_json().to_owned(),
            original.digest().to_owned(),
        );
        ensure!(
            storage::retained_bytes(&db).await?
                == u64::try_from(prepared.json.len())?
                    + crate::tenancy::receipts::capacity::ROW_OVERHEAD
        );
        Ok((value, prepared, original_bytes))
    }
    .await;
    let closed = db.close().await;
    let (value, prepared, original_bytes) = checked?;
    closed?;
    let db = crate::db::connect(&url).await?;
    let checked:Result<()>=async {
 let original=super::super::super::store::lookup_terminal(&db,&"installation".try_into()?,&"tenant".try_into()?,&"execution".try_into()?).await?.context("real reopened original")?;
 ensure!(original.json()==original_bytes.0 && original.private_json()==original_bytes.1 && original.digest()==original_bytes.2);
 ensure!(storage::read_v2(&db,&original).await?==Some(value.clone()));ensure!(storage::read(&db,&original).await?==value.operation_view());
 let exact=storage::read_prepared(&db,&original).await?.context("reopened raw scope")?;
 ensure!(exact.version==2 && exact.json==prepared.json && exact.digest==prepared.digest);
 db.execute_unprepared("DELETE FROM tenancy_execution_receipts WHERE installation_id='installation' AND tenant_id='tenant' AND execution_id='execution'").await?;
 ensure!(storage::retained_bytes(&db).await?==0,"original parent FK did not remove scope");Ok(())
 }.await;
    let closed = db.close().await;
    checked?;
    closed?;
    Ok(())
}
