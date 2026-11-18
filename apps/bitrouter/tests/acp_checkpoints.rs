//! Historical checkpoint readback after stopping M0 and staging its owner stores.

#[path = "../src/acp_trajectory/fixtures.rs"]
mod capture_fixtures;

use std::path::Path;

use anyhow::{Context, Result, ensure};
use bitrouter::acp_trajectory::{CanonicalStore, checkpoint::types::RevisionInput};
use bitrouter::storage;
use bitrouter::storage::{
    capture::{CaptureDirection, CaptureEvent, CaptureKind},
    session_identity::SessionIdentity,
};
use capture_fixtures::{FixtureScope, PersistedHistory};
use sea_orm::{
    ConnectionTrait, DatabaseConnection,
    sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use serde_json::{Value, json};

async fn open(path: &Path, readonly: bool) -> Result<DatabaseConnection> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .read_only(readonly);
    Ok(SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?
        .into())
}

struct Evidence {
    identity: SessionIdentity,
    checkpoint: String,
    next: String,
    content: Value,
    revision: Value,
    resource: Value,
}

async fn record_m0(path: &Path) -> Result<Evidence> {
    std::fs::File::create_new(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let db = open(path, false).await?;
    #[derive(serde::Deserialize)]
    struct Object {
        r#type: String,
        sql: String,
    }
    let catalog: Vec<Object> = serde_json::from_str(include_str!(
        "../src/db/migration/baseline/fixtures/sqlite-current-schema.json"
    ))?;
    for kind in ["table", "index"] {
        for object in catalog.iter().filter(|o| o.r#type == kind) {
            db.execute_unprepared(&object.sql).await?;
        }
    }
    db.execute_unprepared(
        "INSERT INTO seaql_migrations VALUES ('m20261118_000001_model_policy_store',1);
         INSERT INTO bro_runtime_ownership(id,generation) VALUES(1,0)",
    )
    .await?;
    let identity = SessionIdentity {
        owner: "local".into(),
        source: "fixture".into(),
        native_session_id: "native".into(),
    };
    let recorder = PersistedHistory::new(
        db.clone(),
        FixtureScope {
            owner: identity.owner.clone(),
            source: identity.source.clone(),
            controller_instance_id: None,
            route_scope_id: None,
        },
    )
    .await?;
    for (kind, payload) in [
        (CaptureKind::Request, json!({"cwd":"/fixture"})),
        (
            CaptureKind::Response,
            json!({"result":{"sessionId":"native"}}),
        ),
    ] {
        recorder
            .append(
                "native",
                CaptureEvent {
                    direction: CaptureDirection::Client,
                    kind,
                    call_id: Some(1),
                    method: "session/new".into(),
                    payload,
                },
            )
            .await?;
    }
    let store = CanonicalStore::new(db.clone());
    let checkpoint = store.freeze_checkpoint(&identity, 1).await?.checkpoint_id;
    let content = store.checkpoint_content(&identity, &checkpoint).await?;
    ensure!(content.events.len() == 1 && content.setup.len() == 1);
    let input: RevisionInput = serde_json::from_value(json!({
        "submission_id":"manual-1", "checkpoint_id":checkpoint, "expected_revision":null,
        "source":"human", "evaluator_id":"local-user", "evaluator_version":"1", "reason":"manual assessment",
        "assessment":{"pipeline_config_digest":"a".repeat(64), "selection_digest":"b".repeat(64),
            "scores":{"correctness":{"status":"unknown"}}, "evidence":[], "explanation":"Evidence is incomplete"}
    }))?;
    let revision = serde_json::to_value(store.submit_assessment(&identity, input.clone()).await?)?;
    assert_eq!(
        serde_json::to_value(store.submit_assessment(&identity, input).await?)?,
        revision
    );
    let resource = store
        .observe_checkpoint_resources(&identity, &checkpoint)
        .await?;
    ensure!(!resource.metering_complete);
    ensure!(
        store
            .checkpoint_family(&identity)
            .await?
            .current_assessments
            == 1
    );
    recorder.append("native", CaptureEvent {
        direction: CaptureDirection::Agent,
        kind: CaptureKind::Notification,
        call_id: None,
        method: "session/update".into(),
        payload: json!({
            "sessionId":"native",
            "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"additional content"}}
        }),
    }).await?;
    ensure!(store.effective_assessment(&identity).await?.stale);
    ensure!(store.freeze_checkpoint(&identity, 1).await.is_err());
    let next = store.freeze_checkpoint(&identity, 2).await?;
    ensure!(next.previous_checkpoint_id.as_ref() == Some(&checkpoint));
    db.close().await?;
    Ok(Evidence {
        identity,
        checkpoint,
        next: next.checkpoint_id,
        content: serde_json::to_value(content)?,
        revision,
        resource: serde_json::to_value(resource)?,
    })
}

async fn readback(path: &Path, saved: &Evidence) -> Result<()> {
    let db = open(path, true).await?;
    let store = CanonicalStore::new(db.clone());
    let id = &saved.identity;
    let cp = &saved.checkpoint;
    let checkpoints = store.checkpoints(id).await?;
    ensure!(
        checkpoints.len() == 2
            && checkpoints[0].checkpoint_id == *cp
            && checkpoints[1].checkpoint_id == saved.next
            && checkpoints[1].previous_checkpoint_id.as_ref() == Some(cp)
    );
    assert_eq!(
        serde_json::to_value(store.checkpoint_content(id, cp).await?)?,
        saved.content
    );
    assert_eq!(
        serde_json::to_value(store.assessment_history(id).await?)?,
        json!([saved.revision])
    );
    assert_eq!(
        serde_json::to_value(store.checkpoint_resource_history(id, cp).await?)?,
        json!([saved.resource])
    );
    let effective = store.effective_assessment(id).await?;
    ensure!(
        effective.stale
            && effective.current_revision.as_deref() == saved.revision["revision_id"].as_str()
    );
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn retained_checkpoint_history_reopens_and_honors_deletion() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history.db");
    let saved = record_m0(&path).await?;
    readback(&path, &saved).await?;
    let db = open(&path, false).await?;
    let store = CanonicalStore::new(db.clone());
    store.delete(&saved.identity).await?;
    db.close().await?;
    let db = open(&path, true).await?;
    let error = CanonicalStore::new(db.clone())
        .checkpoint_content(&saved.identity, &saved.checkpoint)
        .await
        .err()
        .context("reader returned deleted content")?;
    ensure!(format!("{error:#}").contains("deleted"), "{error:#}");
    db.close().await?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn cli_preserves_checkpoints_and_revisions_across_processes() -> Result<()> {
    use bitrouter::db::mixed_store::StagedStores;
    use std::time::Duration;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("history.db");
    let saved = record_m0(&source).await?;
    let original = std::fs::read(&source)?;
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"))
            .current_dir(dir.path())
            .args([
                "tenancy",
                "offline",
                "split-store",
                "--database",
                "history.db",
                "--destination",
                "staged",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    ensure!(
        result.status.success(),
        "split-store: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let staged: StagedStores = serde_json::from_slice(&result.stdout)?;
    ensure!(!staged.dispatch_allowed && staged.bro_rows["acp_checkpoints"] == 2);
    let gateway = open(&staged.bitrouter, true).await?;
    let capture_table = gateway
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT name FROM sqlite_schema WHERE name IN
         ('acp_checkpoints','acp_capture_connections','acp_assessment_revisions')",
        ))
        .await?;
    ensure!(
        capture_table.is_none(),
        "gateway acquired historical capture tables"
    );
    gateway.close().await?;
    for path in [&staged.rollback, &staged.bro] {
        readback(path, &saved).await?;
    }
    ensure!(std::fs::read(&source)? == original && std::fs::read(staged.rollback)? == original);
    Ok(())
}
