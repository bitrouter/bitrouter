//! Bounded mechanism observations, not release throughput measurements.
use super::*;
use crate::tenancy::{ownership, transaction::receipt_observation::Key};
use sea_orm::{ConnectOptions, ConnectionTrait, Database};
use std::{os::unix::fs::PermissionsExt, sync::atomic::AtomicUsize};

#[derive(Default)]
struct Statements([AtomicUsize; 6]);

impl Statements {
    fn record(&self, info: &sea_orm::metric::Info<'_>) {
        // Keep counts only: never retain SQL text, parameters or credential data.
        let index = match info.statement.sql.split_ascii_whitespace().next() {
            Some("SELECT") => 0,
            Some("INSERT") => 1,
            Some("UPDATE") => 2,
            Some("DELETE") => 3,
            _ => 4,
        };
        self.0[index].fetch_add(1, Ordering::Relaxed);
        if info.failed {
            self.0[5].fetch_add(1, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> [usize; 6] {
        std::array::from_fn(|index| self.0[index].load(Ordering::Relaxed))
    }
}

#[tokio::test]
async fn warm_admission_and_terminal_have_separate_sql_proof_and_volume_observations() -> Result<()>
{
    let directory = tempfile::Builder::new()
        .prefix("bitrouter-warm-cost-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let path = directory.path().join("authority.sqlite");
    let lease = ownership::acquire_path(&path, true)?;
    let mut options = ConnectOptions::new(format!("sqlite://{}?mode=rwc", path.display()));
    options
        .min_connections(1)
        .max_connections(1)
        .sqlx_logging(false);
    let mut db = Database::connect(options).await?;
    db.execute_unprepared("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .await?;
    let statements = Arc::new(Statements::default());
    let observed = statements.clone();
    db.set_metric_callback(move |info| observed.record(info));
    let fixture = Fixture::with_database(db.clone(), &mut |_| {}).await?;
    let ready = Ready::with_fixture(fixture, false, Some(lease)).await?;
    let service = &ready.fixture.service;
    let outcome: Result<()> = async {
        let first = ready.execution().await?;
        let owner = first.receipt_owner()?.clone();
        tokio::time::timeout(TIMEOUT, owner.finish(Outcome::Failed))
            .await?
            .map_err(code)?;
        owner.release_active().map_err(code)?;
        drop((owner, first));
        ensure!(statements.snapshot()[0] > 0, "cold driver observation missing");

        let sql_before = statements.snapshot();
        let proof_before = service.cleanup.sql_counts_for_test();
        let jobs_before = service.authority.read_jobs.load(Ordering::Relaxed);
        let probes_before = service.admission.volume_probes_for_test().map_err(code)?;
        ensure!(probes_before.0 > 0 && probes_before.1 == 0);
        let warm = ready.execution().await?;
        let sql_admitted = statements.snapshot();
        let probes_admitted = service.admission.volume_probes_for_test().map_err(code)?;
        ensure!(sql_admitted == sql_before, "warm admission performed SQL");
        ensure!(service.cleanup.sql_counts_for_test() == proof_before);
        ensure!(service.authority.read_jobs.load(Ordering::Relaxed) == jobs_before + 1);
        ensure!(probes_admitted.0 > probes_before.0 && probes_admitted.1 == probes_before.1);
        eprintln!(
            "warm_cost phase=admission SQL=0 BEGIN=0 ROLLBACK=0 AUTHORITY_JOB=1 bound_volume_refreshes={} probe_errors=0",
            probes_admitted.0 - probes_before.0,
        );

        let owner = warm.receipt_owner()?.clone();
        service.cleanup.observe_receipt_for_test(Key {
            installation: ready.fixture.installation.clone(),
            tenant: ready.fixture.tenant.clone(),
            execution: warm.id().clone(),
        })?;
        tokio::time::timeout(TIMEOUT, owner.finish(Outcome::Failed))
            .await?
            .map_err(code)?;
        let sql_terminal = statements.snapshot();
        let probes_terminal = service.admission.volume_probes_for_test().map_err(code)?;
        let trace = service.cleanup.receipt_trace_for_test()?;
        let fact = trace.facts[0].as_ref().context("terminal SQL observation missing")?;
        ensure!(!trace.overflow && trace.facts[1].is_none());
        ensure!(fact.begin.is_some_and(|outcome| outcome.ok));
        ensure!(fact.store.is_some_and(|outcome| outcome.ok) && fact.inserted == Some(true));
        ensure!(fact.commit.is_some_and(|outcome| outcome.ok) && fact.rollback.is_none());
        ensure!(!fact.check_failed && fact.disposition == crate::tenancy::transaction::Disposition::Committed);
        ensure!(service.cleanup.sql_counts_for_test() == (proof_before.0 + 1, proof_before.1));
        ensure!(service.authority.read_jobs.load(Ordering::Relaxed) == jobs_before + 1);
        ensure!(sql_terminal[0] > sql_admitted[0] && sql_terminal[1] > sql_admitted[1]);
        ensure!(sql_terminal[5] == sql_admitted[5]);
        ensure!(probes_terminal.0 > probes_admitted.0 && probes_terminal.1 == probes_admitted.1);
        let delta: [usize; 6] = std::array::from_fn(|index| sql_terminal[index] - sql_admitted[index]);
        eprintln!(
            "warm_cost phase=terminal_failed_no_provider SQL_SELECT_INSERT_UPDATE_DELETE_OTHER_FAILED={delta:?} BEGIN=1 ACK_COMMIT=1 ACK_ROLLBACK=0 RECEIPT_ATTEMPT=1 bound_volume_refreshes={} probe_errors=0",
            probes_terminal.0 - probes_admitted.0,
        );

        // Outside both observation windows: confirm the actual canonical durable row.
        let saved = crate::tenancy::receipts::store::lookup_candidate(
            &db,
            &ready.fixture.installation,
            &ready.fixture.tenant,
            warm.id(),
        )
        .await?
        .context("terminal receipt missing")?;
        ensure!(saved.receipt.outcome == Outcome::Failed && !saved.unknown());
        ensure!(saved.digest == fact.fingerprint.digest && owner.durable());
        owner.release_active().map_err(code)?;
        drop((owner, warm));
        Ok(())
    }
    .await;
    let stopped = service.shutdown().await;
    let closed = db.close().await;
    let result = outcome.and(stopped).and(closed.map_err(Into::into));
    if result.is_err() {
        let retained = directory.keep();
        eprintln!("warm cost fixture retained at {}", retained.display());
    }
    result
}
