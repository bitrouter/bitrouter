use super::*;
use crate::{tenancy::transaction::Disposition, trajectory::store::TrajectoryStore};
use harness::Harness;
use rows::Snapshot;

pub(super) async fn rollback(database: &mut database::Fixture, point: fault::Point) -> Result<()> {
    let harness = Harness::begin(database).await?;
    let before = Snapshot::read(&harness.fixture.db).await?;
    before.started()?;
    before.owned_by(&harness.fixture)?;
    let events = before.count("trajectory_events")?;
    fault::install(&harness.fixture.db, point, events).await?;
    let store = TrajectoryStore::for_execution(&harness.execution);
    let slot = database.start(store.database()).await?;
    let failed = harness.terminal_in(database.tx(slot)?).await;
    let inspected = async {
        let error = failed
            .err()
            .context("terminal unexpectedly bypassed the real fault trigger")?;
        fault::verify(&error, database.backend())?;
        if database.backend() == DatabaseBackend::MySql {
            // SIGNAL only rolls back the failing statement. These earlier
            // writes must be present INSIDE this transaction, before rollback.
            let partial = Snapshot::read(database.tx(slot)?).await?;
            partial.owned_by(&harness.fixture)?;
            ensure!(partial.count("trajectory_events")? == events + 1);
            ensure!(partial.cell("trajectory_requests", "status")? == Some("failed"));
            for (table, expected) in [
                "trajectory_outbox",
                "eval_subjects",
                "eval_results",
                "eval_admission_events",
            ]
            .into_iter()
            .zip(point.preceding())
            {
                ensure!(
                    partial.count(table)? == expected,
                    "wrong predecessor state in {table} at {point:?}"
                );
            }
            if partial.count("trajectory_outbox")? == 1 {
                ensure!(partial.cell("trajectory_outbox", "delivered_at")?.is_none());
                ensure!(partial.cell("trajectory_outbox", "attempts")? == Some("0"));
            }
        }
        // PostgreSQL cannot query its now-aborted transaction. The trigger's
        // P0001+marker instead proves every predecessor check passed inside it.
        Ok::<_, anyhow::Error>(())
    }
    .await;
    database.rollback(slot).await?;
    inspected?;
    ensure!(
        !harness.observation.confirmed(),
        "borrowed failure acknowledged the obligation"
    );
    let fresh = database.connect().await?;
    ensure!(
        Snapshot::read(&fresh).await? == before,
        "partial compound closure survived explicit rollback at {point:?}"
    );

    // Exercise the real recorder -> actor -> H rollback path with the same fault
    // still installed, not only the transaction-borrowing API above.
    ensure!(
        harness.record().await.is_err(),
        "actor ignored terminal fault at {point:?}"
    );
    ensure!(harness.fixture.service.cleanup.disposition() == Disposition::RolledBack);
    ensure!(!harness.observation.confirmed());
    ensure!(
        Snapshot::read(&fresh).await? == before,
        "actor left partial terminal state at {point:?}"
    );
    fault::remove(&harness.fixture.db, point).await?;
    let original = harness.fixture.db.clone();
    harness.finish(true).await?;
    database.quiesce().await?;
    database::close(&original).await?;
    database::close(&fresh).await?;
    let reopened = database.connect().await?;
    ensure!(
        Snapshot::read(&reopened).await? == before,
        "reopen changed rolled-back closure"
    );
    Ok(())
}

pub(super) async fn replay(database: &mut database::Fixture) -> Result<()> {
    let harness = Harness::begin(database).await?;
    let start = Snapshot::read(&harness.fixture.db).await?;
    start.started()?;
    harness.record().await?;
    ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Committed);
    let complete = Snapshot::read(&harness.fixture.db).await?;
    complete.terminal(start.count("trajectory_events")?)?;
    complete.owned_by(&harness.fixture)?;

    let store = TrajectoryStore::for_execution(&harness.execution);
    let slot = database.start(store.database()).await?;
    let inspected = async {
        let first = harness.terminal_in(database.tx(slot)?).await?;
        let second = harness.terminal_in(database.tx(slot)?).await?;
        ensure!(first == second, "exact replay changed canonical closure");
        // Include raw subject/result creator, immutable JSON, admission sequence,
        // episode head, event rows and delivery timestamp BEFORE rollback.
        ensure!(
            Snapshot::read(database.tx(slot)?).await? == complete,
            "duplicate wrote inside its still-open transaction"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    database.rollback(slot).await?;
    inspected?;
    let fresh = database.connect().await?;
    ensure!(Snapshot::read(&fresh).await? == complete);
    // Legacy consumers remain unable to see this actual owned publication.
    ensure!(
        crate::core::eval::store::EvalStore::new(fresh.clone())
            .list_subjects()
            .await?
            .is_empty()
    );
    ensure!(
        crate::trajectory::store::TrajectoryStore::new(fresh.clone())
            .pending_outbox(&store.owner()?)
            .await?
            .is_empty()
    );
    let original = harness.fixture.db.clone();
    drop(store);
    harness.finish(false).await?;
    database.quiesce().await?;
    database::close(&original).await?;
    database::close(&fresh).await?;
    let reopened = database.connect().await?;
    ensure!(
        Snapshot::read(&reopened).await? == complete,
        "committed closure not durable across complete pool reopen"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Damage {
    MissingOutbox,
    MissingTerminal,
    MissingAdmission,
    BadOutbox,
    BadTerminal,
    BadAdmission,
    Undelivered,
    BadSubject,
    BadResult,
    MissingSubject,
    MissingResult,
}
impl Damage {
    pub(super) const ALL: [Self; 11] = [
        Self::MissingOutbox,
        Self::MissingTerminal,
        Self::MissingAdmission,
        Self::BadOutbox,
        Self::BadTerminal,
        Self::BadAdmission,
        Self::Undelivered,
        Self::BadSubject,
        Self::BadResult,
        Self::MissingSubject,
        Self::MissingResult,
    ];

    fn sql(self) -> &'static str {
        match self {
            Self::MissingOutbox => "DELETE FROM trajectory_outbox",
            Self::MissingTerminal => "DELETE FROM trajectory_events WHERE kind='request_settled'",
            Self::MissingAdmission => "DELETE FROM eval_admission_events",
            Self::BadOutbox => "UPDATE trajectory_outbox SET payload_json='{}'",
            Self::BadTerminal => {
                "UPDATE trajectory_events SET event_json='{}' WHERE kind='request_settled'"
            }
            Self::BadAdmission => "UPDATE eval_admission_events SET status='fixture-invalid'",
            Self::Undelivered => "UPDATE trajectory_outbox SET delivered_at=NULL",
            Self::BadSubject => "UPDATE eval_subjects SET subject_json='{}'",
            Self::BadResult => "UPDATE eval_results SET result_json='{}'",
            Self::MissingSubject => "DELETE FROM eval_subjects",
            Self::MissingResult => "DELETE FROM eval_results",
        }
    }
}

pub(super) async fn damaged(database: &mut database::Fixture, damage: Damage) -> Result<()> {
    let harness = Harness::begin(database).await?;
    harness.record().await?;
    let original = Snapshot::read(&harness.fixture.db).await?;
    let store = TrajectoryStore::for_execution(&harness.execution);
    let slot = database.start(store.database()).await?;
    let changed = database.tx(slot)?.execute_unprepared(damage.sql()).await?;
    ensure!(
        changed.rows_affected() == 1,
        "damage did not affect exactly one closure row: {damage:?}"
    );
    database.commit(slot).await?;
    let fresh = database.connect().await?;
    let damaged = Snapshot::read(&fresh).await?;
    ensure!(damaged != original, "damage was not committed");

    let slot = database.start(store.database()).await?;
    let inspected = async {
        let outcome = harness.terminal_in(database.tx(slot)?).await;
        ensure!(
            outcome.is_err(),
            "absent/malformed closure replay was accepted: {damage:?}"
        );
        // This deliberately catches tentative INSERT/UPDATE repair too. A later
        // rollback cannot hide a mutation by the supposed duplicate-only branch.
        ensure!(
            Snapshot::read(database.tx(slot)?).await? == damaged,
            "rejected replay attempted repair inside tx: {damage:?}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    database.rollback(slot).await?;
    inspected?;
    ensure!(
        Snapshot::read(&fresh).await? == damaged,
        "replay repaired durable damaged closure: {damage:?}"
    );
    let old_pool = harness.fixture.db.clone();
    drop(store);
    harness.finish(false).await?;
    database.quiesce().await?;
    database::close(&old_pool).await?;
    database::close(&fresh).await?;
    let reopened = database.connect().await?;
    ensure!(
        Snapshot::read(&reopened).await? == damaged,
        "reopen hid a replay repair"
    );
    Ok(())
}
