use super::*;

async fn launch_background(
    mock: &MockAcp,
    extra: &[&str],
) -> Result<(String, std::process::Output)> {
    let mut command = mock.bro_command()?;
    command.args([
        "run",
        "stub",
        "background acceptance",
        "--direct",
        "--background",
        "--json",
    ]);
    command.args(extra);
    let output = tokio::time::timeout(PTY_TIMEOUT, command.output()).await??;
    ensure!(
        output.status.success(),
        "background launch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let run_id = report
        .get("agent_run_id")
        .and_then(serde_json::Value::as_str)
        .context("background report omitted agent_run_id")?
        .to_string();
    Ok((run_id, output))
}

async fn wait_for_run(
    mock: &MockAcp,
    run_id: &str,
    ready: impl Fn(&bitrouter::supervisor::RunSnapshot) -> bool,
) -> Result<bitrouter::supervisor::RunSnapshot> {
    tokio::time::timeout(PTY_TIMEOUT, async {
        loop {
            if let Some(run) = mock
                .supervised_runs()
                .await?
                .into_iter()
                .find(|run| run.run_id == run_id && ready(run))
            {
                return Ok(run);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

fn selected_option(outcome: &serde_json::Value) -> Option<&str> {
    outcome
        .get("optionId")
        .or_else(|| outcome.get("option_id"))
        .and_then(serde_json::Value::as_str)
}

#[tokio::test]
async fn background_permissions_default_to_ask_and_explicit_modes_win() -> Result<()> {
    let ask = MockAcp::new(MockScenario::OverlappingPermissions)?;
    let (run_id, _) = launch_background(&ask, &[]).await?;
    let run = wait_for_run(&ask, &run_id, |run| {
        run.attention == bitrouter::supervisor::AttentionState::Permission
            && run.pending_permissions.len() == 2
    })
    .await?;
    assert_eq!(run.turn, bitrouter::supervisor::TurnState::Working);
    assert!(ask.captured_permission_outcomes()?.is_empty());

    let denied = MockAcp::new(MockScenario::OverlappingPermissions)?;
    let (run_id, _) = launch_background(&denied, &["--deny-all"]).await?;
    let run = wait_for_run(&denied, &run_id, |run| {
        run.turn == bitrouter::supervisor::TurnState::Idle
    })
    .await?;
    assert_eq!(run.attention, bitrouter::supervisor::AttentionState::Result);
    let outcomes = denied.wait_for_permission_outcomes()?;
    ensure!(
        outcomes
            .iter()
            .all(|(_, outcome)| selected_option(outcome).is_some_and(|id| id.starts_with("deny-"))),
        "deny-all selected an unexpected option: {outcomes:?}"
    );

    let reads = MockAcp::new(MockScenario::OverlappingPermissions)?;
    let (run_id, _) = launch_background(&reads, &["--approve-reads"]).await?;
    let _ = wait_for_run(&reads, &run_id, |run| {
        run.turn == bitrouter::supervisor::TurnState::Idle
    })
    .await?;
    let outcomes = reads.wait_for_permission_outcomes()?;
    ensure!(
        outcomes
            .iter()
            .all(|(_, outcome)| selected_option(outcome).is_some_and(|id| id.starts_with("deny-"))),
        "unlabelled calls were not denied by approve-reads: {outcomes:?}"
    );

    let mixed = MockAcp::new(MockScenario::OverlappingPermissions)?;
    let policy = r#"{"autoApprove":["fixture permission 1"]}"#;
    let (run_id, _) = launch_background(&mixed, &["--permission-policy", policy]).await?;
    let run = wait_for_run(&mixed, &run_id, |run| {
        run.attention == bitrouter::supervisor::AttentionState::Permission
            && run.pending_permissions.len() == 1
    })
    .await?;
    let pending = &run.pending_permissions[0];
    assert_eq!(pending.tool_call.tool_call_id.0.as_ref(), "tool-2");
    assert_eq!(
        pending.tool_call.fields.title.as_deref(),
        Some("fixture permission 2")
    );
    assert_eq!(
        pending
            .options
            .iter()
            .map(|option| option.option_id.0.as_ref())
            .collect::<Vec<_>>(),
        ["allow-2", "deny-2"]
    );
    assert_eq!(
        pending
            .options
            .iter()
            .map(|option| option.name.as_str())
            .collect::<Vec<_>>(),
        ["Allow fixture 2", "Deny fixture 2"]
    );
    let outcomes = mixed.captured_permission_outcomes()?;
    ensure!(
        outcomes.len() == 1 && selected_option(&outcomes[0].1) == Some("allow-1"),
        "policy match did not approve exactly the named request: {outcomes:?}"
    );
    Ok(())
}

#[tokio::test]
async fn permission_wait_counts_toward_background_turn_timeout() -> Result<()> {
    let mock = MockAcp::new(MockScenario::OverlappingPermissions)?;
    let (run_id, _) = launch_background(&mock, &["--turn-timeout", "1"]).await?;
    let run = wait_for_run(&mock, &run_id, |run| {
        run.failure
            .as_deref()
            .is_some_and(|failure| failure.contains("timed out"))
    })
    .await?;
    assert_eq!(run.attention, bitrouter::supervisor::AttentionState::Error);
    assert!(run.pending_permissions.is_empty());
    Ok(())
}

#[tokio::test]
async fn schema_failure_never_becomes_ready_for_review() -> Result<()> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let schema = r#"{"type":"object","required":["ok"]}"#;
    let (run_id, _) = launch_background(&mock, &["--result-schema", schema]).await?;
    let run = wait_for_run(&mock, &run_id, |run| {
        run.turn == bitrouter::supervisor::TurnState::Idle
    })
    .await?;
    assert_eq!(run.attention, bitrouter::supervisor::AttentionState::Error);
    assert_eq!(
        run.failure.as_deref(),
        Some("no JSON result found in the reply")
    );
    Ok(())
}

#[tokio::test]
async fn background_load_and_resume_use_advertised_native_capabilities() -> Result<()> {
    let loaded = MockAcp::new(MockScenario::SessionLifecycle)?;
    let (loaded_run_id, _) =
        launch_background(&loaded, &["--load", "native-background-load"]).await?;
    let loaded_run = wait_for_run(&loaded, &loaded_run_id, |run| {
        run.turn == bitrouter::supervisor::TurnState::Idle
    })
    .await?;
    assert_eq!(
        loaded_run.native_session_id.as_deref(),
        Some("native-background-load")
    );
    assert_eq!(
        loaded_run.agent_session_id.as_deref(),
        Some("agent-a12-load")
    );
    let load_request = loaded.wait_for_request("session/load")?;
    ensure!(
        load_request["params"]["sessionId"] == "native-background-load",
        "background load changed its native session id: {load_request}"
    );
    assert!(loaded.captured_request("session/resume")?.is_none());

    let resumed = MockAcp::new(MockScenario::SessionLifecycle)?;
    let (resumed_run_id, _) =
        launch_background(&resumed, &["--resume", "native-background-resume"]).await?;
    let resumed_run = wait_for_run(&resumed, &resumed_run_id, |run| {
        run.turn == bitrouter::supervisor::TurnState::Idle
    })
    .await?;
    assert_eq!(
        resumed_run.native_session_id.as_deref(),
        Some("native-background-resume")
    );
    assert_eq!(
        resumed_run.agent_session_id.as_deref(),
        Some("agent-a12-resume")
    );
    let resume_request = resumed.wait_for_request("session/resume")?;
    ensure!(
        resume_request["params"]["sessionId"] == "native-background-resume",
        "background resume changed its native session id: {resume_request}"
    );
    assert!(resumed.captured_request("session/load")?.is_none());
    Ok(())
}

#[tokio::test]
async fn no_wait_is_rejected_and_non_tty_management_is_escape_free() -> Result<()> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let rejected = mock
        .bro_command()?
        .args([
            "run",
            "stub",
            "invalid combination",
            "--background",
            "--no-wait",
        ])
        .output()
        .await?;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("cannot be used with"));

    let (run_id, launched) = launch_background(&mock, &[]).await?;
    ensure!(
        !launched.stdout.contains(&0x1b) && !launched.stderr.contains(&0x1b),
        "background report emitted terminal control bytes"
    );
    let sessions = mock
        .bro_command()?
        .args(["agents", "sessions", "--json"])
        .output()
        .await?;
    ensure!(
        sessions.status.success(),
        "sessions failed: {}",
        String::from_utf8_lossy(&sessions.stderr)
    );
    ensure!(
        !sessions.stdout.contains(&0x1b) && !sessions.stderr.contains(&0x1b),
        "sessions JSON emitted terminal control bytes"
    );
    let report: serde_json::Value = serde_json::from_slice(&sessions.stdout)?;
    ensure!(
        report
            .get("runs")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|runs| {
                runs.iter().any(|run| {
                    run.get("run_id").and_then(serde_json::Value::as_str) == Some(run_id.as_str())
                })
            }),
        "sessions JSON omitted the accepted run: {report}"
    );

    let manager = mock.bro_command()?.args(["agents"]).output().await?;
    assert!(!manager.status.success());
    ensure!(
        !manager.stdout.contains(&0x1b) && !manager.stderr.contains(&0x1b),
        "non-TTY manager emitted terminal control bytes"
    );
    let manager_message = format!(
        "{}{}",
        String::from_utf8_lossy(&manager.stdout),
        String::from_utf8_lossy(&manager.stderr)
    );
    assert!(manager_message.contains("sessions --json"));

    let attach = mock
        .bro_command()?
        .args(["agents", "attach", &run_id])
        .output()
        .await?;
    assert!(!attach.status.success());
    ensure!(
        !attach.stdout.contains(&0x1b) && !attach.stderr.contains(&0x1b),
        "non-TTY attach emitted terminal control bytes"
    );
    Ok(())
}

#[tokio::test]
async fn remote_context_rejects_background_and_session_mutations_before_local_start() -> Result<()>
{
    let remote = RemoteFixture::new(RemoteScenario::Healthy)?;
    let invocations = [
        vec![
            "--context",
            "fixture",
            "run",
            "stub",
            "must remain remote",
            "--direct",
            "--background",
        ],
        vec!["--context", "fixture", "agents", "attach", "run-1"],
        vec!["--context", "fixture", "agents", "stop", "run-1"],
        vec!["--context", "fixture", "agents", "remove", "run-1"],
    ];

    for args in invocations {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"))
            .args(args)
            .env_clear()
            .env("PATH", inherited_path()?)
            .env("HOME", &remote.home_path)
            .env("BITROUTER_HOME", &remote.bitrouter_home_path)
            .env("TMPDIR", &remote.temporary_path)
            .env("NO_COLOR", "1")
            .env("BITROUTER_A12_TOKEN", REMOTE_FIXTURE_TOKEN)
            .current_dir(remote._directory.path())
            .kill_on_drop(true)
            .output()
            .await?;
        assert!(!output.status.success());
        let error = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            error.contains("local-only"),
            "remote execution was not rejected by the pre-dispatch gate: {error}"
        );
        ensure!(
            !error.contains("BITROUTER_HOME") && !error.contains("bitrouter.yaml"),
            "remote execution reached local config resolution: {error}"
        );
    }

    assert!(!remote.bitrouter_home_path.join("bitrouter.sock").exists());
    assert!(!remote.bitrouter_home_path.join("bitrouter.pid").exists());
    Ok(())
}

#[tokio::test]
async fn foreground_claim_rejects_a_background_subdirectory_of_the_same_worktree() -> Result<()> {
    let mock = MockAcp::new(MockScenario::DelayedNormal)?;
    let repository = mock._directory.path();
    let git = Command::new("git")
        .arg("init")
        .arg(repository)
        .output()
        .context("initializing fixture Git worktree")?;
    ensure!(
        git.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&git.stderr)
    );
    let subdirectory = repository.join("nested-background");
    std::fs::create_dir_all(&subdirectory)?;

    let mut code = CodeFixture::agent_with_mock(mock, None)?;
    code.wait_for_agent_ready()?;
    let output = code
        .mock
        .bro_command()?
        .args([
            "run",
            "stub",
            "must not share foreground worktree",
            "--direct",
            "--background",
            "--json",
            "--cwd",
        ])
        .arg(&subdirectory)
        .output()
        .await?;
    assert!(!output.status.success());
    let error = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        error.contains("worktree") || error.contains("directory") || error.contains("claimed"),
        "claim conflict was not explained: {error}"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[tokio::test]
async fn failed_run_stop_is_idempotent_and_remove_preserves_native_session() -> Result<()> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let (run_id, _) = launch_background(&mock, &[]).await?;
    let ready = wait_for_run(&mock, &run_id, |run| {
        run.turn == bitrouter::supervisor::TurnState::Idle
            && run.review == bitrouter::supervisor::ReviewState::Unread
    })
    .await?;
    let native_session_id = ready
        .native_session_id
        .clone()
        .context("ready run omitted its native saved session ID")?;

    mock.disconnect()?;
    let failed = wait_for_run(&mock, &run_id, |run| {
        run.process == bitrouter::supervisor::ProcessState::Failed
    })
    .await?;
    let original_failure = failed
        .failure
        .clone()
        .context("adapter disconnect did not retain a failure")?;
    assert_eq!(
        failed.native_session_id.as_deref(),
        Some(native_session_id.as_str())
    );

    let stopped = mock
        .bro_command()?
        .args(["agents", "stop", &run_id, "--json"])
        .output()
        .await?;
    ensure!(
        stopped.status.success(),
        "idempotent failed-run stop failed: {}{}",
        String::from_utf8_lossy(&stopped.stdout),
        String::from_utf8_lossy(&stopped.stderr)
    );
    let cleaned = wait_for_run(&mock, &run_id, |run| {
        run.process == bitrouter::supervisor::ProcessState::Failed
    })
    .await?;
    assert_eq!(cleaned.failure.as_deref(), Some(original_failure.as_str()));
    assert_eq!(
        cleaned.native_session_id.as_deref(),
        Some(native_session_id.as_str())
    );

    let removed = mock
        .bro_command()?
        .args(["agents", "remove", &run_id, "--json"])
        .output()
        .await?;
    ensure!(
        removed.status.success(),
        "failed-run removal failed: {}{}",
        String::from_utf8_lossy(&removed.stdout),
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(mock.supervised_runs().await?.is_empty());
    assert!(mock.captured_request("session/delete")?.is_none());
    Ok(())
}
