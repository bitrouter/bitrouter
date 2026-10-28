//! Actual v4 producer -> stopped original Registry -> real faulted bootstrap.
//! Nested here only to reuse the existing bounded raw snapshot without widening APIs.
use super::*;
use crate::tenancy::transaction::{Disposition, Phase, receipt_observation::Key};
mod commit;
mod custody;
mod facts;
mod fresh;
const CHILD_PREFIX: &str = "integrity::repair::";

fn config() -> crate::config::tenancy::TenancyConfig {
    crate::config::tenancy::TenancyConfig {
        enabled: true,
        wrapping_key_env: Some(KEY.into()),
        ..Default::default()
    }
}
fn url(root: &Path) -> String {
    format!("sqlite://{}", root.join("native-paid.db").display())
}
fn locked(root: &Path) -> Result<()> {
    let error = crate::tenancy::ownership::acquire_path(&root.join("native-paid.db"), false)
        .err()
        .context("native paid repair released its original claimed owner")?;
    ensure!(matches!(
        error.downcast_ref::<std::fs::TryLockError>(),
        Some(std::fs::TryLockError::WouldBlock)
    ));
    Ok(())
}
async fn released(root: &Path) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            match crate::tenancy::ownership::acquire_path(&root.join("native-paid.db"), false) {
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
async fn blocked() -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(60), input.read_line(&mut line)).await??;
    anyhow::bail!("original unknown owner requires explicit parent kill/wait")
}
fn key(value: &Value) -> Result<Key> {
    ensure!(
        value["version"] == 4
            && value["status"] == "accepted"
            && value["recovery"]["seed"]["native"].is_object()
            && value["recovery"]["native_cleanup"].is_object()
            && value["recovery"]["stopped"].is_object()
            && value["native_evidence"].is_object()
            && value["output"].is_null()
            && value["assessment"].is_null()
            && value["recovery"]["observed"].is_null(),
        "genuine missing035 stopped native v4 producer required"
    );
    let seed = &value["recovery"]["seed"];
    Ok(Key {
        installation: seed["installation"]
            .as_str()
            .context("installation")?
            .try_into()?,
        tenant: seed["tenant"].as_str().context("tenant")?.try_into()?,
        execution: seed["execution"]
            .as_str()
            .context("execution")?
            .try_into()?,
    })
}
async fn begin_fixture(
    root: &Path,
) -> Result<(
    crate::db::OwnedDatabase,
    sea_orm::DatabaseConnection,
    Value,
    state::Snapshot,
)> {
    let database = crate::db::connect_owned_tenancy(&url(root)).await?;
    let db = database.database().clone();
    // This is the actual producer's INSERT fault. No receipt/proof is deleted.
    db.execute_unprepared("DROP TRIGGER native_paid_terminal_abort")
        .await?;
    let original: Value = state::read_json(&root.join("accepted.json"))?;
    let identity = key(&original)?;
    absent(&db, identity.execution.as_str()).await?;
    let active = rows(&db, "owned_judge_active_v3").await?;
    ensure!(active.len() == 1 && active[0] == original);
    let before = state::snapshot(&db).await?;
    state::write_json(&root.join("repair-before.json"), &before)?;
    Ok((database, db, original, before))
}

async fn campaign(root: &Path, mode: &str) -> Result<()> {
    // Leave the rest of the original120s parent watchdog for same-child wait,
    // bounded diagnostics and provider shutdown. No new child after this window.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let subject = upstream().await?;
    let mut judge = Judge::start().await?;
    let subject_url = format!("{}/v1", subject.uri());
    let mut producer = match Child::start(
        "producer",
        root,
        "missing",
        Some((&subject_url, &judge.url)),
    ) {
        Ok(child) => child,
        Err(error) => return Err::<(), _>(error).and(judge.finish().await),
    };
    let produced = tokio::time::timeout_at(deadline, async {
        producer.line("native-paid-source-ready").await?;
        judge.retained_answer(state::read_json(&root.join("answer.json"))?)?;
        producer.advance().await?;
        producer.line("native-paid-accepted").await?;
        ensure!(judge.received().await?["model"] == "judge-model");
        judge.release.cancel();
        producer.line("native-paid-stopped").await?;
        locked(root)
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|value| value);
    judge.release.cancel();
    let reaped = producer.finish(true, produced).await;
    let checked = async {
        reaped?;
        released(root).await?;
        let source: Value = state::read_json(&root.join("accepted.json"))?;
        key(&source)?;
        let lease = crate::tenancy::ownership::acquire_path(&root.join("native-paid.db"), false)?;
        let archive = state::archive(root)?;
        drop(lease);
        ensure!(
            tokio::time::Instant::now() < deadline,
            "repair work window expired"
        );
        let commit = matches!(mode, "commit-error" | "late-live" | "late-cancel");
        let selector = format!(
            "{CHILD_PREFIX}{}::opener",
            if commit { "commit" } else { "custody" }
        );
        let mut fault = Child::start(&selector, root, mode, None)?;
        let witness = tokio::time::timeout_at(deadline, async {
            if commit {
                fault.line("native-paid-repair-commit-pending").await?;
                locked(root)?;
                ensure!(
                    !judge.extra_request()
                        && subject
                            .received_requests()
                            .await
                            .context("subject calls")?
                            .len()
                            == 1
                );
                fault.advance().await?;
            }
            if matches!(mode, "late-live" | "late-cancel") {
                fault.line("native-paid-repair-known-retired").await?;
                released(root).await?;
            } else {
                fault.line("native-paid-repair-unknown-retained").await?;
                locked(root)?;
            }
            ensure!(
                !judge.extra_request()
                    && subject
                        .received_requests()
                        .await
                        .context("subject calls")?
                        .len()
                        == 1
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|value| value);
        fault
            .finish(!matches!(mode, "late-live" | "late-cancel"), witness)
            .await?;
        released(root).await?;
        // Two normal production restarts. Neither has a fault gate or a grant
        // reconstructed from the stored seed; actual StartupOwner owns both.
        for _ in 0..2 {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "fresh repair work window expired"
            );
            let mut fresh =
                Child::start(&format!("{CHILD_PREFIX}fresh::consumer"), root, mode, None)?;
            let result =
                tokio::time::timeout_at(deadline, fresh.line("native-paid-repair-fresh-clean"))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|value| value);
            fresh.finish(false, result).await?;
            released(root).await?;
            ensure!(
                state::archive_digest(root)? == archive,
                "original native producer evidence changed"
            );
            ensure!(
                !judge.extra_request()
                    && subject
                        .received_requests()
                        .await
                        .context("subject calls")?
                        .len()
                        == 1,
                "bootstrap resent provider work"
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    checked.and(judge.finish().await)
}
async fn parent(mode: &str) -> Result<()> {
    let root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        Box::pin(campaign(root.path(), mode)),
    )
    .await
    .context("finite native repair parent watchdog")
    .and_then(|value| value);
    if let Err(error) = result {
        return Err(error.context(format!(
            "preserved native repair fixture {}",
            root.keep().display()
        )));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "real native v4 producer and consuming BEGIN error; select this parent"]
async fn native_paid_repair_actual_begin_error_retains_claim_then_fresh_repairs_twice() -> Result<()>
{
    parent("begin").await
}
#[tokio::test]
#[ignore = "real native v4 producer and consuming ROLLBACK error; select this parent"]
async fn native_paid_repair_actual_rollback_error_retains_claim_then_fresh_repairs_twice()
-> Result<()> {
    parent("rollback").await
}
#[tokio::test]
#[ignore = "real native v4 producer and caught apply unwind; select this parent"]
async fn native_paid_repair_caught_unwind_retains_claim_then_fresh_repairs_twice() -> Result<()> {
    parent("unwind").await
}
#[tokio::test]
#[ignore = "real native v4 producer and cancelled opener; select this parent"]
async fn native_paid_repair_cancelled_begin_waiter_keeps_claim_until_process_exit() -> Result<()> {
    parent("cancel-begin").await
}
#[tokio::test]
#[ignore = "real native v4 producer and deferred COMMIT failure; select this parent"]
async fn native_paid_repair_actual_commit_error_retains_candidate_then_fresh_repairs_twice()
-> Result<()> {
    parent("commit-error").await
}
#[tokio::test]
#[ignore = "real native v4 producer and late original COMMIT; select this parent"]
async fn native_paid_repair_late_known_commit_retires_without_publishing_authority() -> Result<()> {
    parent("late-live").await
}
#[tokio::test]
#[ignore = "real native v4 producer and cancelled waiter with late original COMMIT"]
async fn native_paid_repair_cancelled_commit_waiter_keeps_custody_until_known_ack() -> Result<()> {
    parent("late-cancel").await
}
