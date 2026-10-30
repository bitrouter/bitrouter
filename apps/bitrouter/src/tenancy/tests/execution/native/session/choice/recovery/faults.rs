//! Real mutation/COMMIT failures on the same production repair transaction.
use super::*;
use bitrouter_ai::core::protocol::CommitStatus;

async fn state(db: &sea_orm::DatabaseConnection) -> Result<(String, i64)> {
    let row=db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT payload,(SELECT COUNT(*) FROM tenancy_execution_receipts) AS receipts FROM bro_managed_sessions"))
        .await?.context("native recovery source")?;
    Ok((row.try_get("", "payload")?, row.try_get("", "receipts")?))
}
pub(super) async fn exercise(
    bound: &Bound,
    spec: &Spec,
    root: &Path,
    mode: &str,
    generation: &Provider,
    choice: &wire::Endpoint,
) -> Result<()> {
    let before = state(&bound.fixture.db).await?;
    let sql_before = bound.fixture.service.cleanup.sql_counts_for_test();
    let original: Value = serde_json::from_slice(&std::fs::read(root.join("intent.json"))?)?;
    let child = original["owned_choices"]
        .as_object()
        .context("real intent")?
        .values()
        .next()
        .context("real Choice link")?["child_execution"]
        .as_str()
        .context("actual child")?;
    bound
        .fixture
        .service
        .cleanup
        .observe_receipt_for_test(Key {
            installation: bound.fixture.installation.clone(),
            tenant: bound.fixture.tenant.clone(),
            execution: child.try_into()?,
        })?;
    ensure!(before.1 == 0, "missing-terminal source was not established");
    if mode == "precommit-rollback" {
        bound.fixture.db.execute_unprepared("CREATE TRIGGER stopped_native_rollback BEFORE UPDATE ON bro_managed_sessions BEGIN SELECT RAISE(ABORT,'stopped native rollback'); END").await?;
    } else if mode == "commit-error" {
        // SQLite deferred constraints fail the actual consuming COMMIT after
        // both receipt INSERTs and the native replacement have executed.
        // https://www.sqlite.org/foreignkeys.html#fk_deferred
        for sql in [
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE stopped_native_parent(id INTEGER PRIMARY KEY)",
            "CREATE TABLE stopped_native_child(id INTEGER REFERENCES stopped_native_parent(id) DEFERRABLE INITIALLY DEFERRED)",
            "CREATE TRIGGER stopped_native_commit AFTER UPDATE ON bro_managed_sessions BEGIN INSERT INTO stopped_native_child(id) VALUES(1); END",
        ] {
            bound.fixture.db.execute_unprepared(sql).await?;
        }
    }
    let app = Arc::new(
        host_app(
            &bound.fixture,
            &spec.generation,
            Arc::new(HostSpies::default()),
        )?
        .with_tenancy(bound.fixture.service.clone()),
    );
    let headers = bound.headers.clone();
    let workspace = root.join("workspace");
    let gate = if mode == "revoked-after-prepare" {
        Some(
            bound
                .fixture
                .service
                .hold_host(crate::tenancy::hosts::HostBoundary::Resources)?,
        )
    } else {
        None
    };
    let opening = AbortOnDropHandle::new(tokio::spawn(async move {
        NativeSession::open_authenticated(
            app,
            headers,
            "choice-session",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await
    }));
    if let Some(gate) = &gate {
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        change_authority(&bound.fixture, &bound.key, &bound.route, "revoke").await?;
        gate.release();
    }
    let error = tokio::time::timeout(TIMEOUT, opening)
        .await??
        .err()
        .context("faulted native recovery published a handle")?;
    ensure!(
        generation.calls() == 0 && choice.calls() == 1,
        "recovery re-dispatched provider work"
    );
    let reader = crate::db::connect(&format!(
        "sqlite://{}?mode=ro",
        root.join("owned.db").display()
    ))
    .await?;
    let after = state(&reader).await?;
    reader.close().await?;
    ensure!(
        after == before,
        "partial native/receipt repair escaped rollback"
    );
    let trace = bound.fixture.service.cleanup.receipt_trace_for_test()?;
    ensure!(!trace.overflow);
    if mode == "revoked-after-prepare" {
        ensure!(trace.facts.iter().all(Option::is_none));
    } else {
        let fact = trace.facts[0]
            .as_ref()
            .context("actual native repair SQL was not reached")?;
        ensure!(fact.inserted == Some(true) && fact.store.is_some_and(|outcome| outcome.ok));
        if mode == "precommit-rollback" {
            ensure!(
                fact.native_checkpoint
                    .is_some_and(|outcome| !outcome.ok && outcome.sqlite_code == Some(1811))
                    && fact.rollback.is_some_and(|outcome| outcome.ok)
                    && fact.commit.is_none()
            );
        } else {
            ensure!(
                fact.native_checkpoint.is_some_and(|outcome| outcome.ok)
                    && fact
                        .commit
                        .is_some_and(|outcome| !outcome.ok && outcome.sqlite_code == Some(787))
                    && fact.rollback.is_none()
            );
        }
    }
    if mode == "commit-error" {
        ensure!(error.commit_status == CommitStatus::Unknown);
        ensure!(bound.fixture.service.cleanup.ensure_clean().is_err());
        ensure!(bound.fixture.service.host_slots_for_test() == 0);
        let failure = crate::tenancy::ownership::acquire_path(&root.join("owned.db"), false)
            .err()
            .context("unknown repair released file owner")?;
        ensure!(matches!(
            failure.downcast_ref::<std::fs::TryLockError>(),
            Some(std::fs::TryLockError::WouldBlock)
        ));
        let (reserved, _, _) = bound
            .fixture
            .service
            .admission
            .receipt_capacity_for_test()
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        ensure!(reserved > 0, "uncertain repair released its reservation");
    } else {
        // Core's submitted checkpoint contract stays Unknown without its ACK,
        // even when the exact H writer separately confirmed ROLLBACK. Preserve
        // that distinction instead of weakening the public Core contract.
        ensure!(
            error.commit_status
                == if mode == "precommit-rollback" {
                    CommitStatus::Unknown
                } else {
                    CommitStatus::NotCommitted
                }
        );
        let sql_after = bound.fixture.service.cleanup.sql_counts_for_test();
        let expected_rollbacks = if mode == "precommit-rollback" { 2 } else { 1 };
        ensure!(
            sql_after.0 >= sql_before.0 + expected_rollbacks
                && sql_after.1 >= sql_before.1 + expected_rollbacks
        );
        tokio::time::timeout(TIMEOUT, async {
            while bound.fixture.service.host_slots_for_test() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let (reserved, _, _) = bound
            .fixture
            .service
            .admission
            .receipt_capacity_for_test()
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        ensure!(
            reserved == 0,
            "confirmed non-commit retained repair capacity"
        );
    }
    Ok(())
}

pub(super) async fn unsupported(
    bound: &Bound,
    root: &Path,
    mode: &str,
    generation: &Provider,
    choice: &wire::Endpoint,
) -> Result<()> {
    let before = state(&bound.fixture.db).await?;
    let mut row: Value = serde_json::from_str(&before.0)?;
    if mode == "legacy-unreleased" {
        row["format"] = json!(1);
        row.as_object_mut()
            .context("native envelope")?
            .remove("recovery");
    }
    for link in row["journal"]["owned_choices"]
        .as_object_mut()
        .context("real Choice links")?
        .values_mut()
    {
        link.as_object_mut()
            .context("real Choice link")?
            .remove("seeds");
        if mode == "legacy-unreleased" {
            link["version"] = json!(1);
        }
    }
    let corrupted = serde_json::to_string(&row)?;
    // Negative only: preserve valid indexed length/scope so the actual decoder,
    // rather than an incidental corrupt byte count, must reject missing proof.
    bound
        .fixture
        .db
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE bro_managed_sessions SET payload=?,payload_bytes=?",
            [
                corrupted.clone().into(),
                i64::try_from(corrupted.len())?.into(),
            ],
        ))
        .await?;
    let opened = fixture::open(
        bound,
        generation,
        &root.join("workspace"),
        Arc::new(HostSpies::default()),
    )
    .await;
    ensure!(
        opened.is_err(),
        "unsupported predecessor minted a native handle"
    );
    ensure!(state(&bound.fixture.db).await? == (corrupted, 0));
    ensure!(generation.calls() == 0 && choice.calls() == 1);
    tokio::time::timeout(TIMEOUT, async {
        while bound.fixture.service.host_slots_for_test() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}
