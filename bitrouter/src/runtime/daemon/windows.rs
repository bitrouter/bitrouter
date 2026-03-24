use std::fs;
use std::process::{Command, Stdio};

use crate::runtime::error::{Result, RuntimeError};
use crate::runtime::paths::RuntimePaths;

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Spawn the bitrouter server as a detached daemon process.
///
/// Uses `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` creation flags so the
/// child runs independently of the parent console/session.
pub(crate) fn spawn_daemon(paths: &RuntimePaths) -> Result<u32> {
    use std::os::windows::process::CommandExt;

    fs::create_dir_all(&paths.runtime_dir)?;
    fs::create_dir_all(&paths.log_dir)?;

    let stdout = fs::File::create(paths.log_dir.join("bitrouter.out.log"))?;
    let stderr = fs::File::create(paths.log_dir.join("bitrouter.err.log"))?;
    let exe = std::env::current_exe()?;

    let child = Command::new(exe)
        .arg("--home-dir")
        .arg(&paths.home_dir)
        .arg("serve")
        .stdout(stdout)
        .stderr(stderr)
        .stdin(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()?;

    Ok(child.id())
}

/// Check whether a process with the given PID is alive using `tasklist`.
pub(crate) fn is_process_alive(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|output| {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.contains(&pid.to_string())
        })
        .unwrap_or(false)
}

/// Request graceful shutdown via `taskkill`.
pub(crate) fn signal_stop(pid: u32) -> Result<()> {
    let output = Command::new("taskkill")
        .args(["/PID", &pid.to_string()])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not found") || stderr.contains("not running") {
            return Ok(());
        }
        return Err(RuntimeError::Daemon(format!(
            "failed to stop pid {pid}: {stderr}"
        )));
    }
    Ok(())
}

/// Request configuration reload by writing a flag file.
///
/// Windows has no SIGHUP equivalent, so a sentinel file in the runtime
/// directory signals the server to reload.
pub(crate) fn signal_reload(paths: &RuntimePaths) -> Result<()> {
    fs::create_dir_all(&paths.runtime_dir)?;
    fs::write(paths.runtime_dir.join("reload"), "")?;
    Ok(())
}

/// Force-terminate the process via `taskkill /F`.
pub(crate) fn signal_kill(pid: u32) -> Result<()> {
    let output = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not found") || stderr.contains("not running") {
            return Ok(());
        }
        return Err(RuntimeError::Daemon(format!(
            "failed to force-kill pid {pid}: {stderr}"
        )));
    }
    Ok(())
}
