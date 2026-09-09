//! Persistent checkpoint/assessment behavior through separate CLI processes.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter::acp_trajectory::{CanonicalStore, RecordingScope};
use bitrouter_sdk::acp::capture::{CaptureDirection, CaptureEvent, CaptureKind, CapturePort};
use serde_json::{Value, json};

async fn command(config: &Path, cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    Ok(tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"))
            .current_dir(cwd)
            .args([
                "acp",
                "checkpoints",
                "--agent",
                "fixture",
                "native",
                "--config",
            ])
            .arg(config)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await??)
}

async fn run(config: &Path, cwd: &Path, args: &[&str]) -> Result<Value> {
    let result = command(config, cwd, args).await?;
    ensure!(
        result.status.success(),
        "checkpoint CLI failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(serde_json::from_slice(&result.stdout)?)
}

#[tokio::test]
async fn cli_preserves_checkpoints_and_revisions_across_processes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let cwd = temp.path().join("coding-directory");
    std::fs::create_dir(&cwd)?;
    let config = temp.path().join("bitrouter.yaml");
    std::fs::write(&config, "database:\n  url: sqlite://history.db?mode=rwc\n")?;
    let url = bitrouter::db::anchor_url("sqlite://history.db?mode=rwc", temp.path());
    let db = bitrouter::db::connect(&url).await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = CanonicalStore::new(db);
    let recorder = store
        .recorder(RecordingScope {
            owner: "local".into(),
            source: "fixture".into(),
            controller_instance_id: None,
            route_scope_id: None,
        })
        .await?;
    for (kind, payload) in [
        (CaptureKind::Request, json!({"cwd":"/fixture"})),
        (
            CaptureKind::Response,
            json!({"result":{"sessionId":"native"}}),
        ),
    ] {
        recorder
            .record(CaptureEvent {
                direction: CaptureDirection::Client,
                kind,
                call_id: Some(1),
                method: "session/new".into(),
                payload,
            })
            .await?;
    }
    let created = run(&config, &cwd, &["create", "--watermark", "1"]).await?;
    let cp = created["data"]["checkpoint_id"]
        .as_str()
        .context("checkpoint ID")?;
    let content = run(&config, &cwd, &["show", cp]).await?;
    assert_eq!(
        content["data"]["events"]
            .as_array()
            .context("events")?
            .len(),
        1
    );
    assert_eq!(
        content["data"]["setup"].as_array().context("setup")?.len(),
        1
    );
    assert!(temp.path().join("history.db").exists());
    assert!(!cwd.join("history.db").exists());
    let submission = temp.path().join("assessment.json");
    std::fs::write(
        &submission,
        serde_json::to_vec(&json!({
            "submission_id":"manual-1", "checkpoint_id":cp, "expected_revision":null,
            "source":"human", "evaluator_id":"local-user", "evaluator_version":"1", "reason":"manual assessment",
            "assessment":{"pipeline_config_digest":"a".repeat(64), "selection_digest":"b".repeat(64),
                "scores":{"correctness":{"status":"unknown"}}, "evidence":[], "explanation":"Evidence is incomplete"}
        }))?,
    )?;
    let input_path = submission.to_str().context("input path")?;
    let submitted = run(&config, &cwd, &["submit", input_path]).await?;
    assert_eq!(
        run(&config, &cwd, &["submit", input_path]).await?,
        submitted
    );
    assert_eq!(
        run(&config, &cwd, &["history"]).await?["data"]
            .as_array()
            .context("history")?
            .len(),
        1
    );
    assert_eq!(
        run(&config, &cwd, &["effective"]).await?["data"]["current_revision"],
        submitted["data"]["revision_id"]
    );
    assert_eq!(
        run(&config, &cwd, &["resources", cp, "--refresh"]).await?["data"]["metering_complete"],
        false
    );
    assert_eq!(
        run(&config, &cwd, &["resources", cp]).await?["data"]
            .as_array()
            .context("resources")?
            .len(),
        1
    );
    assert_eq!(
        run(&config, &cwd, &["family"]).await?["data"]["current_assessments"],
        1
    );
    assert_eq!(
        run(&config, &cwd, &["list"]).await?["data"]
            .as_array()
            .context("checkpoints")?
            .len(),
        1
    );
    recorder.record(CaptureEvent { direction: CaptureDirection::Agent, kind: CaptureKind::Notification, call_id: None,
        method: "session/update".into(), payload: json!({"sessionId":"native","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"additional content"}}}) }).await?;
    assert_eq!(
        run(&config, &cwd, &["effective"]).await?["data"]["stale"],
        true
    );
    assert!(
        !command(&config, &cwd, &["create", "--watermark", "1"])
            .await?
            .status
            .success()
    );
    let next = run(&config, &cwd, &["create", "--watermark", "2"]).await?;
    assert_eq!(next["data"]["previous_checkpoint_id"], cp);
    assert_eq!(run(&config, &cwd, &["show", cp]).await?, content);
    let deleted = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"))
        .current_dir(&cwd)
        .args(["acp", "recordings", "--config"])
        .arg(&config)
        .args(["delete", "--agent", "fixture", "native"])
        .output()
        .await?;
    ensure!(deleted.status.success(), "recording deletion failed");
    assert!(
        !command(&config, &cwd, &["show", cp])
            .await?
            .status
            .success()
    );
    Ok(())
}
