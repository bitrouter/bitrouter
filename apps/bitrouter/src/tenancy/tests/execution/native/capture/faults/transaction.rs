use super::*;

pub(super) async fn exercise(root: &Path, mode: &str) -> Result<()> {
    let provider = Provider::start(false).await?;
    let (fixture, route) = file_fixture(&provider, root).await?;
    let (headers, _) = prepared(&fixture, &route, LearningMode::Observe).await?;
    let marker = root.join("never-spawned.json");
    if mode == "commit-error" {
        // DML succeeds; the actual consuming COMMIT encounters a deferred FK.
        // https://www.sqlite.org/foreignkeys.html#fk_deferred
        for sql in [
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE capture_fault_parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE capture_fault_child (parent INTEGER REFERENCES capture_fault_parent(id) DEFERRABLE INITIALLY DEFERRED)",
            "CREATE TRIGGER capture_fault_commit AFTER INSERT ON acp_capture_connections BEGIN INSERT INTO capture_fault_child(parent) VALUES (77); END",
        ] {
            fixture.db.execute_unprepared(sql).await?;
        }
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "PRAGMA foreign_keys",
            ))
            .await?
            .context("foreign key setting")?;
        ensure!(row.try_get::<i64>("", "foreign_keys")? == 1);
    }
    let app = Arc::new(
        host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );
    let reads = fixture.service.reads.available_permits();
    ensure!(reads > 0 && fixture.service.current().is_ok());
    let gate = fixture.service.cleanup.hold(Phase::Committing);
    let proposed = launch(
        Path::new("/must-not-execute"),
        &marker,
        "http://127.0.0.1:1",
        None,
    );
    let opener = AbortOnDropHandle::new(tokio::spawn(async move {
        ControlledSession::open_authenticated(app, headers, proposed, CancellationToken::new())
            .await
    }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    // Only Capture Create writes here: authority lookup uses a read rollback.
    // Reaching Committing proves the facade passed preparation and real DML.
    ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
    ensure!(fixture.service.reads.available_permits() == reads - 1);
    ensure!(fixture.service.host_slots_for_test() == 0 && !opener.is_finished());
    ensure!(!marker.exists() && provider.calls() == 0);
    if mode == "late-commit" {
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::time::resume();
        tokio::time::timeout(TIMEOUT, fixture.service.cleanup.wait_fenced()).await?;
        ensure!(fixture.service.current().is_err());
        ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
        ensure!(!opener.is_finished() && fixture.service.reads.available_permits() == reads - 1);
    }
    fixture.service.cleanup.release_gate();
    ensure!(tokio::time::timeout(TIMEOUT, opener).await??.is_err());
    tokio::time::timeout(TIMEOUT, fixture.service.cleanup.quarantined()).await?;
    retained(&fixture)?;
    let rows = tokio::time::timeout(
        TIMEOUT,
        fixture.db.query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT head, state, metadata_json, installation_id, tenant_id, creator_principal_id FROM acp_capture_connections",
        )),
    )
    .await??;
    if mode == "commit-error" {
        // This real pool read also waits behind the driver's queued rollback.
        // A subsequently empty table cannot turn consumed COMMIT into an ACK.
        ensure!(rows.is_empty());
        ensure!(fixture.service.reads.available_permits() == reads - 1);
    } else {
        ensure!(rows.len() == 1, "late commit was lost or retried");
        let row = rows.first().context("late capture connection")?;
        ensure!(row.try_get::<i64>("", "head")? == 0);
        ensure!(row.try_get::<String>("", "state")? == "recording");
        ensure!(row.try_get::<String>("", "installation_id")? == fixture.installation.as_str());
        ensure!(row.try_get::<String>("", "tenant_id")? == fixture.tenant.as_str());
        ensure!(row.try_get::<String>("", "creator_principal_id")? == fixture.principal.as_str());
        let state: Value = serde_json::from_str(&row.try_get::<String>("", "metadata_json")?)?;
        ensure!(state["connected"] == false && state["closed"] == false);
        ensure!(count(&fixture, "acp_capture_events").await? == 0);
    }
    // The late commit's final disposition may already be Ambiguous because the
    // fenced owner cannot append its disconnect. Durable rows prove the commit;
    // never sample the global last-transaction disposition after CloseProducer.
    no_clean_drain(&fixture, false).await?;
    retained(&fixture)?;
    ensure!(!marker.exists() && provider.calls() == 0);
    tokio::time::timeout(TIMEOUT, fixture.db.clone().close()).await??;
    provider.finish().await?;
    Ok(())
}
