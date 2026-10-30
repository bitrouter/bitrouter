//! Real paid bootstrap consuming COMMIT failure and late acknowledgement.
//! Every parent begins with 002's actual missing-terminal producer and stop set.
use super::*;
use crate::tenancy::transaction::{Disposition, Phase, receipt_observation::Key};

#[tokio::test]
#[ignore = "real paid missing terminal and consuming SQLite COMMIT error; select this parent"]
async fn paid_repair_consuming_commit_error_keeps_owner_until_death_then_repairs_once() -> Result<()>
{
    restart::commit_campaign("commit-error").await
}

#[tokio::test]
#[ignore = "real paid repair held beyond the original startup deadline; select this parent"]
async fn paid_repair_late_known_commit_retires_without_publishing_authority_or_resending()
-> Result<()> {
    restart::commit_campaign("late-live").await
}

#[tokio::test]
#[ignore = "real paid repair with a cancelled opener and late COMMIT ACK; select this parent"]
async fn paid_repair_disconnected_opener_keeps_custody_until_late_known_commit() -> Result<()> {
    restart::commit_campaign("late-cancel").await
}

async fn commit_fault(db: &sea_orm::DatabaseConnection) -> Result<()> {
    // The owned SQLite profile has one physical writer. This is a real deferred
    // FK violation, whose INSERT succeeds and whose consuming COMMIT fails.
    // No error string, disposition, terminal or Seed is supplied by the fixture.
    for sql in [
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE paid_repair_fk_parent(id INTEGER PRIMARY KEY)",
        "CREATE TABLE paid_repair_fk_child(id INTEGER REFERENCES paid_repair_fk_parent(id) DEFERRABLE INITIALLY DEFERRED)",
        "CREATE TRIGGER paid_repair_commit_fault AFTER INSERT ON tenancy_execution_receipts WHEN EXISTS (SELECT 1 FROM checkpoint_evolution WHERE kind='owned_judge_active_v3' AND json_extract(body,'$.execution')=NEW.execution_id) BEGIN INSERT INTO paid_repair_fk_child(id) VALUES(1); END",
    ] {
        db.execute_unprepared(sql).await?;
    }
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA foreign_keys",
        ))
        .await?
        .context("SQLite FK mode")?;
    ensure!(row.try_get::<i64>("", "foreign_keys")? == 1);
    Ok(())
}

fn key(before: &Value) -> Result<Key> {
    let rows = before["jobs"].as_array().context("actual producer jobs")?;
    let rows: Vec<_> = rows
        .iter()
        .filter(|row| row[1] == "owned_judge_active_v3")
        .collect();
    ensure!(
        rows.len() == 1,
        "this parent requires one genuine accepted job"
    );
    let job: Value = serde_json::from_str(rows[0][3].as_str().context("accepted job bytes")?)?;
    ensure!(
        job["recovery"]["stopped"].is_object()
            && job["output"].is_null()
            && job["recovery"]["observed"].is_null(),
        "missing-terminal stopped producer required"
    );
    let seed = &job["recovery"]["seed"];
    let key = Key {
        installation: seed["installation"]
            .as_str()
            .context("seed installation")?
            .try_into()?,
        tenant: seed["tenant"].as_str().context("seed tenant")?.try_into()?,
        execution: seed["execution"]
            .as_str()
            .context("seed execution")?
            .try_into()?,
    };
    ensure!(
        !before["receipts"]
            .as_array()
            .context("producer receipts")?
            .iter()
            .any(|row| row[0] == key.execution.as_str()),
        "paid terminal was not actually missing"
    );
    Ok(key)
}

async fn release_from_parent() -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    let length = tokio::time::timeout(TIMEOUT, input.read_line(&mut line)).await??;
    ensure!(
        length > 0 && line == "release-commit\n",
        "explicit parent scheduling ACK required"
    );
    Ok(())
}

fn interrupted(before: &Value, after: &Value) -> Result<()> {
    missing::repaired(before, after)?;
    ensure!(
        before["heads"] == after["heads"] && before["assessments"] == after["assessments"],
        "repair selected or invented an assessment"
    );
    let original = before["jobs"].as_array().context("original records")?;
    let actual = after["jobs"].as_array().context("repaired records")?;
    let old_counters: Vec<_> = original
        .iter()
        .filter(|row| row[1] == "owned_judge_budget")
        .collect();
    let new_counters: Vec<_> = actual
        .iter()
        .filter(|row| row[1] == "owned_judge_budget")
        .collect();
    ensure!(
        !old_counters.is_empty() && old_counters == new_counters,
        "late commit refunded or rewrote cumulative paid acceptance"
    );
    ensure!(
        !actual
            .iter()
            .any(|row| row[1] == "owned_judge_pending" || row[1] == "owned_judge_active_v3")
    );
    let terminal: Vec<_> = actual
        .iter()
        .filter(|row| row[1] == "owned_judge_job")
        .collect();
    ensure!(terminal.len() == 1);
    let job: Value = serde_json::from_str(terminal[0][3].as_str().context("terminal bytes")?)?;
    ensure!(
        job["status"] == "interrupted"
            && job["output"].is_null()
            && job["assessment"].is_null()
            && job["history_reservation"]["state"]["state"] == "released_without_revision"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "internal real paid repair COMMIT owner; select one of its three parents"]
async fn opener() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("paid root")?);
    let mode = std::env::var(MODE)?;
    ensure!(matches!(
        mode.as_str(),
        "commit-error" | "late-live" | "late-cancel"
    ));
    let database = crate::db::connect_owned_tenancy(&url(&root)).await?;
    let db = database.database().clone();
    // Only 002's deliberate INSERT fault is removed after its predecessor has
    // been killed/reaped. Every durable paid/receipt/source row remains intact.
    db.execute_unprepared("DROP TRIGGER paid_terminal_aborted")
        .await?;
    if mode == "commit-error" {
        commit_fault(&db).await?;
    }
    let before = exact(&db).await?;
    let original: Value = serde_json::from_slice(&std::fs::read(root.join("before.json"))?)?;
    ensure!(
        before == original,
        "COMMIT fixture setup changed actual paid records"
    );
    let identity = key(&before)?;
    let (attached, receive) = tokio::sync::oneshot::channel();
    let mut opening = Some(AbortOnDropHandle::new(tokio::spawn(async move {
        Service::open_owned_profile_observed_startup(
            database,
            &config(),
            &crate::config::ContinuationConfig::default(),
            crate::telemetry::receipt_latency::Sink::default(),
            Some(crate::tenancy::StartupTest {
                bootstrap: Some(crate::tenancy::BootstrapGate {
                    phase: Phase::RollingBack,
                    attached,
                }),
                #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
                bind: None,
            }),
        )
        .await
    })));
    let (proof, epoch) = tokio::time::timeout(TIMEOUT, receive).await??;
    let result = async {
        tokio::time::timeout(TIMEOUT, epoch.entered.notified()).await?;
        // Existing 031 read-only pin validation precedes paid prepare/apply. Its
        // held rollback lets us install the exact-key observer before either.
        proof.observe_receipt_for_test(identity.clone())?;
        let commit = proof.hold(Phase::Committing);
        epoch.release.notify_one();
        tokio::time::timeout(TIMEOUT, commit.entered.notified()).await?;
        ensure!(proof.disposition() == Disposition::Pending);
        let counts = proof.sql_counts_for_test();
        let trace = proof.receipt_trace_for_test()?;
        ensure!(trace.key == identity && !trace.overflow);
        let candidate = trace.facts[0].as_ref().context("paid repair candidate never reached COMMIT")?;
        ensure!(candidate.commit.is_none() && candidate.fingerprint.attempt_count == 0);
        let fingerprint = candidate.fingerprint.clone();
        // Drop the last test-side writable clone BEFORE any ownership witness.
        // The Proof/Gate hold no database. Only the real actor/opener can own it.
        drop(db);
        if mode == "late-cancel" {
            let mut waiter = opening.take().context("opening waiter")?;
            waiter.abort();
            ensure!(matches!((&mut waiter).await, Err(error) if error.is_cancelled()));
        }
        if mode != "commit-error" {
            // Real unmodified management deadline; no paused clock, short limit,
            // synthesized timeout or release from an alternative owner.
            tokio::time::timeout(TIMEOUT, proof.wait_fenced()).await?;
        }
        ensure!(proof.disposition() == Disposition::Pending);
        if let Some(waiter) = &opening { ensure!(!waiter.is_finished()); }
        locked(&root)?;
        let reader = crate::db::connect(&format!("{}?mode=ro", url(&root))).await?;
        let checked = async {
            ensure!(exact(&reader).await? == before, "unacknowledged repair became visible early");
            signal("paid-repair-commit-pending")?;
            release_from_parent().await?;
            ensure!(proof.disposition() == Disposition::Pending);
            locked(&root)?;
            proof.release_gate();
            let trace = tokio::time::timeout(TIMEOUT, async {
                loop {
                    let trace = proof.receipt_trace_for_test()?;
                    ensure!(trace.key == identity && !trace.overflow);
                    if trace.facts[0].as_ref().is_some_and(|fact| fact.commit.is_some()) {
                        break Ok::<_, anyhow::Error>(trace);
                    }
                    tokio::task::yield_now().await;
                }
            }).await??;
            let fact = trace.facts[0].as_ref().context("consuming COMMIT observation")?;
            ensure!(fact.fingerprint == fingerprint && trace.facts[1].is_none(),
                "repair replaced its fixed candidate or retried consuming COMMIT");
            // This observer reports only the real consuming call; it does not
            // fabricate Begin/Store/Rollback results from a successful prefix.
            ensure!(fact.begin.is_none() && fact.store.is_none() && fact.rollback.is_none());
            if let Some(waiter) = opening.take() {
                let opened = tokio::time::timeout(TIMEOUT, waiter).await??;
                if let Ok(service) = opened {
                    let cleanup = service.shutdown().await;
                    anyhow::bail!("faulted/expired bootstrap published authority: {cleanup:?}");
                }
            }
            let after = exact(&reader).await?;
            if mode == "commit-error" {
                ensure!(fact.commit.is_some_and(|outcome| !outcome.ok && outcome.sqlite_code == Some(787)),
                    "actual consuming COMMIT must return SQLite CONSTRAINT_FOREIGNKEY");
                tokio::time::timeout(TIMEOUT, proof.quarantined()).await?;
                ensure!(proof.disposition() == Disposition::Ambiguous && proof.ensure_clean().is_err());
                ensure!(proof.sql_counts_for_test() == counts,
                    "test must not replace consuming COMMIT failure with a later explicit rollback ACK");
                ensure!(after == before, "unconfirmed COMMIT exposed partial paid/receipt repair");
                locked(&root)?;
                signal("paid-repair-commit-unknown-retained")?;
                blocked().await
            } else {
                ensure!(fact.commit.is_some_and(|outcome| outcome.ok && outcome.sqlite_code.is_none()));
                ensure!(proof.disposition() == Disposition::Committed);
                proof.ensure_clean()?;
                proof.wait_fenced().await;
                interrupted(&before, &after)?;
                let terminal = crate::tenancy::receipts::store::lookup_candidate(
                    &reader, &identity.installation, &identity.tenant, &identity.execution,
                ).await?.context("late exact terminal")?;
                ensure!(terminal.fingerprint_for_test() == fingerprint,
                    "late ACK published a different immutable receipt candidate");
                std::fs::write(root.join("after.json"), serde_json::to_vec(&after)?)?;
                // A known ACK permits clean retirement, even after its waiter
                // vanished. This checks release while the child is still alive.
                wait_file(&root).await?;
                signal("paid-repair-late-commit-retired")?;
                Ok(())
            }
        }.await;
        let closed = reader.close().await;
        checked.and(closed.map_err(Into::into))
    }.await;
    // On a failed assertion, release scheduling gates before the parent reaps
    // the failed child. This never declares an uncertain transaction clean.
    epoch.release.notify_one();
    proof.release_gate();
    result
}
