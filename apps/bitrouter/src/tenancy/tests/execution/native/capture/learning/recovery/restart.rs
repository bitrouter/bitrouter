use super::*;

struct Stopped {
    root: tempfile::TempDir,
    provider: Provider,
    judge: Judge,
    calls: usize,
}
impl Stopped {
    async fn new(mode: &str) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let provider = Provider::start(false).await?;
        let mut judge = if mode == "interrupted" {
            Judge::rejected().await?
        } else {
            Judge::new(mode == "missing-terminal").await?
        };
        let calls = if matches!(mode, "human-head" | "two-revisions") {
            2
        } else {
            1
        };
        let result = async {
            let mut child = Child::start(
                "producer",
                root.path(),
                mode,
                Some((&provider.url, &judge.url)),
            )?;
            let checked = async {
                if mode == "missing-terminal" {
                    child.line("paid-recovery-accepted-before-terminal").await?;
                    let _request = judge.receive().await?;
                    ensure!(judge.calls.load(Ordering::SeqCst) == 1);
                    judge.release.cancel();
                }
                child.line("paid-recovery-producer-stopped").await?;
                locked(root.path())?;
                reaped(&root.path().join("acp.json")).await?;
                if mode == "two-revisions" {
                    reaped(&root.path().join("acp-r1.json")).await?;
                }
                ensure!(provider.calls() == calls && judge.calls.load(Ordering::SeqCst) == calls);
                Ok(())
            }
            .await;
            child.finish(true, checked).await?;
            wait_file(root.path()).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            let judge = judge.stop().await;
            let provider = provider.finish().await;
            if let Err(cleanup) = judge.and(provider) {
                return Err(error.context(format!("paid fixture cleanup: {cleanup:#}")));
            }
            return Err(error);
        }
        Ok(Self {
            root,
            provider,
            judge,
            calls,
        })
    }
    async fn run(&self, selector: &str, mode: &str) -> Result<()> {
        let child = Child::start(selector, self.root.path(), mode, None)?;
        child.finish(false, Ok(())).await?;
        ensure!(
            self.provider.calls() == self.calls
                && self.judge.calls.load(Ordering::SeqCst) == self.calls,
            "startup replayed provider work"
        );
        wait_file(self.root.path()).await
    }
    async fn close(self, result: Result<()>) -> Result<()> {
        let judge = self.judge.stop().await;
        let provider = self.provider.finish().await;
        result.and(judge).and(provider)
    }
}
#[tokio::test]
#[ignore = "real file/process/ACP recovery; select this parent, no nested Cargo"]
async fn paid_stopped_restart_preserves_known_usage_never_selects_or_resends_and_is_idempotent()
-> Result<()> {
    let stopped = Stopped::new("stopped").await?;
    let result = async {
        stopped.run("restart::consumer", "positive").await?;
        stopped.run("restart::consumer", "again").await
    }
    .await;
    stopped.close(result).await
}
#[tokio::test]
#[ignore = "real file/process/ACP recovery; select this parent, no nested Cargo"]
async fn paid_local_cleanup_without_exact_registry_ack_cannot_recover() -> Result<()> {
    let stopped = Stopped::new("no-stop-set").await?;
    let result = stopped.run("restart::consumer", "refuse").await;
    stopped.close(result).await
}
#[tokio::test]
#[ignore = "real stopped record corruption; no constructed recovery authority"]
async fn paid_stopped_counter_pending_and_origin_corruption_refuse_without_repair() -> Result<()> {
    for mode in ["counter", "mirror", "origin"] {
        let stopped = Stopped::new("stopped").await?;
        let result = stopped.run("restart::consumer", mode).await;
        stopped.close(result).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "internal real paid recovery opener; select its parent only"]
async fn consumer() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("paid root")?);
    let mode = std::env::var(MODE)?;
    let database = crate::db::connect_owned_tenancy(&url(&root)).await?;
    let db = database.database().clone();
    let checked = async {
        if matches!(mode.as_str(), "counter" | "mirror" | "origin") {
            corrupt(&db, &mode).await?;
        }
        if mode == "positive" {
            // A prior real rollback-fault child may leave its test trigger; only
            // that exact fixture trigger is removed after the owner was reaped.
            db.execute_unprepared("DROP TRIGGER IF EXISTS paid_repair_rollback")
                .await?;
        }
        if mode == "missing-terminal" {
            // Only the exact test trigger is removed, after real predecessor death.
            db.execute_unprepared("DROP TRIGGER paid_terminal_aborted")
                .await?;
        }
        if mode == "commit-retry" {
            // The real failed consuming COMMIT owner has now been killed/reaped.
            // Drop only this campaign's exact deferred-FK trigger/tables. No
            // production row, immutable receipt or accepted Seed is changed.
            for sql in [
                "DROP TRIGGER paid_repair_commit_fault",
                "DROP TABLE paid_repair_fk_child",
                "DROP TABLE paid_repair_fk_parent",
            ] {
                db.execute_unprepared(sql).await?;
            }
        }
        let before = exact(&db).await?;
        let open = Service::open_owned_profile(
            database,
            &config(),
            &crate::config::ContinuationConfig::default(),
        )
        .await;
        if matches!(mode.as_str(), "refuse" | "counter" | "mirror" | "origin") {
            if let Ok(service) = open {
                service.shutdown().await?;
                anyhow::bail!("unproved paid owner was adopted");
            }
            ensure!(
                exact(&db).await? == before,
                "rejected startup changed paid/receipt/head bytes"
            );
            return Ok(());
        }
        let service = open?;
        let result = async {
            ensure!(service.current().is_ok());
            service.cleanup.ensure_clean()?;
            let after = exact(&db).await?;
            missing::accounting(&service, &db).await?;
            if mode == "again" {
                let saved: Value =
                    serde_json::from_slice(&std::fs::read(root.join("after.json"))?)?;
                ensure!(
                    after == saved,
                    "repeated restart rewrote immutable terminal facts"
                );
                return Ok(());
            }
            let old: Value = serde_json::from_slice(&std::fs::read(root.join("before.json"))?)?;
            ensure!(old == before, "parent crash changed committed source facts");
            ensure!(
                after["heads"] == before["heads"],
                "recovery selected a head"
            );
            if matches!(mode.as_str(), "missing-terminal" | "commit-retry") {
                missing::repaired(&before, &after)?;
            } else {
                ensure!(
                    after["receipts"] == before["receipts"] && after["usage"] == before["usage"],
                    "recovery replaced known receipt or usage evidence bytes"
                );
            }
            let jobs = values(&db, "owned_judge_job").await?;
            let expected = if matches!(mode.as_str(), "human-head" | "two-revisions") {
                2
            } else {
                1
            };
            ensure!(jobs.len() == expected);
            let job = jobs
                .iter()
                .find(|job| job["recovery"]["stopped"].is_object())
                .context("repaired paid job")?;
            ensure!(job["version"] == 3);
            if matches!(
                mode.as_str(),
                "interrupted" | "missing-terminal" | "commit-retry"
            ) {
                ensure!(
                    job["status"] == "interrupted"
                        && job["output"].is_null()
                        && job["assessment"].is_null()
                );
                ensure!(
                    job["history_reservation"]["state"]["state"] == "released_without_revision"
                );
            } else {
                ensure!(
                    job["status"] == "unselected"
                        && job["output"]["usage"]["prompt_tokens"] == 4
                        && job["output"]["usage"]["completion_tokens"] == 2
                );
                ensure!(job["history_reservation"]["state"]["state"] == "consumed");
            }
            ensure!(
                values(&db, "owned_judge_pending").await?.is_empty()
                    && values(&db, "owned_judge_active_v3").await?.is_empty()
            );
            let counters = values(&db, "owned_judge_budget").await?;
            if mode == "two-revisions" {
                ensure!(
                    counters.len() == 2 && counters.iter().all(|row| row["accepted"] == 1),
                    "distinct retained revisions must keep their own accepted budget"
                );
                ensure!(
                    jobs[0]["configuration"] != jobs[1]["configuration"],
                    "bootstrap must actually decode two paid configurations"
                );
            } else {
                ensure!(
                    counters.len() == 1 && counters[0]["accepted"] == expected,
                    "repair refunded cumulative acceptance"
                );
            }
            let revisions = after["assessments"].as_array().context("assessment rows")?;
            if matches!(
                mode.as_str(),
                "interrupted" | "missing-terminal" | "commit-retry"
            ) {
                ensure!(revisions.is_empty());
            } else {
                ensure!(
                    revisions.len()
                        == match mode.as_str() {
                            "human-head" => 3,
                            "two-revisions" => 2,
                            _ => 1,
                        }
                );
                let parsed = revisions
                    .iter()
                    .map(|row| {
                        Ok(serde_json::from_str::<Value>(
                            row[1].as_str().context("revision bytes")?,
                        )?)
                    })
                    .collect::<Result<Vec<_>>>()?;
                let revision = parsed
                    .iter()
                    .find(|revision| revision["input"]["submission_id"] == job["job"])
                    .context("recovered revision")?;
                if mode == "human-head" {
                    ensure!(
                        before["heads"]
                            .as_array()
                            .is_some_and(|rows| rows.len() == 1)
                    );
                    ensure!(
                        parsed
                            .iter()
                            .any(|revision| revision["input"]["source"] == "human"
                                && revision["input"]["assessment"].is_null()
                                && revision["selected_on_submission"] == true)
                    );
                }
                ensure!(
                    revision["selection_reason"] == "stopped_owner_recovery"
                        && revision["selected_on_submission"] == false
                        && revision["supersedes"].is_null()
                );
                ensure!(
                    revision["input"]["source"] == "agentic"
                        && revision["input"]["submission_id"] == job["job"]
                );
            }
            ensure!(
                service
                    .admission
                    .receipt_capacity_for_test()
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?
                    .0
                    == 0
            );
            std::fs::write(root.join("after.json"), serde_json::to_vec(&after)?)?;
            Ok(())
        }
        .await;
        let stopped = service.shutdown().await;
        result.and(stopped)
    }
    .await;
    let closed = db.close().await;
    checked.and(closed.map_err(Into::into))
}
async fn corrupt(db: &sea_orm::DatabaseConnection, mode: &str) -> Result<()> {
    // Negative-only mutations of real committed producer records. Both mirrors
    // remain canonical in the origin case; no positive seed/proof is fabricated.
    let sql = match mode {
        "counter" => {
            "UPDATE checkpoint_evolution SET body=json_set(body,'$.accepted',2), revision=1 WHERE kind='owned_judge_budget'"
        }
        "mirror" => {
            "UPDATE checkpoint_evolution SET body=json_set(body,'$.input_digest',printf('%064d',0)) WHERE kind='owned_judge_pending'"
        }
        "origin" => {
            "UPDATE checkpoint_evolution SET body=json_set(body,'$.recovery.seed.origin.acquisition','00000000-0000-4000-8000-000000000001') WHERE kind IN ('owned_judge_active_v3','owned_judge_pending')"
        }
        _ => anyhow::bail!("unknown negative mode"),
    };
    let result = db
        .execute(Statement::from_string(DatabaseBackend::Sqlite, sql))
        .await?;
    ensure!(result.rows_affected() == if mode == "origin" { 2 } else { 1 });
    Ok(())
}

pub(super) async fn campaign(mode: &str) -> Result<()> {
    let stopped = Stopped::new("stopped").await?;
    let result = async {
        let mut child = Child::start("custody::opener", stopped.root.path(), mode, None)?;
        let witnessed = async {
            child.line("paid-bootstrap-owner-retained").await?;
            locked(stopped.root.path())?;
            Ok(())
        }
        .await;
        child.finish(true, witnessed).await?;
        wait_file(stopped.root.path()).await?;
        stopped.run("restart::consumer", "positive").await
    }
    .await;
    stopped.close(result).await
}

#[tokio::test]
#[ignore = "real failed judge and stopped owner; select this parent"]
async fn paid_stopped_failed_judge_is_interrupted_without_refund_or_resend() -> Result<()> {
    let stopped = Stopped::new("interrupted").await?;
    let result = async {
        stopped.run("restart::consumer", "interrupted").await?;
        stopped.run("restart::consumer", "again").await
    }
    .await;
    stopped.close(result).await
}

#[tokio::test]
#[ignore = "actual public human correction followed by a stopped paid owner"]
async fn paid_stopped_repair_preserves_the_actual_public_human_head() -> Result<()> {
    let stopped = Stopped::new("human-head").await?;
    let result = async {
        stopped.run("restart::consumer", "human-head").await?;
        stopped.run("restart::consumer", "again").await
    }
    .await;
    stopped.close(result).await
}

#[tokio::test]
#[ignore = "two real paid revisions on one leased file; select this parent"]
async fn paid_bootstrap_two_real_distinct_revisions_fit_the_shared_operation_budget() -> Result<()>
{
    let stopped = Stopped::new("two-revisions").await?;
    let result = async {
        stopped.run("restart::consumer", "two-revisions").await?;
        stopped.run("restart::consumer", "again").await
    }
    .await;
    stopped.close(result).await
}

#[tokio::test]
#[ignore = "real receipt INSERT rollback, real cleanup, crash and M5 repair; select this parent"]
async fn paid_missing_terminal_after_confirmed_local_stop_repairs_unknown_once_without_resend()
-> Result<()> {
    let stopped = Stopped::new("missing-terminal").await?;
    let result = async {
        stopped.run("restart::consumer", "missing-terminal").await?;
        stopped.run("restart::consumer", "again").await
    }
    .await;
    stopped.close(result).await
}

pub(super) async fn commit_campaign(mode: &str) -> Result<()> {
    let stopped = Stopped::new("missing-terminal").await?;
    let result = async {
        let mut child = Child::start("commit::opener", stopped.root.path(), mode, None)?;
        let checked = async {
            child.line("paid-repair-commit-pending").await?;
            locked(stopped.root.path())?;
            ensure!(
                stopped.provider.calls() == stopped.calls
                    && stopped.judge.calls.load(Ordering::SeqCst) == stopped.calls,
                "repair dispatched before its consuming COMMIT settled"
            );
            child.release_commit().await?;
            if mode == "commit-error" {
                child.line("paid-repair-commit-unknown-retained").await?;
                locked(stopped.root.path())?;
            } else {
                child.line("paid-repair-late-commit-retired").await?;
                wait_file(stopped.root.path()).await?;
            }
            ensure!(
                stopped.provider.calls() == stopped.calls
                    && stopped.judge.calls.load(Ordering::SeqCst) == stopped.calls,
                "repair dispatched a new provider request"
            );
            Ok(())
        }
        .await;
        child.finish(mode == "commit-error", checked).await?;
        wait_file(stopped.root.path()).await?;
        if mode == "commit-error" {
            stopped.run("restart::consumer", "commit-retry").await?;
        }
        // The late-known case already wrote after.json from the actual ACK;
        // the failed-COMMIT case writes it only after a new exclusive repair.
        stopped.run("restart::consumer", "again").await
    }
    .await;
    stopped.close(result).await
}
