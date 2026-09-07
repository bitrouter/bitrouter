use super::*;
use crate::session_evidence::service::tests::{claude_service, observation};
use crate::session_evidence::types::{Artifact, WorkspaceEvidence};

async fn repository(directory: &Path) -> Result<PathBuf> {
    let code = directory.join("code");
    tokio::fs::create_dir(&code).await?;
    for args in [
        vec!["init", "--quiet"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "baseline",
        ],
    ] {
        ensure!(
            tokio::process::Command::new("git")
                .current_dir(&code)
                .args(args)
                .status()
                .await?
                .success(),
            "Git fixture"
        );
    }
    tokio::fs::write(code.join("main.rs"), b"fn main() {}\n").await?;
    Ok(code)
}

async fn load(service: &ControllerEvidence, operation: &str, params: Value) -> Result<()> {
    let session = params.get("sessionId").cloned().context("session")?;
    // Simulate the SDK observer's filtered request followed by preparation
    // with the complete original envelope, including native _meta options.
    service
        .observe(observation(
            operation,
            "session/load",
            "request",
            json!({"sessionId":session,"cwd":params.get("cwd")}),
        ))
        .await?;
    service
        .prepare_session_request(operation, "session/load", params)
        .await?;
    service
        .observe(observation(
            operation,
            "session/load",
            "response",
            json!({}),
        ))
        .await?;
    Ok(())
}

async fn prompt(
    service: &ControllerEvidence,
    operation: &str,
    session: &str,
) -> Result<WorkspaceEvidence> {
    service
        .observe(observation(
            operation,
            "session/prompt",
            "request",
            json!({"sessionId":session,"prompt":[]}),
        ))
        .await?;
    service
        .observe(observation(
            operation,
            "session/prompt",
            "response",
            json!({"stopReason":"end_turn"}),
        ))
        .await?;
    let attempt = service
        .store
        .attempts(None, 16)
        .await?
        .into_iter()
        .find(|attempt| attempt.root.native_id == session)
        .context("attempt")?;
    service.store.workspace_evidence(&attempt.root).await
}

async fn artifact(home: &Path, database_url: &str, id: &str) -> Result<Value> {
    let db = crate::db::connect(&crate::db::anchor_url(database_url, home)).await?;
    let rows = db.query_all(Statement::from_sql_and_values(DbBackend::Sqlite,
        "SELECT object_json FROM native_evidence_objects WHERE kind = 'workspace_artifact' AND object_key = ?", [id.into()])).await?;
    assert_eq!(rows.len(), 1);
    let object: Value = serde_json::from_str(&rows[0].try_get::<String>("", "object_json")?)?;
    let artifact: Artifact = serde_json::from_value(object["artifact"].clone())?;
    crate::session_evidence::workspace::Workspace::from_artifact(&artifact)?;
    Ok(serde_json::from_str(&artifact.content)?)
}

#[tokio::test]
async fn pending_result_keeps_its_original_native_exclusions_after_another_prompt_fails()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let code = repository(directory.path()).await?;
    let native = code.join("profile-b");
    tokio::fs::create_dir(&native).await?;
    tokio::fs::write(native.join("private-fixture"), b"native runtime content").await?;
    let mut handle = claude_service(directory.path()).await?;
    let service = &handle.service;
    load(
        service,
        "load-b",
        json!({"sessionId":"b", "cwd":code,
        "_meta":{"claudeCode":{"options":{"env":{"CLAUDE_CONFIG_DIR":native}}}}}),
    )
    .await?;
    for operation in ["one", "two"] {
        service
            .observe(observation(
                operation,
                "session/prompt",
                "request",
                json!({"sessionId":"b","prompt":[]}),
            ))
            .await?;
    }
    service
        .observe(observation(
            "one",
            "session/prompt",
            "response",
            json!({"error_code":-32603}),
        ))
        .await?;
    let response = observation(
        "two",
        "session/prompt",
        "response",
        json!({"stopReason":"end_turn"}),
    );
    assert_eq!(
        service.observation_context(&response).await?.1,
        "unresolved"
    );
    service.observe(response).await?;
    let attempt = service.store.attempts(None, 16).await?.remove(0);
    let evidence = service.store.workspace_evidence(&attempt.root).await?;
    let result = artifact(
        &directory.path().join("router"),
        "sqlite:evidence.db?mode=rwc",
        evidence
            .latest_prompt_result
            .as_deref()
            .context("result artifact")?,
    )
    .await?;
    assert!(
        result["exclusions"]
            .as_array()
            .context("exclusions")?
            .contains(&json!(tokio::fs::canonicalize(native).await?))
    );
    assert!(result["files"].get("profile-b/private-fixture").is_none());
    assert!(result["files"].get("main.rs").is_some());
    assert!(evidence.gaps.contains("workspace_runtime_data_excluded"));
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn additional_root_options_survive_preparation_and_possible_query_reuse() -> Result<()> {
    for (harness_id, package, version) in [
        (
            "claude-acp",
            "@agentclientprotocol/claude-agent-acp",
            "0.70.0",
        ),
        ("codex-acp", "@agentclientprotocol/codex-acp", "1.7.0"),
    ] {
        let directory = tempfile::tempdir()?;
        let code = repository(directory.path()).await?;
        let mut env = HashMap::from([
            (
                "CODEX_HOME".into(),
                directory
                    .path()
                    .join("codex")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("CODEX_PATH".into(), "/fixture/codex".into()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                directory
                    .path()
                    .join("claude")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]);
        let home = directory.path().join("router");
        let mut handle = EvidenceHandle::open(EvidenceLaunch {
            home: &home,
            database_url: "sqlite:evidence.db?mode=rwc",
            identity: &ControllerIdentity::new(harness_id, package, version),
            env: &mut env,
            strip_inherited_env: &[],
        })
        .await?
        .context("service")?;
        let service = &handle.service;
        load(
            service,
            "legacy",
            json!({"sessionId":"legacy", "cwd":code,
            "_meta":{"additionalRoots":[directory.path().join("extra")]}}),
        )
        .await?;
        assert!(
            prompt(service, "first", "legacy")
                .await?
                .gaps
                .contains("workspace_additional_directories_unavailable")
        );
        if harness_id == "claude-acp" {
            // The adapter can reuse this Query and ignore omitted new options.
            load(service, "reuse", json!({"sessionId":"legacy", "cwd":code})).await?;
            prompt(service, "second", "legacy").await?;
            assert!(service.state.lock().await.workspaces["legacy"].additional_directories);
            load(service, "sdk", json!({"sessionId":"sdk", "cwd":code,
                "_meta":{"claudeCode":{"options":{"additionalDirectories":[directory.path().join("sdk-extra")]}}}})).await?;
            assert!(
                prompt(service, "sdk-prompt", "sdk")
                    .await?
                    .gaps
                    .contains("workspace_additional_directories_unavailable")
            );
        }
        handle.shutdown().await?;
    }
    Ok(())
}

#[test]
fn acp_additional_directories_take_precedence_over_the_legacy_extension() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let params =
        json!({"cwd":cwd, "additionalDirectories":[], "_meta":{"additionalRoots":["legacy"]}});
    assert!(
        !workspace_scope(&params, BTreeSet::new())
            .context("scope")?
            .additional_directories
    );
    let params =
        json!({"cwd":cwd, "additionalDirectories":null, "_meta":{"additionalRoots":["legacy"]}});
    assert!(
        workspace_scope(&params, BTreeSet::new())
            .context("scope")?
            .additional_directories
    );
    Ok(())
}

#[tokio::test]
async fn external_sqlite_file_and_sidecars_are_excluded_using_the_engine_filename() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let code = repository(directory.path()).await?;
    let database_url = format!(
        "sqlite:{}?mode=rwc",
        code.join("evidence%20capture.db").display()
    );
    let home = directory.path().join("router");
    let mut env = HashMap::from([(
        "CLAUDE_CONFIG_DIR".into(),
        directory
            .path()
            .join("native")
            .to_string_lossy()
            .into_owned(),
    )]);
    let mut handle = EvidenceHandle::open(EvidenceLaunch {
        home: &home,
        database_url: &database_url,
        identity: &ControllerIdentity::new(
            "claude-acp",
            "@agentclientprotocol/claude-agent-acp",
            "0.70.0",
        ),
        env: &mut env,
        strip_inherited_env: &[],
    })
    .await?
    .context("service")?;
    load(
        &handle.service,
        "load",
        json!({"cwd":code,"sessionId":"root"}),
    )
    .await?;
    let evidence = prompt(&handle.service, "prompt", "root").await?;
    let result = artifact(
        &home,
        &database_url,
        evidence.latest_prompt_result.as_deref().context("result")?,
    )
    .await?;
    let root = tokio::fs::canonicalize(&code).await?;
    assert!(root.join("evidence capture.db").exists());
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let name = format!("evidence capture.db{suffix}");
        assert!(
            result["exclusions"]
                .as_array()
                .context("exclusions")?
                .contains(&json!(root.join(&name)))
        );
        assert!(result["files"].get(&name).is_none());
    }
    assert!(result["files"].get("main.rs").is_some());
    assert!(evidence.gaps.contains("workspace_runtime_data_excluded"));
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn unavailable_checkpoint_keeps_the_confirmed_attempt_visible() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let code = repository(directory.path()).await?;
    let mut handle = claude_service(directory.path()).await?;
    load(
        &handle.service,
        "load",
        json!({"sessionId":"root","cwd":code}),
    )
    .await?;
    let evidence = prompt(&handle.service, "prompt", "root").await?;
    let attempt = handle.service.store.attempts(None, 16).await?.remove(0);
    let db = crate::db::connect(&crate::db::anchor_url(
        "sqlite:evidence.db?mode=rwc",
        &directory.path().join("router"),
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "DELETE FROM native_evidence_objects WHERE kind = 'workspace_artifact' AND object_key = ?",
        [evidence.latest_prompt_result.context("checkpoint")?.into()],
    ))
    .await?;
    let snapshot = handle.service.reconcile().await?;
    assert!(snapshot.attempts.iter().any(|item| item.id == attempt.id));
    let partial = snapshot
        .workspace_checkpoints
        .get(&attempt.id)
        .context("partial checkpoint")?;
    assert!(partial.gaps.contains("workspace_checkpoint_invalid"));
    assert!(partial.baseline.is_none() && partial.latest_prompt_result.is_none());
    assert!(!snapshot.gaps.contains("native_task_state_invalid"));
    handle.shutdown().await?;
    Ok(())
}
