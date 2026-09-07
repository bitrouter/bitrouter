//! Exercise the real binary entry point and SDK-style termination, without a
//! model request or access to the user's native sessions.
#![cfg(unix)]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter::session_evidence::service::{EvidenceHandle, EvidenceLaunch};
use bitrouter::session_evidence::{claude_proxy, types::Harness};
use bitrouter_sdk::acp::controller::{ControllerIdentity, SessionObservation, SessionObserver};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

struct NativeCleanup(rustix::process::Pid);
impl Drop for NativeCleanup {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process(self.0, rustix::process::Signal::KILL);
    }
}

#[tokio::test]
async fn term_reaps_unresponsive_native_child_before_sdk_five_second_kill() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = std::fs::canonicalize(directory.path())?;
    let spool = root.join("spool");
    std::fs::create_dir(&spool)?;
    let profile = root.join("profile");
    std::fs::create_dir(&profile)?;
    let namespace =
        bitrouter::eval::types::canonical_digest(&(Harness::ClaudeCode, profile.join("projects")))?;
    let native = root.join("claude");
    std::fs::write(
        &native,
        "#!/bin/sh\ntrap '' TERM\necho $$ > \"$BITROUTER_TEST_NATIVE_PID\"\nprintf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"fixture\",\"claude_code_version\":\"2.1.257\"}'\nwhile :; do read -r line || sleep 1; done\n",
    )?;
    std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o700))?;
    let binary = env!("CARGO_BIN_EXE_bitrouter");
    let alias = root.join(claude_proxy::PROXY_NAME);
    std::os::unix::fs::symlink(binary, &alias)?;
    let pid_file = root.join("native.pid");
    let mut proxy = Command::new(&alias)
        .args([
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ])
        .env("CLAUDE_CONFIG_DIR", &profile)
        .env("CLAUDE_CODE_EXECUTABLE", &alias)
        .env(claude_proxy::SPOOL_ENV, &spool)
        .env(claude_proxy::NAMESPACE_ENV, namespace)
        .env(claude_proxy::UPSTREAM_ENV, &native)
        .env("BITROUTER_TEST_NATIVE_PID", &pid_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = proxy.stdout.take().context("proxy stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await??
        .context("native init")?;
    assert_eq!(
        serde_json::from_str::<Value>(&line)?["session_id"],
        "fixture"
    );
    let native_pid = std::fs::read_to_string(pid_file)?.trim().parse::<i32>()?;
    let cleanup = NativeCleanup(rustix::process::Pid::from_raw(native_pid).context("native pid")?);
    let pid = rustix::process::Pid::from_raw(i32::try_from(proxy.id().context("proxy pid")?)?)
        .context("proxy pid")?;
    let started = tokio::time::Instant::now();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
    // A second termination signal must not restart the grace period and push
    // child cleanup beyond the SDK's original five-second deadline.
    tokio::time::sleep(Duration::from_secs(2)).await;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
    let status = tokio::time::timeout_at(started + Duration::from_millis(4500), proxy.wait())
        .await
        .context("proxy exceeded SDK kill deadline")??;
    ensure!(
        !status.success(),
        "forced termination must not report a clean exit"
    );
    let probe = Command::new("kill")
        .args(["-0", &native_pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    assert!(
        !probe.success(),
        "native CLI remained alive after proxy shutdown"
    );
    std::mem::forget(cleanup); // The owned process was reaped; do not target a reused pid.
    let files = std::fs::read_dir(spool)?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(files.len(), 1);
    let saved = std::fs::read_to_string(files[0].path())?;
    let stopped: Value = serde_json::from_str(saved.lines().last().context("last event")?)?;
    assert_eq!(stopped["method"], "runtime/stopped");
    assert_eq!(stopped["clean"], false);
    assert_eq!(stopped["exit_code"], 137);
    Ok(())
}

#[tokio::test]
async fn private_alias_preserves_native_auth_probes_without_capturing_credentials() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = std::fs::canonicalize(directory.path())?;
    let alias = root.join(claude_proxy::PROXY_NAME);
    let binary = env!("CARGO_BIN_EXE_bitrouter");
    std::os::unix::fs::symlink(binary, &alias)?;
    let spool = root.join("spool");
    std::fs::create_dir(&spool)?;
    let native = root.join("claude");
    std::fs::write(
        &native,
        "#!/bin/sh\n[ \"$1\" = auth ] || exit 91\n[ \"$CLAUDE_CODE_EXECUTABLE\" = \"$0\" ] || exit 92\n[ -z \"$BITROUTER_CLAUDE_EVIDENCE_SPOOL\" ] || exit 93\nprintf '%s' '{\"loggedIn\":true,\"fixtureCredential\":\"private\"}'\nexit 7\n",
    )?;
    std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o700))?;
    for args in [vec!["auth", "status", "--json"], vec!["auth", "logout"]] {
        let output = Command::new(&alias)
            .args(args)
            .env("CLAUDE_CODE_EXECUTABLE", &alias)
            .env(claude_proxy::SPOOL_ENV, &spool)
            .env(claude_proxy::UPSTREAM_ENV, &native)
            .output()
            .await?;
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout)?["fixtureCredential"],
            "private"
        );
    }
    assert_eq!(std::fs::read_dir(spool)?.count(), 0);
    // The real BitRouter executable must still parse its own commands even
    // when an MCP server inherits every private adapter environment variable.
    let output = Command::new(binary)
        .arg("--version")
        .env(claude_proxy::SPOOL_ENV, &root)
        .env(claude_proxy::UPSTREAM_ENV, native)
        .output()
        .await?;
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)?.starts_with("bitrouter "));
    Ok(())
}

#[tokio::test]
async fn real_proxy_header_binds_the_controllers_committed_configuration() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = std::fs::canonicalize(directory.path())?;
    let native = root.join("claude");
    std::fs::write(
        &native,
        "#!/bin/sh\n[ -z \"$BITROUTER_CLAUDE_EVIDENCE_ORIGIN\" ] || exit 93\nprintf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"native-session\",\"claude_code_version\":\"2.1.257\"}'\n",
    )?;
    std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o700))?;
    let mut env = HashMap::from([
        (
            "CLAUDE_CONFIG_DIR".into(),
            root.join("default-profile").to_string_lossy().into_owned(),
        ),
        (
            "CLAUDE_CODE_EXECUTABLE".into(),
            native.to_string_lossy().into_owned(),
        ),
    ]);
    let mut handle = EvidenceHandle::open(EvidenceLaunch {
        home: &root.join("router"),
        database_url: "sqlite:evidence.db?mode=rwc",
        identity: &ControllerIdentity::new(
            "claude-acp",
            "@agentclientprotocol/claude-agent-acp",
            "0.75.1",
        ),
        env: &mut env,
        strip_inherited_env: &[],
    })
    .await?
    .context("evidence service")?;
    handle
        .service
        .observe(SessionObservation {
            operation_id: "create".into(),
            method: "session/new".into(),
            phase: "request".into(),
            payload: json!({"cwd":root,"mcpServers":[]}),
        })
        .await?;
    let params = handle
        .service
        .prepare_session_request(
            "create",
            "session/new",
            json!({
                "cwd":root,"mcpServers":[],"_meta":{"claudeCode":{"options":{"env":{
                    "CLAUDE_CONFIG_DIR":root.join("session-profile")}}}}
            }),
        )
        .await?;
    let subprocess_env = params
        .pointer("/_meta/claudeCode/options/env")
        .and_then(Value::as_object)
        .context("prepared environment")?;
    // EvidenceHandle executes inside this integration-test binary. Select the
    // actual application binary while retaining all controller-prepared env.
    let alias = root.join(claude_proxy::PROXY_NAME);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_bitrouter"), &alias)?;
    let mut command = Command::new(&alias);
    command
        .args([
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ])
        .envs(env)
        .env("CLAUDE_CODE_EXECUTABLE", &alias)
        .current_dir(&root)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (key, value) in subprocess_env {
        command.env(key, value.as_str().context("subprocess env string")?);
    }
    let output = tokio::time::timeout(Duration::from_secs(10), command.output()).await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout)?["session_id"],
        "native-session"
    );
    let snapshot = handle.service.reconcile().await?;
    assert_eq!(snapshot.processes.len(), 1);
    let binding = &snapshot.processes[0];
    assert!(binding.gaps.is_empty(), "{:?}", binding.gaps);
    let configured = binding
        .configured_by
        .as_ref()
        .context("verified process origin")?;
    assert_eq!(configured.operation_id, "create");
    assert_eq!(configured.method, "session/new");
    assert_ne!(
        configured.request.range.source_id,
        configured.configuration.range.source_id
    );
    assert!(
        snapshot.attempts.is_empty(),
        "process start cannot fabricate a task"
    );
    handle.shutdown().await?;
    Ok(())
}
