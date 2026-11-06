//! Finite samples of actual local writers, not a SQLite allocation theorem.
//! One sequential test keeps scratch bounded; archive reuse is tested elsewhere.
use super::measurement::{Sample, checkpoint, sample};
use super::*;
use anyhow::{Context, ensure};
use futures::FutureExt;
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseBackend, Statement};
use std::{os::unix::fs::MetadataExt, path::PathBuf};

use crate::{
    core::evolution::jobs::{JobStatus, JudgeJob, JudgeJobs},
    gateway::routing::blocks::{
        allocation::BanditConfig,
        control::{BlockDefinition, BlockRule, ControlState},
        store::{CONTROL_KEY, CONTROL_KIND},
    },
    metering::{
        db::{ReconciliationStatus, RequestMetric},
        entities::requests,
        pricing::{ChargeStatus, unavailable_charge_evidence},
        store::MeteringStore,
    },
    storage::{evolution::EvolutionStore, session_identity::SessionIdentity},
    tenancy::{Service, storage::estimate},
};

const MIB: u64 = 1024 * 1024;
const FLOOR: u64 = 8 * MIB;

fn sql(text: impl Into<String>) -> Statement {
    Statement::from_string(DatabaseBackend::Sqlite, text.into())
}

fn observation(budget: &Budget, free: Option<u64>) -> Result<u64> {
    let observed = budget
        .calibration_observation(free)
        .map_err(|e| anyhow::anyhow!("calibration accounting unavailable: {e:?}"))?;
    ensure!(
        observed.observed_free.is_none(),
        "pressure injection requires an unbound test volume"
    );
    Ok(observed.physical_write_reserved)
}

fn physical_estimate() -> Result<u64> {
    let result = estimate(u64::try_from(MAX_BYTES)?).map_err(capacity_error)?;
    ensure!(result == 256 * MIB + 64 * 1024);
    Ok(result)
}

struct Fixture {
    directory: tempfile::TempDir,
    path: PathBuf,
    db: DatabaseConnection,
    service: Service,
    cell: Arc<OnceLock<Budget>>,
    wal: bool,
}
impl Fixture {
    async fn new(mode: &str) -> Result<Self> {
        ensure!(matches!(mode, "WAL" | "DELETE"));
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("writer-calibration.sqlite");
        let mut options = ConnectOptions::new(format!("sqlite://{}?mode=rwc", path.display()));
        options
            .min_connections(1)
            .max_connections(1)
            .sqlx_logging(false);
        let db = Database::connect(options).await?;
        db.execute_unprepared("PRAGMA page_size=4096; PRAGMA auto_vacuum=NONE; PRAGMA synchronous=FULL; PRAGMA busy_timeout=1000;").await?;
        let actual: String = db
            .query_one(sql(format!("PRAGMA journal_mode={mode}")))
            .await?
            .context("journal mode")?
            .try_get_by_index(0)?;
        ensure!(actual.eq_ignore_ascii_case(mode));
        // Cap the main DB at 64 MiB. The fixed workload uses at most two ~11 MiB
        // control rows and checkpoints between transactions; never fill a host disk.
        let cap: i64 = db
            .query_one(sql("PRAGMA max_page_count=16384"))
            .await?
            .context("page limit")?
            .try_get_by_index(0)?;
        ensure!(cap == 16384);
        db.execute_unprepared("PRAGMA wal_autocheckpoint=0").await?;
        crate::db::run_migrations(&db).await?;
        let mut config = crate::config::tenancy::TenancyConfig::default();
        config.capacity.receipts.admission_free_bytes = 16 * MIB;
        config.capacity.receipts.maintenance_free_bytes = FLOOR;
        let service = Service::open_profile(
            db.clone(),
            None,
            &config,
            &crate::config::ContinuationConfig::default(),
        )
        .await?;
        let cell = Arc::new(OnceLock::new());
        cell.set(service.storage_budget())
            .map_err(|_| anyhow::anyhow!("budget cell already set"))?;
        let fixture = Self {
            directory,
            path,
            db,
            service,
            cell,
            wal: mode == "WAL",
        };
        fixture.settle_checkpoint().await?;
        if fixture.wal {
            ensure!(checkpoint(&fixture.db, "TRUNCATE").await? == (0, 0, 0));
        }
        let version: String = fixture
            .db
            .query_one(sql("SELECT sqlite_version() AS version"))
            .await?
            .context("SQLite version")?
            .try_get("", "version")?;
        let metadata = std::fs::metadata(&fixture.path)?;
        eprintln!(
            "writer calibration sqlite={version} journal={actual} os={} arch={} device={} io_block={} baseline={:?}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            metadata.dev(),
            metadata.blksize(),
            sample(&fixture.db, &fixture.path).await?
        );
        Ok(fixture)
    }
    fn store(&self, owner: &str) -> Result<EvolutionStore> {
        Ok(EvolutionStore::new(self.db.clone(), owner)?
            .with_storage_budget(Some(self.cell.clone())))
    }
    async fn settle_checkpoint(&self) -> Result<i64> {
        if !self.wal {
            return Ok(0);
        }
        let (busy, frames, complete) = checkpoint(&self.db, "PASSIVE").await?;
        ensure!(
            busy == 0 && frames >= 0 && complete == frames,
            "incomplete checkpoint: {busy}/{frames}/{complete}"
        );
        Ok(frames)
    }
    async fn close(self, result: Result<()>, quarantined: bool) -> Result<()> {
        let stopped = self.service.shutdown().await;
        drop(self.service);
        let closed = self.db.close().await;
        let removed = self.directory.close();
        result?;
        ensure!(
            stopped.is_err() == quarantined,
            "unexpected shutdown custody: {stopped:?}"
        );
        closed?;
        removed?;
        Ok(())
    }
}

fn control(bytes: usize) -> Result<ControlState> {
    let mut result = ControlState::default();
    result.register(
        BlockDefinition {
            block_id: "calibration".into(),
            source: "fixture".into(),
            rationale: "r".repeat(bytes),
            rules: vec![BlockRule {
                selector: "baseline".into(),
                fingerprint: None,
                baseline_route: "baseline".into(),
                challenger_route: "challenger".into(),
            }],
            independence_rationale: "single block".into(),
            dependencies: Default::default(),
            measurement_contract: "fixture".into(),
            batch_sessions: 2,
            bandit: BanditConfig::default(),
        },
        "fixture-config".into(),
    )?;
    Ok(result)
}

async fn record_sample(
    f: &Fixture,
    label: &str,
    before: &Sample,
    pending: &Sample,
    spent: usize,
) -> Result<()> {
    let settled = sample(&f.db, &f.path).await?;
    let frames = f.settle_checkpoint().await?;
    let after = sample(&f.db, &f.path).await?;
    let growth = |field: fn(&Sample) -> &[u64; 4]| -> u64 {
        (0..4)
            .map(|i| {
                [pending, &settled, &after]
                    .into_iter()
                    .map(|s| field(s)[i])
                    .max()
                    .unwrap_or(0)
                    .saturating_sub(field(before)[i])
            })
            .sum()
    };
    let lengths = growth(|s| &s.lengths);
    let allocated = growth(|s| &s.allocated);
    // Frame traffic catches WAL reuse hidden by lengths. This remains a sample,
    // not a claim to observe transient peaks between these points.
    let frame_bytes = u64::try_from(frames)?
        .checked_mul(after.page_size + 24)
        .and_then(|n| n.checked_add(if f.wal { 32 } else { 0 }))
        .and_then(|n| n.checked_add(after.lengths[0].saturating_sub(before.lengths[0])))
        .context("frame bytes")?;
    let estimate = physical_estimate()?;
    eprintln!(
        "writer calibration case={label} E={MAX_BYTES} estimate={estimate} charged_program_bytes={spent} length_growth={lengths} allocated_growth={allocated} frame_and_main_bytes={frame_bytes} before={before:?} pending={pending:?} settled={settled:?} checkpointed={after:?}"
    );
    ensure!(
        lengths.max(allocated).max(frame_bytes) <= estimate,
        "sample exceeds production estimate"
    );
    ensure!(observation(&f.service.storage_budget(), None)? == 0);
    if f.wal {
        ensure!(checkpoint(&f.db, "TRUNCATE").await? == (0, 0, 0));
    }
    Ok(())
}

async fn sizes_and_rollback(f: &Fixture) -> Result<()> {
    for (label, size) in [
        ("small", 128),
        ("near-program-bound", (MAX_BYTES - 64 * 1024) / 3),
    ] {
        let store = f.store(label)?;
        let before = sample(&f.db, &f.path).await?;
        let (mut write, tx) = store.begin_write(false).await?;
        ensure!(observation(&f.service.storage_budget(), None)? == physical_estimate()?);
        let inserted = store
            .initialize_charged(
                &mut write,
                &tx,
                CONTROL_KIND,
                CONTROL_KEY,
                None,
                &control(size)?,
            )
            .await;
        let spent = MAX_BYTES - write.remaining;
        let pending = sample(&tx, &f.path).await?;
        write.finish(tx, inserted.map(|_| ())).await?;
        record_sample(f, &format!("{label}/insert"), &before, &pending, spent).await?;
        // The lock-only UPDATE charges the old row twice; replacement charges
        // old+new again. This shrinking transaction is near the real program cap.
        for commit in [false, true] {
            let before = sample(&f.db, &f.path).await?;
            let (mut write, tx) = store.begin_write(false).await?;
            let (row, mut state): (_, ControlState) = store
                .lock_charged(&mut write, &tx, CONTROL_KIND, CONTROL_KEY)
                .await?;
            state
                .blocks
                .get_mut("calibration")
                .context("control block")?
                .definition
                .rationale = "small".into();
            store.save_charged(&mut write, &tx, row, &state).await?;
            let spent = MAX_BYTES - write.remaining;
            if size > 128 {
                ensure!(spent >= MAX_BYTES - 128 * 1024);
            }
            ensure!(observation(&f.service.storage_budget(), None)? == physical_estimate()?);
            let pending = sample(&tx, &f.path).await?;
            let result = if commit {
                Ok(())
            } else {
                Err(anyhow::anyhow!("fixture business rollback"))
            };
            ensure!(write.finish(tx, result).await.is_ok() == commit);
            let (revision, saved): (_, ControlState) = store
                .get(CONTROL_KIND, CONTROL_KEY)
                .await?
                .context("saved control")?;
            ensure!(revision == i64::from(commit));
            ensure!(
                saved
                    .blocks
                    .get("calibration")
                    .context("saved block")?
                    .definition
                    .rationale
                    .len()
                    == if commit { 5 } else { size }
            );
            record_sample(
                f,
                &format!("{label}/shrink/commit={commit}"),
                &before,
                &pending,
                spent,
            )
            .await?;
        }
    }
    Ok(())
}

async fn pinned_reader(f: &Fixture) -> Result<()> {
    let store = f.store("pinned-reader")?;
    store
        .initialize("fixture", "row", None, &"before".to_owned())
        .await?;
    ensure!(checkpoint(&f.db, "TRUNCATE").await? == (0, 0, 0));
    let mut options = ConnectOptions::new(format!("sqlite://{}?mode=ro", f.path.display()));
    options
        .min_connections(1)
        .max_connections(1)
        .sqlx_logging(false);
    let reader = Database::connect(options).await?;
    let tx = reader.begin().await?;
    ensure!(store.get_in::<String>(&tx, "fixture", "row").await? == Some((0, "before".into())));
    for revision in 0..2 {
        store
            .update("fixture", "row", revision, |body: &mut String| {
                *body = "p".repeat(128 * 1024);
                Ok(())
            })
            .await?;
    }
    let held = checkpoint(&f.db, "PASSIVE").await?;
    ensure!(
        held.0 == 0 && held.1 > held.2 && held.2 >= 0,
        "reader did not pin frames: {held:?}"
    );
    ensure!(store.get_in::<String>(&tx, "fixture", "row").await? == Some((0, "before".into())));
    let sample = sample(&f.db, &f.path).await?;
    ensure!(sample.lengths[1] > 0 && observation(&f.service.storage_budget(), None)? == 0);
    tx.rollback().await?;
    reader.close().await?;
    let released = f.settle_checkpoint().await?;
    ensure!(released >= held.1);
    ensure!(checkpoint(&f.db, "TRUNCATE").await? == (0, 0, 0));
    store
        .update("fixture", "row", 2, |body: &mut String| {
            *body = "after".into();
            Ok(())
        })
        .await?;
    ensure!(store.get::<String>("fixture", "row").await? == Some((3, "after".into())));
    eprintln!(
        "writer calibration pinned_reader={held:?} released_frames={released} files={sample:?}"
    );
    Ok(())
}

fn metric() -> RequestMetric {
    let usage = bitrouter_ai::types::Usage::default();
    RequestMetric {
        request_id: "calibration-metering".into(),
        user_id: "fixture".into(),
        api_key_id: "fixture".into(),
        launch_id: None,
        session_identity: None,
        router_id: None,
        binding_digest: None,
        original_selector: None,
        model_id: "fixture".into(),
        provider_id: "fixture".into(),
        prompt_tokens: 0,
        completion_tokens: 0,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        uncached_input_tokens: 0,
        output_tokens: 0,
        usage_origin: usage.origin,
        raw_usage: None,
        charge_status: ChargeStatus::Unknown,
        charge_evidence: unavailable_charge_evidence(&usage, "fixture"),
        reconciliation_status: ReconciliationStatus::Pending,
        estimated_charge_micro_usd: 0,
        latency_ms: 1,
        generation_time_ms: 1,
        streamed: false,
        error: None,
    }
}

async fn shared_pressure(f: &Fixture) -> Result<()> {
    use sea_orm::EntityTrait;
    let store = f.store("pressure")?;
    let job = JudgeJob {
        job_id: "retire".into(),
        identity: SessionIdentity {
            owner: "pressure".into(),
            source: "fixture".into(),
            native_session_id: "session".into(),
        },
        checkpoint_id: "checkpoint".into(),
        expected_revision: None,
        model: "fixture".into(),
        input_digest: "fixture".into(),
        mode_epoch: None,
        status: JobStatus::Queued,
        lease_owner: None,
        lease_until: None,
        request_ids: vec![],
        cached_response: None,
        outcome_revision: None,
        error_code: None,
    };
    store.initialize("judge_job", "retire", None, &job).await?;
    let budget = f.service.storage_budget();
    let amount = physical_estimate()?;
    observation(&budget, Some(amount + FLOOR))?;
    let (mut write, tx) = store.begin_write(false).await?;
    let _: (_, JudgeJob) = store
        .lock_charged(&mut write, &tx, "judge_job", "retire")
        .await?;
    ensure!(observation(&budget, None)? == amount);
    let metering = MeteringStore::new(f.db.clone()).with_storage_budget(f.cell.clone());
    // This must refuse before waiting on the one-connection pool occupied by tx.
    ensure!(matches!(
        metering
            .record_request(metric())
            .now_or_never()
            .context("metering refusal waited for SQL")?,
        Err(BitrouterError::RateLimited { .. })
    ));
    ensure!(matches!(
        budget.reserve(MAX_BYTES, true),
        Err(ErrorCode::CapacityExceeded)
    ));
    write.finish(tx, Ok(())).await?;
    ensure!(
        requests::Entity::find_by_id("calibration-metering")
            .one(&f.db)
            .await?
            .is_none()
    );
    metering.record_request(metric()).await?;
    observation(&budget, Some(amount))?;
    ensure!(store.begin_write(false).await.is_err());
    ensure!(observation(&budget, None)? == 0);
    // Real existing maintenance classification: retire a nonleased obsolete job.
    JudgeJobs::new(store.clone())
        .supersede("retire", "obsolete")
        .await?;
    let (_, retired): (_, JudgeJob) = store
        .get("judge_job", "retire")
        .await?
        .context("retired job")?;
    ensure!(retired.status == JobStatus::Superseded);
    ensure!(observation(&budget, None)? == 0);
    observation(&budget, Some(amount + FLOOR))?;
    let (write, tx) = store.begin_write(false).await?;
    write.finish(tx, Ok(())).await?;
    eprintln!(
        "writer calibration injected_unbound_free pressure={amount} ordinary_floor={FLOOR}; metering refusal and judge maintenance acknowledged"
    );
    Ok(())
}

async fn sqlite_full(f: &Fixture) -> Result<bool> {
    let store = f.store("page-limit-full")?;
    store
        .initialize("fixture", "full", None, &"original".to_owned())
        .await?;
    let before = sample(&f.db, &f.path).await?;
    ensure!(
        (before.free_pages + 1) * before.page_size < 4 * MIB,
        "fresh fixture must have fewer reusable pages than the FULL payload"
    );
    let cap: i64 =
        f.db.query_one(sql(format!("PRAGMA max_page_count={}", before.pages + 1)))
            .await?
            .context("lower page limit")?
            .try_get_by_index(0)?;
    ensure!(u64::try_from(cap)? == before.pages + 1);
    let (mut write, tx) = store.begin_write(false).await?;
    let (row, _): (_, String) = store
        .lock_charged(&mut write, &tx, "fixture", "full")
        .await?;
    let result = store
        .save_charged(&mut write, &tx, row, &"f".repeat(4 * 1024 * 1024))
        .await;
    let error = result
        .as_ref()
        .err()
        .and_then(|e| e.downcast_ref::<DbErr>())
        .context("real SQL failure")?;
    let code = match error {
        DbErr::Exec(sea_orm::RuntimeErr::SqlxError(sea_orm::SqlxError::Database(error)))
        | DbErr::Query(sea_orm::RuntimeErr::SqlxError(sea_orm::SqlxError::Database(error))) => {
            error.code()
        }
        _ => None,
    }
    .context("typed SQLite result code")?
    .parse::<i32>()?;
    ensure!(code & 0xff == 13, "expected SQLite FULL, received {code}");
    let finished = write.finish(tx, result).await;
    let message = finished.err().context("FULL reported success")?.to_string();
    let retained = observation(&f.service.storage_budget(), None)?;
    let ambiguous = message == "internal error: local write rollback unconfirmed";
    ensure!(ambiguous || message == "internal error: local storage SQL failed");
    ensure!(retained == if ambiguous { physical_estimate()? } else { 0 });
    for maintenance in [false, true] {
        ensure!(matches!(
            f.service.storage_budget().reserve(0, maintenance),
            Err(ErrorCode::StorageUnavailable)
        ));
    }
    ensure!(store.get::<String>("fixture", "full").await? == Some((0, "original".into())));
    eprintln!(
        "writer calibration real_SQLITE_FULL code={code} page_limit={cap} retained={retained} rollback_unconfirmed={ambiguous} before={before:?} after={:?}",
        sample(&f.db, &f.path).await?
    );
    Ok(ambiguous)
}

#[tokio::test]
async fn finite_physical_samples_use_real_writers_and_shared_custody() -> Result<()> {
    for mode in ["WAL", "DELETE"] {
        let fixture = Fixture::new(mode).await?;
        let result = async {
            sizes_and_rollback(&fixture).await?;
            if fixture.wal {
                pinned_reader(&fixture).await?;
            }
            shared_pressure(&fixture).await?;
            sample(&fixture.db, &fixture.path).await?;
            Ok(())
        }
        .await;
        fixture.close(result, false).await?;
    }
    // Fresh small DB: freelist pages from the near-bound case must not mask FULL.
    let fixture = Fixture::new("DELETE").await?;
    let result = sqlite_full(&fixture).await;
    let quarantined = result.as_ref().copied().unwrap_or(false);
    fixture.close(result.map(|_| ()), quarantined).await
}
