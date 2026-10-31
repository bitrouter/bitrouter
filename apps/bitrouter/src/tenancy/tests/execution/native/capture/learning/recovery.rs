//! Real leased file, public checked controller and the existing Registry stop set.
//! Internal children are never standalone qualification selectors.
use super::*;
use base64::Engine;
use std::{io::Write, process::Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};
mod commit;
mod custody;
mod missing;
mod process;
mod restart;
use process::Child;

const ROOT: &str = "BITROUTER_PAID_RECOVERY_ROOT";
const MODE: &str = "BITROUTER_PAID_RECOVERY_MODE";
const KEY: &str = "BITROUTER_PAID_RECOVERY_KEY";
const SUBJECT: &str = "BITROUTER_PAID_RECOVERY_SUBJECT";
const JUDGE: &str = "BITROUTER_PAID_RECOVERY_JUDGE";
const PREFIX: &str = "tenancy::tests::execution::native::capture::learning::recovery::";

fn config() -> crate::config::tenancy::TenancyConfig {
    crate::config::tenancy::TenancyConfig {
        enabled: true,
        wrapping_key_env: Some(KEY.into()),
        ..Default::default()
    }
}
fn url(root: &Path) -> String {
    format!("sqlite://{}", root.join("paid.sqlite").display())
}
fn signal(line: &str) -> Result<()> {
    println!("\n{line}");
    std::io::stdout().flush()?;
    Ok(())
}
async fn blocked() -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(60), input.read_line(&mut line)).await??;
    anyhow::bail!("isolated recovery child must be explicitly reaped by its parent")
}
fn locked(root: &Path) -> Result<()> {
    let error = crate::tenancy::ownership::acquire_path(&root.join("paid.sqlite"), false)
        .err()
        .context("paid file owner was released")?;
    ensure!(matches!(
        error.downcast_ref::<std::fs::TryLockError>(),
        Some(std::fs::TryLockError::WouldBlock)
    ));
    Ok(())
}
async fn wait_file(root: &Path) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            match crate::tenancy::ownership::acquire_path(&root.join("paid.sqlite"), false) {
                Ok(owner) => {
                    drop(owner);
                    return Ok::<_, anyhow::Error>(());
                }
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    tokio::task::yield_now().await
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await?
}
async fn values(db: &sea_orm::DatabaseConnection, kind: &str) -> Result<Vec<Value>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT body FROM checkpoint_evolution WHERE kind=? ORDER BY record_id LIMIT 129",
            [kind.into()],
        ))
        .await?;
    ensure!(rows.len() <= 128);
    rows.into_iter()
        .map(|row| Ok(serde_json::from_str(&row.try_get::<String>("", "body")?)?))
        .collect()
}
async fn exact(db: &sea_orm::DatabaseConnection) -> Result<Value> {
    let mut result = serde_json::Map::new();
    for (name, sql) in [
        (
            "jobs",
            "SELECT record_id,kind,revision,body FROM checkpoint_evolution ORDER BY record_id LIMIT 265",
        ),
        (
            "heads",
            "SELECT session_key,revision_id FROM acp_effective_assessments ORDER BY session_key LIMIT 129",
        ),
        (
            "assessments",
            "SELECT revision_id,revision_json FROM acp_assessment_revisions ORDER BY revision_id LIMIT 129",
        ),
        (
            "receipts",
            "SELECT execution_id,receipt_json,disposition_json FROM tenancy_execution_receipts ORDER BY execution_id LIMIT 129",
        ),
        (
            "usage",
            "SELECT execution_id,evidence_json FROM tenancy_usage_evidence ORDER BY execution_id LIMIT 129",
        ),
    ] {
        let rows = db
            .query_all(Statement::from_string(DatabaseBackend::Sqlite, sql))
            .await?;
        let mut encoded = Vec::new();
        for row in rows {
            let value = match name {
                "jobs" => json!([
                    row.try_get::<String>("", "record_id")?,
                    row.try_get::<String>("", "kind")?,
                    row.try_get::<i64>("", "revision")?,
                    row.try_get::<String>("", "body")?
                ]),
                "heads" => json!([
                    row.try_get::<String>("", "session_key")?,
                    row.try_get::<Option<String>>("", "revision_id")?
                ]),
                "assessments" => json!([
                    row.try_get::<String>("", "revision_id")?,
                    row.try_get::<String>("", "revision_json")?
                ]),
                "usage" => json!([
                    row.try_get::<String>("", "execution_id")?,
                    row.try_get::<String>("", "evidence_json")?
                ]),
                _ => json!([
                    row.try_get::<String>("", "execution_id")?,
                    row.try_get::<String>("", "receipt_json")?,
                    row.try_get::<String>("", "disposition_json")?
                ]),
            };
            encoded.push(value);
        }
        result.insert(name.into(), Value::Array(encoded));
    }
    Ok(Value::Object(result))
}

#[tokio::test]
#[ignore = "internal real paid producer; select the bounded parent only"]
async fn producer() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("paid root")?);
    let mode = std::env::var(MODE)?;
    let database = crate::db::connect_owned_tenancy(&url(&root)).await?;
    let db = database.database().clone();
    let fixture =
        Fixture::with_database(db.clone(), OperationLimits::default(), &mut |_| {}).await?;
    let mut ready = Ready::from_fixture(
        fixture,
        None,
        &std::env::var(SUBJECT)?,
        &std::env::var(JUDGE)?,
        true,
        LearningMode::Observe,
        2,
    )
    .await?;
    ready.fixture.service.shutdown().await?;
    // The same originally leased pool is consumed once, before real admission.
    ready.fixture.service = Service::open_owned_profile(
        database,
        &config(),
        &crate::config::ContinuationConfig::default(),
    )
    .await?;
    ready
        .fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let gate = Arc::new(crate::tenancy::receipts::authority_test::CredentialGate::default());
    let app = Arc::new(
        host_app(
            &ready.fixture,
            &std::env::var(SUBJECT)?,
            Arc::new(HostSpies::default()),
        )?
        .with_tenancy(ready.fixture.service.clone()),
    );
    let listener = Listener::start(app.clone()).await?;
    let marker = root.join("acp.json");
    let mut session = None;
    let mut first_session = None;
    let first_marker = root.join("acp-r1.json");
    let work = async {
        if mode == "two-revisions" {
            first_session = Some(
                ControlledSession::open_authenticated(
                    app.clone(),
                    ready.headers()?,
                    launch(&executable()?, &first_marker, &listener.origin, None),
                    CancellationToken::new(),
                )
                .await?,
            );
            let first = first_session
                .as_ref()
                .context("first revision controller")?;
            let native = first.new_session(root.to_owned()).await?;
            first
                .prompt(&native, "first real revision assessment")
                .await?;
            first
                .checkpoint(&native)
                .await?
                .context("first revision checkpoint")?;
            drained(&ready).await?;
            first_session
                .take()
                .context("first revision owner")?
                .shutdown()
                .await?;
            reaped(&first_marker).await?;
            let revision = ready
                .configuration("assessment-recovery-r2", LearningMode::Observe, 2)
                .await?;
            ensure!(
                revision != ready.revision,
                "fixture must retain two distinct configurations"
            );
            ready.revision = revision;
        }
        session = Some(Arc::new(
            ControlledSession::open_authenticated(
                app,
                ready.headers()?,
                launch(&executable()?, &marker, &listener.origin, None),
                CancellationToken::new(),
            )
            .await?,
        ));
        let current = session.as_ref().context("actual controller")?;
        let native = current.new_session(root.to_owned()).await?;
        current
            .prompt(
                &native,
                "recorded task before interrupted paid finalization",
            )
            .await?;
        drained(&ready).await?;
        if mode == "human-head" {
            current
                .checkpoint(&native)
                .await?
                .context("first paid checkpoint")?;
            drained(&ready).await?;
            let review = current.review_checkpoint(&native, None).await?;
            current
                .retract_assessment(
                    &native,
                    crate::acp_trajectory::checkpoint::types::RevisionInput {
                        submission_id: "explicit-human-withdrawal".into(),
                        checkpoint_id: review.checkpoint.checkpoint_id,
                        expected_revision: review.scoring.expected_revision,
                        source: crate::acp_trajectory::checkpoint::types::AssessmentSource::Human,
                        evaluator_id: String::new(),
                        evaluator_version: String::new(),
                        assessment: None,
                        reason: "Keep this real human head while a later paid result is recovered"
                            .into(),
                    },
                )
                .await?;
            current
                .prompt(
                    &native,
                    "second historical prefix with a retained human head",
                )
                .await?;
            drained(&ready).await?;
        }
        if mode == "missing-terminal" {
            missing::install(&db).await?;
        } else {
            *ready
                .fixture
                .service
                .paid_finish_gate
                .lock()
                .map_err(|_| anyhow::anyhow!("finish scheduling witness"))? = Some(gate.clone());
        }
        let caller = current.clone();
        let checkpoint =
            AbortOnDropHandle::new(tokio::spawn(
                async move { caller.checkpoint(&native).await },
            ));
        if mode == "missing-terminal" {
            missing::receipt_rollback(&db, &ready.fixture.service).await?;
        } else {
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        }
        ensure!(!checkpoint.is_finished());
        let jobs = values(&db, "owned_judge_active_v3").await?;
        ensure!(jobs.len() == 1);
        if matches!(mode.as_str(), "interrupted" | "missing-terminal") {
            ensure!(jobs[0]["output"].is_null() && jobs[0]["recovery"]["observed"].is_null());
        } else {
            ensure!(
                jobs[0]["recovery"]["observed"].is_object()
                    && jobs[0]["output"]["usage"]["prompt_tokens"] == 4
            );
        }
        if mode != "missing-terminal" {
            ensure!(
                gate.execution
                    .get()
                    .is_some_and(|id| jobs[0]["execution"] == id.as_str())
            );
        }
        // Cancelling the controller command is not a fake cleanup proof: the
        // actual public owner is stopped and its real child/reaper joins below.
        drop(session.take());
        // Close actual host admissions before joining the existing task owner.
        // Cancelling its stop token alone does not close producer tickets.
        ready.fixture.service.hosts.close();
        tokio::time::timeout(TIMEOUT, ready.fixture.service.hosts.drain()).await??;
        reaped(&marker).await?;
        let _caller = tokio::time::timeout(TIMEOUT, checkpoint).await??;
        let mut shutdown = None;
        if mode != "no-stop-set" {
            let service = ready.fixture.service.clone();
            shutdown = Some(AbortOnDropHandle::new(tokio::spawn(async move {
                service.shutdown().await
            })));
            tokio::time::timeout(TIMEOUT, async {
                loop {
                    let rows = values(&db, "owned_judge_active_v3").await?;
                    if rows.len() == 1 && rows[0]["recovery"]["stopped"].is_object() {
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await??;
            if mode == "missing-terminal" {
                // Known failed receipt storage can finish shutdown with Err.
                // It must never report clean while retaining an uncommitted terminal.
                if shutdown.as_ref().is_some_and(|task| task.is_finished()) {
                    ensure!(shutdown.take().context("shutdown owner")?.await?.is_err());
                }
                missing::absent(
                    &db,
                    jobs[0]["execution"].as_str().context("paid execution")?,
                )
                .await?;
            } else {
                ensure!(
                    shutdown.as_ref().is_some_and(|task| !task.is_finished()),
                    "held finalizer was reported clean"
                );
            }
        }
        let before = exact(&db).await?;
        std::fs::write(root.join("before.json"), serde_json::to_vec(&before)?)?;
        locked(&root)?;
        signal("paid-recovery-producer-stopped")?;
        let result = blocked().await;
        drop(shutdown);
        result
    }
    .await;
    // Normal/error exits still attempt every real cleanup. Parent death is only
    // permitted after its observed reaper/local-ACK/Registry boundary above.
    gate.release.cancel();
    drop(session);
    drop(first_session);
    let stopped = ready.fixture.service.shutdown().await;
    let final_reaped = if marker.try_exists()? {
        reaped(&marker).await
    } else {
        Ok(())
    };
    let first_reaped = if first_marker.try_exists()? {
        reaped(&first_marker).await
    } else {
        Ok(())
    };
    let listener = listener.finish().await;
    let closed = db.close().await;
    work.and(stopped)
        .and(final_reaped)
        .and(first_reaped)
        .and(listener)
        .and(closed.map_err(Into::into))
}
