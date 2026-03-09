mod pid;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

use pid::PidFile;

use std::time::Duration;

use crate::error::{Result, RuntimeError};
use crate::paths::RuntimePaths;

const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Manages the lifecycle of the bitrouter daemon process via PID-file tracking
/// and platform-specific process control.
pub struct DaemonManager {
    paths: RuntimePaths,
    pid_file: PidFile,
}

impl DaemonManager {
    pub fn new(paths: RuntimePaths) -> Self {
        let pid_file = PidFile::new(&paths.runtime_dir);
        Self { paths, pid_file }
    }

    /// Returns the PID of the running daemon, or `None` if it is not running.
    /// Cleans up stale PID files when the recorded process no longer exists.
    pub fn is_running(&self) -> Result<Option<u32>> {
        if let Some(pid) = self.pid_file.read()? {
            if platform::is_process_alive(pid) {
                return Ok(Some(pid));
            }
            // Stale PID file — clean up
            self.pid_file.remove()?;
        }
        Ok(None)
    }

    /// Spawn the daemon, returning the new PID.
    ///
    /// Fails if the daemon is already running.
    pub async fn start(&self) -> Result<u32> {
        if let Some(pid) = self.is_running()? {
            return Err(RuntimeError::Daemon(format!(
                "daemon is already running (pid {pid})"
            )));
        }

        let pid = platform::spawn_daemon(&self.paths)?;
        self.pid_file.write(pid)?;

        // Brief wait to verify the process didn't exit immediately.
        tokio::time::sleep(Duration::from_millis(200)).await;

        if !platform::is_process_alive(pid) {
            self.pid_file.remove()?;
            return Err(RuntimeError::Daemon(
                "daemon process exited immediately after start".into(),
            ));
        }

        tracing::info!(pid, "daemon started");
        Ok(pid)
    }

    /// Stop the running daemon gracefully (SIGTERM / taskkill), falling back to
    /// a forced kill after [`STOP_TIMEOUT`].
    pub async fn stop(&self) -> Result<()> {
        let pid = match self.is_running()? {
            Some(pid) => pid,
            None => {
                return Err(RuntimeError::Daemon("daemon is not running".into()));
            }
        };

        platform::signal_stop(pid)?;

        // Poll until the process exits or the timeout elapses.
        let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
        loop {
            if !platform::is_process_alive(pid) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(pid, "daemon did not stop gracefully, force killing");
                platform::signal_kill(pid)?;
                tokio::time::sleep(Duration::from_millis(500)).await;
                break;
            }
            tokio::time::sleep(STOP_POLL_INTERVAL).await;
        }

        self.pid_file.remove()?;
        tracing::info!(pid, "daemon stopped");
        Ok(())
    }

    /// Stop any running daemon and start a fresh one, returning the new PID.
    ///
    /// The new process re-reads the configuration file on startup, effectively
    /// reloading it.
    pub async fn restart(&self) -> Result<u32> {
        if self.is_running()?.is_some() {
            self.stop().await?;
        }
        self.start().await
    }
}
