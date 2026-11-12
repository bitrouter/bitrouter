//! Unit-only clock/association values confer no send or financial authority.
use super::*;
use crate::tenancy::receipts::store::{Candidate, Disposition, format::Terminal};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::{Attempt, ExecutionReceipt, Outcome, Provenance, ReceiptState, Usage},
    operation::DispatchState,
};

fn terminal() -> Result<Terminal> {
    Ok(Terminal::V1(Arc::new(Candidate::new(
        ExecutionReceipt {
            installation_id: "installation".try_into()?,
            tenant_id: "tenant".try_into()?,
            principal_id: "principal".try_into()?,
            execution_id: "execution".try_into()?,
            correlation_id: "correlation".try_into()?,
            execution_revision: "revision".try_into()?,
            state: ReceiptState::Incomplete,
            outcome: Outcome::Failed,
            attempts: vec![Attempt {
                attempt_id: "attempt".try_into()?,
                target: None,
                outcome: Outcome::Failed,
                usage: usage(),
                native_usage: Vec::new(),
            }],
            usage: usage(),
            recorded_at: "2026-01-01T00:00:00Z".try_into()?,
        },
        Disposition {
            dispatch: DispatchState::Started,
            statuses: vec![Some(503)],
            auxiliary_digest: None,
        },
    )?)))
}
fn usage() -> Usage {
    Usage {
        input_tokens: None,
        output_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
        provenance: Provenance::Unknown,
        complete: false,
    }
}
fn facts() -> Result<ProviderTechnicalFactsV1> {
    let capture = Arc::new(Capture::default());
    let operation = capture.begin(Instant::now(), false);
    operation.armed(&"attempt".try_into()?);
    operation.finish(false);
    operation.selected();
    capture.freeze();
    capture.take(&terminal()?).context("unit observations")
}

#[test]
fn original_json_floor_and_stream_helper_keep_zero_submillisecond_and_saturation() {
    assert_eq!(json_millis(Duration::ZERO), 0);
    assert_eq!(json_millis(Duration::from_nanos(1)), 0);
    assert_eq!(stream_millis(Duration::ZERO), 0);
    assert_eq!(stream_millis(Duration::from_nanos(1)), 1);
    assert_eq!(json_millis(Duration::MAX), u64::MAX);
    assert_eq!(stream_millis(Duration::MAX), u64::MAX);
}

#[test]
fn unarmed_is_absent_prefix_is_partial_and_single_semantic_has_zero_generation() -> Result<()> {
    let capture = Arc::new(Capture::default());
    let started = Instant::now();
    let _operation = capture.begin(started, true);
    capture.freeze();
    ensure!(capture.take(&terminal()?).is_none());
    let capture = Arc::new(Capture::default());
    let operation = capture.begin(started, true);
    operation.armed(&"attempt".try_into()?);
    operation.semantic(
        started + Duration::from_nanos(1),
        crate::gateway::pipeline::timing::FirstTokenKind::Reasoning,
    );
    capture.freeze();
    let value = capture.take(&terminal()?).context("partial facts")?;
    ensure!(value.operations[0].partial && value.operations[0].upstream_duration_ms.is_none());
    ensure!(
        value.operations[0]
            .first_semantic
            .as_ref()
            .context("first")?
            .ttft_ms
            == 1
    );
    ensure!(value.operations[0].generation_duration_ms == Some(0));
    // Later data cannot mutate the frozen prefix or generate a second write.
    operation.semantic(
        started + Duration::from_secs(1),
        crate::gateway::pipeline::timing::FirstTokenKind::Text,
    );
    operation.source_end(true);
    operation.finish(false);
    ensure!(capture.take(&terminal()?).is_none());
    ensure!(value.operations[0].generation_duration_ms == Some(0));
    Ok(())
}

#[test]
fn operation_ownership_has_no_arc_cycle_and_busy_capture_never_changes_receipt() -> Result<()> {
    let capture = Arc::new(Capture::default());
    let weak = Arc::downgrade(&capture);
    let operation = capture.begin(Instant::now(), false);
    operation.armed(&"attempt".try_into()?);
    let before = terminal()?;
    let original = (
        before.json().to_owned(),
        before.private_json().to_owned(),
        before.digest().to_owned(),
    );
    let lock = capture
        .0
        .lock()
        .map_err(|_| anyhow::anyhow!("unit capture lock"))?;
    operation.selected();
    capture.freeze();
    drop(lock);
    ensure!(
        before.json() == original.0
            && before.private_json() == original.1
            && before.digest() == original.2
    );
    drop(capture);
    ensure!(weak.upgrade().is_none());
    drop(operation);
    Ok(())
}

#[tokio::test]
async fn absent_optional_schema_is_null_but_partial_shadowed_and_forged_layouts_refuse()
-> Result<()> {
    use sea_orm::{ConnectionTrait, TransactionTrait};
    let home = tempfile::tempdir()?;
    let db = crate::db::connect(&format!(
        "sqlite://{}?mode=rwc",
        home.path().join("optional.sqlite").display()
    ))
    .await?;
    let checked: Result<()> = async {
        crate::db::run_migrations(&db).await?;
        let original = terminal()?;
        let tx = db.begin().await?;
        ensure!(super::super::store::insert_terminal(&tx, &original).await?);
        tx.commit().await?;
        ensure!(!crate::db::migration::provider_technical::available(&db).await?);
        ensure!(storage::read(&db, &original).await?.is_none());

        let tx = db.begin().await?;
        tx.execute_unprepared("CREATE TEMP VIEW tenancy_provider_technical_facts AS SELECT 1")
            .await?;
        ensure!(
            crate::db::migration::provider_technical::available(&tx)
                .await
                .is_err()
        );
        ensure!(storage::read(&tx, &original).await.is_err());
        tx.rollback().await?;

        db.execute_unprepared("CREATE TABLE tenancy_provider_technical_facts(foreign_data TEXT)")
            .await?;
        ensure!(
            crate::db::migration::provider_technical::available(&db)
                .await
                .is_err()
        );
        ensure!(storage::read(&db, &original).await.is_err());
        ensure!(
            crate::db::migration::provider_technical::install(&db)
                .await
                .is_err()
        );
        db.execute_unprepared("DROP TABLE tenancy_provider_technical_facts")
            .await?;
        crate::db::migration::provider_technical::install(&db).await?;
        ensure!(crate::db::migration::provider_technical::available(&db).await?);
        ensure!(storage::read(&db, &original).await?.is_none());
        db.execute_unprepared("UPDATE tenancy_provider_technical_schema SET version=2")
            .await?;
        ensure!(
            crate::db::migration::provider_technical::available(&db)
                .await
                .is_err()
        );
        ensure!(storage::read(&db, &original).await.is_err());
        let retained = super::super::store::lookup_terminal(
            &db,
            &"installation".try_into()?,
            &"tenant".try_into()?,
            &"execution".try_into()?,
        )
        .await?
        .context("original receipt remains readable")?;
        ensure!(retained.json() == original.json() && retained.digest() == original.digest());
        Ok(())
    }
    .await;
    let closed = db.close().await;
    checked?;
    closed?;
    Ok(())
}

#[tokio::test]
async fn optional_bytes_restart_idempotence_corruption_archive_and_exact_parent_cleanup()
-> Result<()> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};
    let root = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        root.path().join("facts.sqlite").display()
    );
    let db = crate::db::connect(&url).await?;
    let first: Result<_> = async {
        crate::db::run_migrations(&db).await?;
        crate::db::migration::provider_technical::install(&db).await?;
        let original = terminal()?;
        let tx = db.begin().await?;
        ensure!(super::super::store::insert_terminal(&tx, &original).await?);
        tx.commit().await?;
        ensure!(storage::read(&db, &original).await?.is_none());
        let prepared = storage::Prepared::new(&facts()?)?;
        ensure!(storage::insert(&db, &original, &prepared).await?);
        ensure!(!storage::insert(&db, &original, &prepared).await?);
        let before = (
            original.json().to_owned(),
            original.private_json().to_owned(),
            original.digest().to_owned(),
        );
        let mut foreign = facts()?;
        foreign.operations[0].upstream_duration_ms = Some(999999);
        ensure!(
            storage::insert(&db, &original, &storage::Prepared::new(&foreign)?)
                .await
                .is_err()
        );
        let Terminal::V1(candidate) = &original else {
            anyhow::bail!("unit V1")
        };
        let charge = |_, _| Ok(());
        let archive = super::super::store::archive::Prepared::new(candidate, &charge)?;
        let tx = db.begin().await?;
        ensure!(super::super::store::archive::archive_one(&tx, &archive, &charge).await?);
        tx.commit().await?;
        ensure!(storage::retained_bytes(&db).await? > 0);
        Ok((before, prepared.json))
    }
    .await;
    let closed = db.close().await;
    let (before, expected) = first?;
    closed?;
    let db = crate::db::connect(&url).await?;
    let checked:Result<()>=async {
        let original=super::super::store::lookup_terminal(&db,&"installation".try_into()?,&"tenant".try_into()?,&"execution".try_into()?).await?.context("original archived receipt")?;
        ensure!(original.json()==before.0 && original.private_json()==before.1 && original.digest()==before.2);
        ensure!(serde_json::to_string(&storage::read(&db,&original).await?.context("reopened same-key facts")?)?==expected);
        for mutation in ["UPDATE tenancy_provider_technical_facts SET technical_json=printf('%.*c',65537,'x')",
            "UPDATE tenancy_provider_technical_facts SET byte_count=0",
            "UPDATE tenancy_provider_technical_facts SET receipt_digest=printf('%064d',0)"] {
            let tx=db.begin().await?;tx.execute_unprepared(mutation).await?;
            ensure!(storage::read(&tx,&original).await.is_err());
            let loaded=super::super::store::lookup_terminal(&tx,&"installation".try_into()?,&"tenant".try_into()?,&"execution".try_into()?).await?.context("original still readable")?;
            ensure!(loaded.digest()==before.2);tx.rollback().await?;
        }
        db.execute_unprepared("DELETE FROM tenancy_execution_receipts WHERE installation_id='installation' AND tenant_id='tenant' AND execution_id='execution'").await?;
        let row=db.query_one(Statement::from_string(DatabaseBackend::Sqlite,"SELECT count(*) AS count FROM tenancy_provider_technical_facts")).await?.context("exact child removal")?;
        ensure!(row.try_get::<i64>("","count")?==0);Ok(())
    }.await;
    let closed = db.close().await;
    checked?;
    closed?;
    Ok(())
}

#[test]
fn lost_capture_is_sticky_and_unmeasured_final_selection_cannot_reuse_a_previous_clock()
-> Result<()> {
    let capture = Arc::new(Capture::default());
    let other = Arc::new(Capture::default());
    let operation = capture.begin(Instant::now(), false);
    ensure!(operation.belongs_to(&capture) && !operation.belongs_to(&other));
    operation.armed(&"attempt".try_into()?);
    operation.finish(false);
    operation.selected();
    // A later actual successful but unmeasured bridge selection clears the
    // old selected pointer rather than projecting that previous provider clock.
    capture.clear_selected();
    capture.freeze();
    let data = capture
        .take(&terminal()?)
        .context("previous observations")?;
    ensure!(data.operations[0].selected_attempt_id.is_none());
    let capture = Arc::new(Capture::default());
    let operation = capture.begin(Instant::now(), false);
    let held = operation
        .observed
        .lock()
        .map_err(|_| anyhow::anyhow!("unit observation lock"))?;
    operation.armed(&"attempt".try_into()?);
    drop(held);
    ensure!(!capture.healthy());
    operation.armed(&"attempt".try_into()?);
    capture.freeze();
    ensure!(capture.take(&terminal()?).is_none());
    Ok(())
}

#[test]
fn selected_stream_stop_and_intermediate_source_end_keep_distinct_operation_clocks() -> Result<()> {
    let capture = Arc::new(Capture::default());
    let started = Instant::now();
    let operation = capture.begin(started, true);
    operation.armed(&"attempt".try_into()?);
    operation.semantic(
        started,
        crate::gateway::pipeline::timing::FirstTokenKind::Tool,
    );
    operation.semantic(
        started + Duration::from_millis(3),
        crate::gateway::pipeline::timing::FirstTokenKind::Text,
    );
    operation.source_end(true);
    operation.selected();
    capture.finish_selected();
    capture.freeze();
    let data = capture
        .take(&terminal()?)
        .context("selected original stream stop")?;
    let observed = &data.operations[0];
    ensure!(!observed.partial && observed.upstream_duration_ms.is_some());
    ensure!(
        observed.first_semantic.as_ref().context("first tool")?.kind == FirstSemanticKind::Tool
    );
    ensure!(observed.generation_duration_ms == Some(3));
    // A completed intermediate operation has its own accepted source end even
    // when a later unmeasured operation is the original final selection.
    let capture = Arc::new(Capture::default());
    let operation = capture.begin(Instant::now(), true);
    operation.armed(&"attempt".try_into()?);
    operation.source_end(true);
    capture.clear_selected();
    capture.freeze();
    let data = capture
        .take(&terminal()?)
        .context("intermediate source stop")?;
    ensure!(!data.operations[0].partial && data.operations[0].upstream_duration_ms.is_some());
    ensure!(data.operations[0].selected_attempt_id.is_none());
    ensure!(
        data.operations[0].first_semantic.is_none()
            && data.operations[0].generation_duration_ms.is_none()
    );
    Ok(())
}

mod scope;
