//! Whole-journal reconciliation through the real native opener and H commit.
use super::*;

async fn row(fixture: &Fixture) -> Result<(i64, Value)> {
    let row = fixture.db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT revision, payload, payload_bytes, installation_id, tenant_id, session_principal_id, logical_name FROM bro_managed_sessions"))
        .await?.context("native journal row")?;
    let bytes = row.try_get::<String>("", "payload")?;
    ensure!(row.try_get::<i64>("", "payload_bytes")? == i64::try_from(bytes.len())?);
    ensure!(row.try_get::<String>("", "installation_id")? == fixture.installation.as_str());
    ensure!(row.try_get::<String>("", "tenant_id")? == fixture.tenant.as_str());
    ensure!(row.try_get::<String>("", "session_principal_id")? == fixture.principal.as_str());
    ensure!(row.try_get::<String>("", "logical_name")? == "readback");
    Ok((row.try_get("", "revision")?, serde_json::from_str(&bytes)?))
}

pub(super) async fn exercise(root: &Path, mode: &str) -> Result<()> {
    ensure!(matches!(
        mode,
        "journal-exact" | "journal-artifact" | "journal-fence"
    ));
    let mut provider = Provider::start(false).await?;
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, root).await?;
    let (headers, key) = ready(&fixture, &route).await?;
    let app = Arc::new(
        host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );
    let mut session = NativeSession::open_authenticated(
        app.clone(),
        headers.clone(),
        "readback",
        &workspace,
        ToolMode::ReadOnly,
        &HarnessConfig::default(),
    )
    .await?;
    let snapshot = session
        .run(input(), Arc::new(Approval), CancellationToken::new())
        .await?;
    ensure!(snapshot.run.as_ref().is_some_and(
        |run| run.status == crate::storage::checkpoint::snapshot::RunStatus::Completed
    ));
    provider.receive().await?;
    session.close().await?;
    let (revision, baseline) = row(&fixture).await?;
    ensure!(baseline["journal"]["released"] == true);
    ensure!(baseline["journal"]["checkpoint"].is_object());
    ensure!(
        baseline["journal"]["head"]["state_revision"]
            .as_u64()
            .is_some_and(|n| n > 1)
    );
    let checkpoint = baseline["journal"]["checkpoint"].clone();

    let mutation = match mode {
        "journal-artifact" => {
            // A valid, unrelated stored object changes the whole journal without
            // changing the checkpoint, grant, scope or row revision. The trigger
            // keeps the declared length accurate so metadata checks still pass.
            use base64::Engine;
            let bytes = b"unreferenced evidence";
            let object = json!({"reference": {
                "artifact_id": "readback-probe",
                "sha256": crate::storage::checkpoint::sha256(bytes),
                "bytes": bytes.len(), "media_type": "text/plain"
            }, "content_base64": base64::engine::general_purpose::STANDARD.encode(bytes)});
            Some(format!("json_set(NEW.payload, '$.journal.artifacts.\"readback-probe\"', json('{}'))",
                serde_json::to_string(&object)?.replace('\'', "''")))
        }
        "journal-fence" => Some("json_insert(NEW.payload, '$.journal.fences[#]', json('{\"invocation_id\":\"unrelated-invocation\",\"attempt_id\":\"unrelated-attempt\"}'))".to_owned()),
        _ => None,
    };
    if let Some(expression) = mutation {
        // Actual SQLite DML corrupts only the restoring epoch's committed bytes.
        // UPDATE OF revision avoids recursion from the trigger's payload update.
        // https://www.sqlite.org/lang_createtrigger.html
        fixture.db.execute_unprepared(&format!(
            "CREATE TRIGGER native36_readback_tamper AFTER UPDATE OF revision ON bro_managed_sessions WHEN NEW.revision = {} BEGIN UPDATE bro_managed_sessions SET payload = {expression}, payload_bytes = length(CAST({expression} AS BLOB)) WHERE id = NEW.id; END",
            revision.checked_add(1).context("revision overflow")?
        )).await?;
    }
    let resources = fixture
        .service
        .hold_host(crate::tenancy::hosts::HostBoundary::Resources)?;
    let opener = AbortOnDropHandle::new(tokio::spawn(async move {
        NativeSession::open_authenticated(
            app,
            headers,
            "readback",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await
    }));
    tokio::time::timeout(TIMEOUT, resources.entered.notified()).await?;
    let commit = fixture.service.cleanup.hold(Phase::Committing);
    resources.release();
    tokio::time::timeout(TIMEOUT, commit.entered.notified()).await?;
    // Suppress the initial restoring write's reply after a real confirmed COMMIT.
    // Management cannot run on this same actor while COMMIT is held; use the
    // existing postcommit-expiry fixture technique, not a fabricated SQL outcome.
    let expired = crate::tenancy::model::Deadline::parse(
        (Utc::now() - chrono::Duration::seconds(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            .try_into()?,
    )?;
    {
        let mut state = fixture
            .service
            .snapshot
            .write()
            .map_err(|_| anyhow::anyhow!("snapshot lock"))?;
        Arc::make_mut(&mut state)
            .credentials
            .get_mut(&key.credential_id)
            .context("credential")?
            .expires = expired;
    }
    fixture.service.cleanup.release_gate();
    let error = tokio::time::timeout(TIMEOUT, opener)
        .await??
        .err()
        .context("expired restoring opener returned a session")?;
    ensure!(error.commit_status == bitrouter_ai::core::protocol::CommitStatus::Committed);
    if mode == "journal-exact" {
        tokio::time::timeout(TIMEOUT, async {
            while fixture.service.host_slots_for_test() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        ensure!(fixture.service.current().is_ok());
        let (after, saved) = row(&fixture).await?;
        ensure!(after == revision + 2 && saved["journal"]["released"] == true);
        ensure!(saved["journal"]["checkpoint"] == checkpoint);
        fixture.service.shutdown().await?;
        fixture.service.cleanup.ensure_clean()?;
    } else {
        tokio::time::timeout(TIMEOUT, fixture.service.cleanup.quarantined()).await?;
        let (after, saved) = row(&fixture).await?;
        ensure!(
            after == revision + 1,
            "mismatched readback wrote a release or retried the epoch"
        );
        ensure!(saved["descriptor"] == baseline["descriptor"]);
        ensure!(saved["journal"]["checkpoint"] == checkpoint);
        ensure!(saved["journal"]["head"] == baseline["journal"]["head"]);
        ensure!(saved["journal"]["released"] == false);
        if mode == "journal-artifact" {
            ensure!(
                saved["journal"]["artifacts"]["readback-probe"]["reference"]["artifact_id"]
                    == "readback-probe"
            );
        } else {
            ensure!(
                saved["journal"]["fences"]
                    .as_array()
                    .context("fences")?
                    .iter()
                    .any(|fence| fence["invocation_id"] == "unrelated-invocation")
            );
        }
        ensure!(fixture.service.current().is_err());
        ensure!(fixture.service.host_slots_for_test() == 0);
        ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
        ensure!(fixture.service.completion.clone().now_or_never().is_none());
        no_clean_drain(&fixture).await?;
        ensure!(
            row(&fixture).await? == (after, saved),
            "unconfirmed cleanup modified journal evidence"
        );
    }
    ensure!(
        provider.calls() == 1,
        "restoration admitted another provider attempt"
    );
    fixture.db.clone().close().await?;
    provider.finish().await?;
    // The existing isolated parent checks file custody after these ordinary
    // handles drop, and releases the runtime only after that external probe.
    Ok(())
}

#[tokio::test]
async fn native36_readback_requires_the_whole_retained_journal_not_only_checkpoint() -> Result<()> {
    for mode in ["journal-exact", "journal-artifact", "journal-fence"] {
        isolated(mode).await?;
    }
    Ok(())
}
