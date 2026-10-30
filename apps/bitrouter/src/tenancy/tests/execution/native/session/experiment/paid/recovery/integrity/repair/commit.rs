//! Actual deferred-FK COMMIT error and late acknowledgement of the original candidate.
use super::*;

async fn fault(db: &DatabaseConnection) -> Result<()> {
    for sql in [
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE native_paid_repair_fk_parent(id INTEGER PRIMARY KEY)",
        "CREATE TABLE native_paid_repair_fk_child(id INTEGER REFERENCES native_paid_repair_fk_parent(id) DEFERRABLE INITIALLY DEFERRED)",
        "CREATE TRIGGER native_paid_repair_commit_fault AFTER INSERT ON tenancy_execution_receipts WHEN EXISTS (SELECT 1 FROM checkpoint_evolution WHERE kind='owned_judge_active_v3' AND json_extract(body,'$.version')=4 AND json_extract(body,'$.execution')=NEW.execution_id) BEGIN INSERT INTO native_paid_repair_fk_child(id) VALUES(1); END",
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

async fn release_from_parent() -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    let read = tokio::time::timeout(TIMEOUT, input.read_line(&mut line)).await??;
    ensure!(
        read > 0 && line == "release-commit\n",
        "explicit parent scheduling ACK required"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "internal native paid consuming COMMIT owner; select a bounded parent"]
async fn opener() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("native repair root")?);
    let mode = std::env::var(MODE)?;
    ensure!(matches!(
        mode.as_str(),
        "commit-error" | "late-live" | "late-cancel"
    ));
    let (database, db, original, clean_before) = begin_fixture(&root).await?;
    if mode == "commit-error" {
        fault(&db).await?;
    }
    let before = state::snapshot(&db).await?;
    let identity = key(&original)?;
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
    let (proof, prepared) = tokio::time::timeout(TIMEOUT, receive).await??;
    let result = async {
        tokio::time::timeout(TIMEOUT, prepared.entered.notified()).await?;
        // Successful continuation initialization commits. This first rollback
        // is the paid prepare transaction, after the fixed proposal is installed.
        // Apply still runs through the original H owner and full source audit.
        proof.observe_receipt_for_test(identity.clone())?;
        let commit = proof.hold(Phase::Committing);
        prepared.release.notify_one();
        tokio::time::timeout(TIMEOUT, commit.entered.notified()).await?;
        ensure!(proof.disposition() == Disposition::Pending);
        let counts = proof.sql_counts_for_test();
        let trace = proof.receipt_trace_for_test()?;
        ensure!(trace.key == identity && !trace.overflow);
        let candidate = trace.facts[0]
            .as_ref()
            .context("native repair candidate never reached COMMIT")?;
        ensure!(candidate.commit.is_none() && candidate.fingerprint.attempt_count == 0);
        let fingerprint = candidate.fingerprint.clone();
        drop(db); // Only the original actor/opener may now hold the writer claim.
        if mode == "late-cancel" {
            let mut waiter = opening.take().context("opening waiter")?;
            waiter.abort();
            ensure!(matches!((&mut waiter).await, Err(error) if error.is_cancelled()));
        }
        if mode != "commit-error" {
            // Original production management deadline; no clock/limit override.
            tokio::time::timeout(TIMEOUT, proof.wait_fenced()).await?;
        }
        ensure!(proof.disposition() == Disposition::Pending);
        if let Some(waiter) = &opening {
            ensure!(!waiter.is_finished());
        }
        locked(&root)?;
        let reader = crate::db::connect(&format!("{}?mode=ro", url(&root))).await?;
        let checked = async {
            ensure!(
                state::snapshot(&reader).await? == before,
                "unacknowledged native repair became visible early"
            );
            signal("native-paid-repair-commit-pending")?;
            release_from_parent().await?;
            ensure!(proof.disposition() == Disposition::Pending);
            locked(&root)?;
            proof.release_gate();
            let trace = tokio::time::timeout(TIMEOUT, async {
                loop {
                    let trace = proof.receipt_trace_for_test()?;
                    ensure!(trace.key == identity && !trace.overflow);
                    if trace.facts[0]
                        .as_ref()
                        .is_some_and(|fact| fact.commit.is_some())
                    {
                        break Ok::<_, anyhow::Error>(trace);
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await??;
            let fact = trace.facts[0]
                .as_ref()
                .context("consuming native repair COMMIT observation")?;
            ensure!(
                fact.fingerprint == fingerprint && trace.facts[1].is_none(),
                "native repair replaced or retried its fixed candidate"
            );
            ensure!(
                fact.begin.is_none() && fact.store.is_none() && fact.rollback.is_none(),
                "commit-only observer must not fabricate other SQL outcomes"
            );
            if let Some(waiter) = opening.take() {
                let opened = tokio::time::timeout(TIMEOUT, waiter).await??;
                if let Ok(service) = opened {
                    let cleanup = service.shutdown().await;
                    anyhow::bail!(
                        "faulted/expired native bootstrap published authority: {cleanup:?}"
                    );
                }
            }
            let after = state::snapshot(&reader).await?;
            if mode == "commit-error" {
                ensure!(
                    fact.commit
                        .is_some_and(|outcome| !outcome.ok && outcome.sqlite_code == Some(787)),
                    "real consuming COMMIT must return SQLite CONSTRAINT_FOREIGNKEY"
                );
                tokio::time::timeout(TIMEOUT, proof.quarantined()).await?;
                ensure!(
                    proof.disposition() == Disposition::Ambiguous && proof.ensure_clean().is_err()
                );
                ensure!(
                    proof.sql_counts_for_test() == counts,
                    "consuming COMMIT failure gained a false explicit rollback ACK"
                );
                ensure!(
                    after == before,
                    "unconfirmed native COMMIT exposed partial repair"
                );
                locked(&root)?;
                signal("native-paid-repair-unknown-retained")?;
                blocked().await
            } else {
                ensure!(
                    fact.commit
                        .is_some_and(|outcome| outcome.ok && outcome.sqlite_code.is_none())
                );
                ensure!(proof.disposition() == Disposition::Committed);
                proof.ensure_clean()?;
                proof.wait_fenced().await;
                facts::interrupted(&clean_before, &after, &original)?;
                let terminal = facts::terminal(&reader, &original).await?;
                ensure!(
                    terminal.fingerprint_for_test() == fingerprint,
                    "late native ACK changed the immutable receipt candidate"
                );
                state::write_json(&root.join("repair-after.json"), &after)?;
                // Known ACK may retire while this child is alive, even though its
                // opener disappeared. A read-only pool cannot own the sidecar.
                released(&root).await?;
                signal("native-paid-repair-known-retired")?;
                Ok(())
            }
        }
        .await;
        let closed = reader.close().await;
        checked.and(closed.map_err(Into::into))
    }
    .await;
    prepared.release.notify_one();
    proof.release_gate();
    result
}
