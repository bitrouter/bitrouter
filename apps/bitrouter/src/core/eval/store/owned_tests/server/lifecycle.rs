//! Backend composites: committed replay, external RR writers, atomic failure, and freeze.
use super::*;
use futures::{FutureExt, future::LocalBoxFuture};
use std::panic::AssertUnwindSafe;

mod database;
mod fault;
use database::Fixture;

const FROZEN: &str = "2026-10-25T08:02:00.000+08:00";

async fn run(
    case: impl for<'a> FnOnce(&'a mut Fixture) -> LocalBoxFuture<'a, Result<()>>,
) -> Result<()> {
    let mut fixture = Fixture::create().await?;
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        AssertUnwindSafe(case(&mut fixture)).catch_unwind(),
    )
    .await
    .context("owned backend campaign deadline exceeded")
    .and_then(|value| value.map_err(|_| anyhow::anyhow!("owned backend campaign unwound")))
    .and_then(|value| value);
    fixture.finish(result).await
}

async fn initial(fixture: &mut Fixture) -> Result<EvalStore> {
    let db = fixture.connect().await?;
    crate::db::run_migrations(&db).await?;
    scoped(&db)
}

async fn reconnect(fixture: &mut Fixture, original: &EvalStore) -> Result<EvalStore> {
    Ok(EvalStore {
        db: fixture.connect().await?,
        scope: original.scope.clone(),
        provenance: original.provenance.clone(),
    })
}

fn creator(store: &EvalStore, principal: &str) -> Result<EvalStore> {
    let (installation, tenant) = store.scope.snapshot_namespace().context("tenant scope")?;
    let (scope, provenance) = Scope::for_test(
        installation.try_into()?,
        tenant.try_into()?,
        principal.try_into()?,
    );
    ensure!(
        scope == store.scope,
        "fixture changed namespace while changing creator"
    );
    Ok(EvalStore {
        db: store.db.clone(),
        scope,
        provenance,
    })
}

fn evaluation(store: &EvalStore, verdict: EvalVerdict, key: &str) -> Result<EvaluationResult> {
    let mut value = result(verdict, key);
    value.evaluator.authority_id = store
        .provenance
        .creator()
        .context("fixture creator")?
        .to_string();
    Ok(value)
}

async fn seed(fixture: &mut Fixture, store: &EvalStore) -> Result<String> {
    let input = evaluation(store, EvalVerdict::Pass, "pass")?;
    let slot = fixture.start(&store.db).await?;
    let subject = store
        .put_owned_in(fixture.tx(slot)?, &tests::subject()?, &|| Ok(()))
        .await?;
    ensure!(subject.outcome == SubjectInsertOutcome::Inserted);
    let admitted = store
        .submit_owned_in(fixture.tx(slot)?, &input, &|| Ok(()))
        .await?;
    ensure!(!admitted.duplicate && admitted.status == AdmissionStatus::Admitted);
    fixture.commit(slot).await?;
    Ok(admitted.result_id)
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE privileges"]
async fn server_owned_eval_committed_replay_preserves_provenance_and_historical_snapshot()
-> Result<()> {
    run(|fixture| Box::pin(committed(fixture))).await
}

async fn committed(fixture: &mut Fixture) -> Result<()> {
    let first = initial(fixture).await?;
    let logical = seed(fixture, &first).await?;
    let subject_before = first
        .subjects()
        .one(&first.db)
        .await?
        .context("committed subject")?;
    let result_before = first
        .results()
        .one(&first.db)
        .await?
        .context("committed result")?;
    let admissions_before = first
        .admissions()
        .order_by_asc(admission_entity::Column::Sequence)
        .all(&first.db)
        .await?;
    ensure!(subject_before.creator_principal_id.as_deref() == Some("principal"));
    ensure!(result_before.creator_principal_id.as_deref() == Some("principal"));
    ensure!(admissions_before.len() == 1);
    database::close(&first.db).await?;

    // Fresh pool; do not call scoped(), which would generate another tenant.
    let reader = reconnect(fixture, &first).await?;
    let reviewer = creator(&reader, "reviewer")?;
    let slot = fixture.start(&reader.db).await?;
    let replay = reader
        .submit_owned_in(
            fixture.tx(slot)?,
            &evaluation(&reader, EvalVerdict::Pass, "pass")?,
            &|| Ok(()),
        )
        .await?;
    ensure!(
        replay.duplicate
            && replay.result_id == logical
            && replay.status == AdmissionStatus::Admitted
    );
    let duplicate = reviewer
        .put_owned_in(fixture.tx(slot)?, &tests::subject()?, &|| Ok(()))
        .await?;
    ensure!(duplicate.outcome == SubjectInsertOutcome::Duplicate);
    // Inspect before rollback: cleanup must not hide an unintended replay write.
    ensure!(reader.subjects().one(fixture.tx(slot)?).await? == Some(subject_before.clone()));
    ensure!(reader.results().one(fixture.tx(slot)?).await? == Some(result_before.clone()));
    ensure!(
        reader
            .admissions()
            .order_by_asc(admission_entity::Column::Sequence)
            .all(fixture.tx(slot)?)
            .await?
            == admissions_before
    );
    fixture.rollback(slot).await?;
    ensure!(reader.subjects().one(&reader.db).await? == Some(subject_before.clone()));
    ensure!(reader.results().one(&reader.db).await? == Some(result_before.clone()));
    ensure!(
        reader
            .admissions()
            .order_by_asc(admission_entity::Column::Sequence)
            .all(&reader.db)
            .await?
            == admissions_before
    );

    let slot = fixture.start(&reader.db).await?;
    let frozen = reader
        .freeze_owned_in(fixture.tx(slot)?, FROZEN, &|| Ok(()))
        .await?;
    ensure!(frozen.entries.len() == 1 && frozen.entries[0].result_id == logical);
    ensure!(frozen.frozen_at == FROZEN);
    fixture.commit(slot).await?;
    let manifest_before = reader
        .snapshots()
        .one(&reader.db)
        .await?
        .context("committed manifest")?;
    ensure!(manifest_before.creator_principal_id.as_deref() == Some("principal"));

    let slot = fixture.start(&reviewer.db).await?;
    let conflict = reviewer
        .submit_owned_in(
            fixture.tx(slot)?,
            &evaluation(&reviewer, EvalVerdict::Fail, "fail")?,
            &|| Ok(()),
        )
        .await?;
    ensure!(!conflict.duplicate && conflict.status == AdmissionStatus::Disputed);
    fixture.commit(slot).await?;
    database::close(&reader.db).await?;

    let fresh = reconnect(fixture, &first).await?;
    let events = fresh
        .admissions()
        .order_by_asc(admission_entity::Column::Sequence)
        .all(&fresh.db)
        .await?;
    ensure!(events.len() == 3 && events[0] == admissions_before[0]);
    for event in &events[1..] {
        ensure!(
            event.status == "disputed" && event.creator_principal_id.as_deref() == Some("reviewer")
        );
        ensure!(event.authority_id == "reviewer");
    }
    let latest = fresh.latest_admissions().await?;
    ensure!(
        latest.get(&logical).context("old result admission")?.status == AdmissionStatus::Disputed
    );
    ensure!(
        latest
            .get(&conflict.result_id)
            .context("new result admission")?
            .status
            == AdmissionStatus::Disputed
    );
    ensure!(fresh.subjects().one(&fresh.db).await? == Some(subject_before));
    ensure!(
        fresh
            .results()
            .filter(result_entity::Column::ResultId.eq(&result_before.result_id))
            .one(&fresh.db)
            .await?
            == Some(result_before)
    );
    let new_row = fresh
        .results()
        .filter(
            result_entity::Column::ResultId
                .eq(fresh.key(KeyDomain::EvalResult, &conflict.result_id)?),
        )
        .one(&fresh.db)
        .await?
        .context("new result")?;
    ensure!(new_row.creator_principal_id.as_deref() == Some("reviewer"));
    ensure!(fresh.snapshots().one(&fresh.db).await? == Some(manifest_before.clone()));
    let results_before_replay = fresh
        .results()
        .order_by_asc(result_entity::Column::ResultId)
        .all(&fresh.db)
        .await?;
    let slot = fixture.start(&fresh.db).await?;
    ensure!(
        fresh
            .snapshot_owned_in(fixture.tx(slot)?, &frozen.evidence_root, &|| Ok(()))
            .await?
            == frozen
    );
    let replay = fresh
        .submit_owned_in(
            fixture.tx(slot)?,
            &evaluation(&fresh, EvalVerdict::Pass, "pass")?,
            &|| Ok(()),
        )
        .await?;
    ensure!(replay.duplicate && replay.status == AdmissionStatus::Disputed);
    ensure!(fresh.snapshots().one(fixture.tx(slot)?).await? == Some(manifest_before));
    ensure!(
        fresh
            .results()
            .order_by_asc(result_entity::Column::ResultId)
            .all(fixture.tx(slot)?)
            .await?
            == results_before_replay
    );
    ensure!(
        fresh
            .admissions()
            .order_by_asc(admission_entity::Column::Sequence)
            .all(fixture.tx(slot)?)
            .await?
            == events
    );
    fixture.rollback(slot).await?;
    ensure!(
        fresh
            .admissions()
            .order_by_asc(admission_entity::Column::Sequence)
            .all(&fresh.db)
            .await?
            == events
    );

    // Identical logical IDs remain distinct from another tenant and Legacy.
    let foreign = scoped(&fresh.db)?;
    let slot = fixture.start(&foreign.db).await?;
    ensure!(matches!(
        foreign
            .snapshot_owned_in(fixture.tx(slot)?, &frozen.evidence_root, &|| Ok(()))
            .await,
        Err(OwnedError::Rejected(ErrorCode::NotFound))
    ));
    foreign
        .put_owned_in(fixture.tx(slot)?, &tests::subject()?, &|| Ok(()))
        .await?;
    fixture.commit(slot).await?;
    ensure!(
        foreign.key(KeyDomain::EvalSubject, "eval-1")?
            != fresh.key(KeyDomain::EvalSubject, "eval-1")?
    );
    let legacy = EvalStore::new(fresh.db.clone());
    legacy.insert_subject(&tests::subject()?).await?;
    ensure!(legacy.list_subjects().await?.len() == 1);
    ensure!(
        legacy
            .snapshot_by_root(&frozen.evidence_root)
            .await?
            .is_none()
    );
    let slot = fixture.start(&fresh.db).await?;
    let status = fresh.status_owned_in(fixture.tx(slot)?, &|| Ok(())).await?;
    ensure!(status.subjects == 1 && status.results == 2);
    let foreign_status = foreign
        .status_owned_in(fixture.tx(slot)?, &|| Ok(()))
        .await?;
    ensure!(foreign_status.subjects == 1 && foreign_status.results == 0);
    fixture.rollback(slot).await
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE privileges"]
async fn server_owned_eval_external_rr_winner_is_rejected_without_replay_or_events() -> Result<()> {
    run(|fixture| Box::pin(external_writer(fixture))).await
}

async fn external_writer(fixture: &mut Fixture) -> Result<()> {
    let left = initial(fixture).await?;
    let slot = fixture.start(&left.db).await?;
    left.put_owned_in(fixture.tx(slot)?, &tests::subject()?, &|| Ok(()))
        .await?;
    fixture.commit(slot).await?;
    let right = reconnect(fixture, &left).await?;
    let input = evaluation(&left, EvalVerdict::Pass, "external-winner")?;
    let old = fixture.start(&left.db).await?;
    // This nonlocking read establishes A's RR snapshot before B's commit. The
    // two live transactions overlap; no timing sleeps or synthetic SQL errors.
    let before = left.status_owned_in(fixture.tx(old)?, &|| Ok(())).await?;
    ensure!(before.subjects == 1 && before.results == 0);
    let winner = fixture.start(&right.db).await?;
    let accepted = right
        .submit_owned_in(fixture.tx(winner)?, &input, &|| Ok(()))
        .await?;
    ensure!(!accepted.duplicate);
    fixture.commit(winner).await?;
    ensure!(
        left.status_owned_in(fixture.tx(old)?, &|| Ok(()))
            .await?
            .results
            == 0
    );
    let rows_before = right.results().all(&right.db).await?;
    let events_before = right
        .admissions()
        .order_by_asc(admission_entity::Column::Sequence)
        .all(&right.db)
        .await?;
    ensure!(rows_before.len() == 1 && events_before.len() == 1);
    let outcome = left
        .submit_owned_in(fixture.tx(old)?, &input, &|| Ok(()))
        .await;
    // Roll back even when an assertion about the precise backend error fails.
    fixture.rollback(old).await?;
    match (fixture.backend(), outcome) {
        (DatabaseBackend::Postgres, Err(OwnedError::Storage(error))) => {
            ensure!(
                fault::sql_code(&error).as_deref() == Some("40001"),
                "expected real RR serialization failure: {error:#}"
            );
        }
        (DatabaseBackend::MySql, Err(OwnedError::Integrity(error))) => {
            ensure!(
                error
                    .to_string()
                    .contains("unexpected concurrent result insertion"),
                "expected validated MySQL winner rejection: {error:#}"
            );
        }
        (_, other) => {
            anyhow::bail!("external writer was not rejected by the expected RR path: {other:?}")
        }
    }
    let fresh = reconnect(fixture, &left).await?;
    ensure!(fresh.results().all(&fresh.db).await? == rows_before);
    ensure!(
        fresh
            .admissions()
            .order_by_asc(admission_entity::Column::Sequence)
            .all(&fresh.db)
            .await?
            == events_before
    );
    // A new snapshot can replay the committed winner, without repairing or appending.
    let slot = fixture.start(&fresh.db).await?;
    let replay = fresh
        .submit_owned_in(fixture.tx(slot)?, &input, &|| Ok(()))
        .await?;
    ensure!(
        replay.duplicate
            && replay.result_id == accepted.result_id
            && replay.status == accepted.status
    );
    ensure!(fresh.results().all(fixture.tx(slot)?).await? == rows_before);
    ensure!(
        fresh
            .admissions()
            .order_by_asc(admission_entity::Column::Sequence)
            .all(fixture.tx(slot)?)
            .await?
            == events_before
    );
    fixture.rollback(slot).await?;
    ensure!(
        fresh
            .admissions()
            .order_by_asc(admission_entity::Column::Sequence)
            .all(&fresh.db)
            .await?
            == events_before
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE and trigger privileges"]
async fn server_owned_eval_terminal_fault_rolls_back_result_and_prior_dispute() -> Result<()> {
    run(|fixture| Box::pin(partial_failure(fixture))).await
}

async fn partial_failure(fixture: &mut Fixture) -> Result<()> {
    let store = initial(fixture).await?;
    let prior = seed(fixture, &store).await?;
    let input = evaluation(&store, EvalVerdict::Fail, "terminal-fault")?;
    let logical = crate::storage::evaluation::result_digest(&input)?;
    let rows_before = store.results().all(&store.db).await?;
    let events_before = store
        .admissions()
        .order_by_asc(admission_entity::Column::Sequence)
        .all(&store.db)
        .await?;
    fault::install(&store, &prior, &logical).await?;
    let slot = fixture.start(&store.db).await?;
    let outcome = store
        .submit_owned_in(fixture.tx(slot)?, &input, &|| Ok(()))
        .await;
    fixture.rollback(slot).await?;
    let Err(OwnedError::Storage(error)) = outcome else {
        anyhow::bail!("terminal trigger did not produce a storage error: {outcome:?}");
    };
    let expected = match fixture.backend() {
        DatabaseBackend::Postgres => "P0001",
        DatabaseBackend::MySql => "45000",
        _ => anyhow::bail!("unsupported fixture backend"),
    };
    ensure!(
        fault::sql_code(&error).as_deref() == Some(expected),
        "wrong trigger SQLSTATE: {error:#}"
    );
    ensure!(
        format!("{error:#}").contains(fault::MARKER),
        "fault fired before its prior-dispute/result preconditions: {error:#}"
    );
    let fresh = reconnect(fixture, &store).await?;
    ensure!(fresh.results().all(&fresh.db).await? == rows_before);
    ensure!(
        fresh
            .admissions()
            .order_by_asc(admission_entity::Column::Sequence)
            .all(&fresh.db)
            .await?
            == events_before
    );
    ensure!(fresh.result(&logical).await?.is_none());
    ensure!(
        fresh
            .latest_admissions()
            .await?
            .get(&prior)
            .context("prior admission")?
            .status
            == AdmissionStatus::Admitted
    );
    fault::remove(&store).await?;
    // Both explicit rollback and trigger removal are real: the same connection
    // now commits the complete result plus the two dispute events.
    let slot = fixture.start(&store.db).await?;
    let accepted = store
        .submit_owned_in(fixture.tx(slot)?, &input, &|| Ok(()))
        .await?;
    ensure!(!accepted.duplicate && accepted.status == AdmissionStatus::Disputed);
    fixture.commit(slot).await?;
    ensure!(fresh.results().all(&fresh.db).await?.len() == 2);
    ensure!(fresh.admissions().all(&fresh.db).await?.len() == 3);
    Ok(())
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE privileges"]
async fn server_owned_eval_freeze_keeps_rr_membership_across_external_admission_commit()
-> Result<()> {
    run(|fixture| Box::pin(stable_freeze(fixture))).await
}

async fn stable_freeze(fixture: &mut Fixture) -> Result<()> {
    let left = initial(fixture).await?;
    let prior = seed(fixture, &left).await?;
    let right = reconnect(fixture, &left).await?;
    let old = fixture.start(&left.db).await?;
    let before = left.status_owned_in(fixture.tx(old)?, &|| Ok(())).await?;
    ensure!(before.admission.get("admitted") == Some(&1));
    let newer = fixture.start(&right.db).await?;
    let outcome = right
        .submit_owned_in(
            fixture.tx(newer)?,
            &evaluation(&right, EvalVerdict::Fail, "changed-membership")?,
            &|| Ok(()),
        )
        .await?;
    ensure!(outcome.status == AdmissionStatus::Disputed);
    fixture.commit(newer).await?;
    let old_snapshot = left
        .freeze_owned_in(fixture.tx(old)?, FROZEN, &|| Ok(()))
        .await?;
    ensure!(old_snapshot.entries.len() == 1 && old_snapshot.entries[0].result_id == prior);
    fixture.commit(old).await?;

    let fresh = reconnect(fixture, &left).await?;
    let slot = fixture.start(&fresh.db).await?;
    let status = fresh.status_owned_in(fixture.tx(slot)?, &|| Ok(())).await?;
    ensure!(status.results == 2 && status.admission.get("disputed") == Some(&2));
    ensure!(
        fresh
            .snapshot_owned_in(fixture.tx(slot)?, &old_snapshot.evidence_root, &|| Ok(()))
            .await?
            == old_snapshot
    );
    // The same timestamp isolates membership as the reason the roots differ.
    let new_snapshot = fresh
        .freeze_owned_in(fixture.tx(slot)?, FROZEN, &|| Ok(()))
        .await?;
    ensure!(new_snapshot.entries.is_empty());
    ensure!(new_snapshot.frozen_at == old_snapshot.frozen_at);
    ensure!(new_snapshot.evidence_root != old_snapshot.evidence_root);
    fixture.commit(slot).await?;
    database::close(&fresh.db).await?;
    let restarted = reconnect(fixture, &left).await?;
    let slot = fixture.start(&restarted.db).await?;
    for snapshot in [&old_snapshot, &new_snapshot] {
        ensure!(
            restarted
                .snapshot_owned_in(fixture.tx(slot)?, &snapshot.evidence_root, &|| Ok(()))
                .await?
                == *snapshot
        );
    }
    fixture.rollback(slot).await
}
