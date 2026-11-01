use super::{CapacityExceeded, Options};
use crate::offline::files;
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

struct Memory {
    limit: usize,
    used: AtomicUsize,
    unconfirmed: AtomicBool,
}

pub(crate) struct Charge {
    memory: Arc<Memory>,
    bytes: usize,
}
impl Charge {
    pub(crate) fn grow(&mut self, bytes: usize) -> Result<()> {
        ensure!(
            !self.memory.unconfirmed.load(Ordering::Acquire),
            CapacityExceeded
        );
        let total = self.bytes.checked_add(bytes).ok_or(CapacityExceeded)?;
        let mut used = self.memory.used.load(Ordering::Acquire);
        loop {
            let next = used
                .checked_add(bytes)
                .filter(|next| *next <= self.memory.limit)
                .ok_or(CapacityExceeded)?;
            match self.memory.used.compare_exchange_weak(
                used,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => used = actual,
            }
        }
        self.bytes = total;
        ensure!(
            !self.memory.unconfirmed.load(Ordering::Acquire),
            CapacityExceeded
        );
        Ok(())
    }
}
impl Drop for Charge {
    fn drop(&mut self) {
        if !self.memory.unconfirmed.load(Ordering::Acquire) {
            self.memory.used.fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

pub(crate) struct Workspace {
    directory: tempfile::TempDir,
    memory: Arc<Memory>,
    cancel: CancellationToken,
    min_free_bytes: u64,
    next_spool: AtomicU64,
    _base: Charge,
    #[cfg(test)]
    fail_completed_probe: std::sync::atomic::AtomicBool,
}
impl Workspace {
    pub(crate) fn create(options: &Options, cancel: CancellationToken) -> Result<Self> {
        ensure!(
            options.working_bytes > 0
                && options.working_bytes <= isize::MAX as usize
                && options.min_free_bytes > 0,
            "invalid offline working budget"
        );
        ensure!(
            options.scratch_directory.is_absolute(),
            "absolute scratch directory required"
        );
        files::private_directory(&options.scratch_directory)?;
        ensure!(!cancel.is_cancelled(), "offline operation cancelled");
        let memory = Arc::new(Memory {
            limit: options.working_bytes,
            used: AtomicUsize::new(0),
            unconfirmed: AtomicBool::new(false),
        });
        let mut base = Charge {
            memory: memory.clone(),
            bytes: 0,
        };
        base.grow(
            options
                .scratch_directory
                .as_os_str()
                .len()
                .checked_mul(8)
                .and_then(|bytes| bytes.checked_add(4096))
                .ok_or(CapacityExceeded)?,
        )?;
        probe(&options.scratch_directory, options.min_free_bytes, 4096)?;
        let mut builder = tempfile::Builder::new();
        builder.prefix(".bitrouter-offline-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = builder.tempdir_in(&options.scratch_directory)?;
        files::private_directory(directory.path())?;
        Ok(Self {
            directory,
            memory,
            cancel,
            min_free_bytes: options.min_free_bytes,
            next_spool: AtomicU64::new(0),
            _base: base,
            #[cfg(test)]
            fail_completed_probe: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub(crate) fn reserve(&self, bytes: usize) -> Result<Charge> {
        let mut charge = Charge {
            memory: self.memory.clone(),
            bytes: 0,
        };
        charge.grow(bytes)?;
        Ok(charge)
    }

    pub(crate) fn available(&self) -> usize {
        if self.memory.unconfirmed.load(Ordering::Acquire) {
            return 0;
        }
        self.memory
            .limit
            .saturating_sub(self.memory.used.load(Ordering::Acquire))
    }

    pub(crate) fn check(&self) -> Result<()> {
        ensure!(
            !self.memory.unconfirmed.load(Ordering::Acquire),
            "offline SQL cleanup unconfirmed"
        );
        ensure!(!self.cancel.is_cancelled(), "offline operation cancelled");
        Ok(())
    }

    /// Borrowed SQL phases cannot release their caller's guards on a lost ACK.
    /// Disable this command allowance permanently, including later guard drops.
    pub(crate) fn quarantine(&self) {
        self.memory.unconfirmed.store(true, Ordering::Release);
    }

    pub(crate) fn probe(&self, output_parent: &Path, next_write: u64) -> Result<()> {
        self.check()?;
        #[cfg(test)]
        if next_write == 0 && self.fail_completed_probe.swap(false, Ordering::AcqRel) {
            anyhow::bail!("injected completed-output probe failure");
        }
        probe(output_parent, self.min_free_bytes, next_write)
    }

    #[cfg(test)]
    pub(super) fn refuse_completed_probe(&self) {
        self.fail_completed_probe.store(true, Ordering::Release);
    }

    pub(super) fn next_spool(&self) -> Result<u64> {
        let mut current = self.next_spool.load(Ordering::Acquire);
        loop {
            let next = current.checked_add(1).ok_or(CapacityExceeded)?;
            match self.next_spool.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(current),
                Err(actual) => current = actual,
            }
        }
    }

    pub(super) fn run_path(&self, spool: u64, pass: u64, run: u64) -> PathBuf {
        self.directory
            .path()
            .join(format!("{spool}-{pass}-{run}.run"))
    }

    pub(super) fn directory(&self) -> &Path {
        self.directory.path()
    }

    pub(crate) fn finish(self) -> Result<()> {
        // No cancellation check: cleanup must still finish after caller loss.
        self.directory
            .close()
            .context("offline scratch cleanup failed")
    }
}

pub(super) fn probe(path: &Path, floor: u64, write: u64) -> Result<()> {
    let needed = floor.checked_add(write).ok_or(CapacityExceeded)?;
    #[cfg(unix)]
    {
        // Probe the directory volume, never a database descriptor.
        // https://docs.rs/rustix/latest/rustix/fs/fn.statvfs.html
        let status = rustix::fs::statvfs(path)?;
        let available = status
            .f_bavail
            .checked_mul(status.f_frsize)
            .context("offline volume size overflow")?;
        ensure!(available >= needed, "insufficient offline working space");
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, needed);
        anyhow::bail!("offline volume probing is unsupported on this host")
    }
}
