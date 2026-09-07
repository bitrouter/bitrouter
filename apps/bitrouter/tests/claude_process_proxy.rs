//! Exercise the real binary entry point and SDK-style termination, without a
//! model request or access to the user's native sessions.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter::session_evidence::{claude_proxy, types::Harness};
use serde_json::Value;
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
    let pid_file = root.join("native.pid");
    let mut proxy = Command::new(binary)
        .args([
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ])
        .env("CLAUDE_CONFIG_DIR", &profile)
        .env("CLAUDE_CODE_EXECUTABLE", binary)
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
