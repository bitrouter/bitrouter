use std::fs;
use std::process::{Command, Stdio};

use crate::runtime::error::{Result, RuntimeError};
use crate::runtime::paths::RuntimePaths;

/// Spawn the bitrouter server as a detached daemon process.
///
/// Creates a new process group via `process_group(0)` so the child survives
/// parent exit. Stdout/stderr are redirected to log files under `paths.log_dir`.
pub(crate) fn spawn_daemon(paths: &RuntimePaths) -> Result<u32> {
    use std::os::unix::process::CommandExt;

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
        .process_group(0)
        .spawn()?;

    Ok(child.id())
}

/// Check whether a process with the given PID is alive.
pub(crate) fn is_process_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) only checks process existence without sending a signal.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Send SIGTERM to request graceful shutdown.
pub(crate) fn signal_stop(pid: u32) -> Result<()> {
    // SAFETY: sending SIGTERM to a process ID is well-defined.
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if result != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(RuntimeError::Daemon(format!(
            "failed to send SIGTERM to pid {pid}: {err}"
        )));
    }
    Ok(())
}

/// Send SIGHUP to request configuration reload.
pub(crate) fn signal_reload(pid: u32) -> Result<()> {
    // SAFETY: sending SIGHUP to a process ID is well-defined.
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGHUP) };
    if result != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(RuntimeError::Daemon(format!(
            "failed to send SIGHUP to pid {pid}: {err}"
        )));
    }
    Ok(())
}

/// Send SIGKILL to force-terminate the process.
pub(crate) fn signal_kill(pid: u32) -> Result<()> {
    // SAFETY: sending SIGKILL to a process ID is well-defined.
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    if result != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(RuntimeError::Daemon(format!(
            "failed to send SIGKILL to pid {pid}: {err}"
        )));
    }
    Ok(())
}
