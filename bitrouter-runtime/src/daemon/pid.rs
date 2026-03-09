use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Result, RuntimeError};

/// Manages a PID file at `<runtime_dir>/bitrouter.pid`.
pub(crate) struct PidFile {
    path: PathBuf,
}

impl PidFile {
    pub(crate) fn new(runtime_dir: &Path) -> Self {
        Self {
            path: runtime_dir.join("bitrouter.pid"),
        }
    }

    /// Read the PID from the file, returning `None` if the file does not exist.
    pub(crate) fn read(&self) -> Result<Option<u32>> {
        match fs::read_to_string(&self.path) {
            Ok(content) => {
                let pid = content.trim().parse::<u32>().map_err(|_| {
                    RuntimeError::Daemon(format!("corrupt PID file: {}", self.path.display()))
                })?;
                Ok(Some(pid))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Write a PID to the file, creating parent directories as needed.
    pub(crate) fn write(&self, pid: u32) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, pid.to_string())?;
        Ok(())
    }

    /// Remove the PID file. No-op if the file does not exist.
    pub(crate) fn remove(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
