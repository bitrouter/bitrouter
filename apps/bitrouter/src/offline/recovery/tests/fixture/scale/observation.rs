//! Scalar observations for the explicit offline qualifications, not quota measurements.

use super::*;
use std::{
    io::Seek,
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

fn available(volume: &File) -> Result<u64> {
    // Same volume accounting as the existing offline floor probe, pinned across cleanup.
    // https://docs.rs/rustix/latest/rustix/fs/fn.fstatvfs.html
    let status = rustix::fs::fstatvfs(volume)?;
    status
        .f_bavail
        .checked_mul(status.f_frsize)
        .context("scratch volume size overflow")
}

fn resident(file: &mut File) -> Result<(u64, u64)> {
    file.rewind()?;
    let mut bytes = [0_u8; BUFFER];
    let mut used = 0;
    loop {
        ensure!(
            used < bytes.len(),
            "process status exceeds observation buffer"
        );
        let count = file.read(&mut bytes[used..])?;
        if count == 0 {
            break;
        }
        used += count;
    }
    let status = std::str::from_utf8(&bytes[..used])?;
    let field = |name: &str| -> Result<u64> {
        let line = status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .context("resident observation field unavailable")?;
        let mut parts = line.split_whitespace();
        let value: u64 = parts.next().context("resident value")?.parse()?;
        ensure!(
            parts.next() == Some("kB") && parts.next().is_none(),
            "resident unit"
        );
        value.checked_mul(1024).context("resident bytes overflow")
    };
    Ok((field("VmRSS:")?, field("VmHWM:")?))
}

/// No monitor task or process: callers poll `tick` alongside their retained work/ACK future.
pub(crate) struct Observation {
    state: Mutex<Metrics>,
}

impl Observation {
    pub(crate) fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            state: Mutex::new(Metrics::new(root)?),
        })
    }

    fn state(&self) -> MutexGuard<'_, Metrics> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.poisoned = true;
                state
            }
        }
    }

    /// Without this opt-in, RSS belongs to the external exact-invocation wrapper.
    pub(crate) fn attach(&self, pid: Option<u32>) {
        self.state().attach(pid);
    }

    /// Also call immediately before deleting a phase's temporary data.
    pub(crate) fn sample(&self) {
        self.state().sample();
    }

    pub(crate) async fn tick(&self) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        self.sample();
    }

    pub(crate) fn finish(self, label: &str, cleanup_confirmed: bool, removed: bool) -> Result<()> {
        let state = match self.state.into_inner() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.poisoned = true;
                state
            }
        };
        state.finish(label, cleanup_confirmed, removed)
    }
}

struct Metrics {
    root: PathBuf,
    volume: File,
    pid: Option<u32>,
    status: Option<File>,
    rss_requested: bool,
    unavailable: Option<&'static str>,
    before: u64,
    minimum_available: u64,
    scratch_samples: Option<u64>,
    scratch_missed: bool,
    rss_peak: Option<u64>,
    hwm_peak: Option<u64>,
    rss_samples: Option<u64>,
    rss_missed: bool,
    started: Instant,
    last_sample: Instant,
    maximum_gap: Duration,
    poisoned: bool,
}

impl Metrics {
    fn new(root: &Path) -> Result<Self> {
        let volume = File::open(root)?;
        let before = available(&volume)?;
        let started = Instant::now();
        Ok(Self {
            root: root.into(),
            volume,
            pid: None,
            status: None,
            rss_requested: false,
            unavailable: None,
            before,
            minimum_available: before,
            scratch_samples: Some(1),
            scratch_missed: false,
            rss_peak: None,
            hwm_peak: None,
            rss_samples: None,
            rss_missed: false,
            started,
            last_sample: started,
            maximum_gap: Duration::ZERO,
            poisoned: false,
        })
    }

    fn attach(&mut self, pid: Option<u32>) {
        self.pid = pid;
        self.rss_requested = true;
        self.rss_samples = Some(0);
        // An open proc descriptor never refers to a later process reusing this PID.
        // https://docs.kernel.org/filesystems/proc.html#process-specific-subdirectories
        self.status = match pid {
            Some(pid) if cfg!(target_os = "linux") => {
                match File::open(format!("/proc/{pid}/status")) {
                    Ok(file) => Some(file),
                    Err(_) => {
                        self.unavailable = Some("proc_status_open_failed");
                        None
                    }
                }
            }
            Some(_) => {
                self.unavailable = Some("unsupported_platform");
                None
            }
            None => {
                self.unavailable = Some("pid_unavailable");
                None
            }
        };
        self.memory();
    }

    fn memory(&mut self) {
        if !self.rss_requested {
            return;
        }
        let Some(file) = self.status.as_mut() else {
            self.rss_missed = true;
            return;
        };
        match resident(file) {
            Ok((rss, hwm)) => {
                self.rss_peak = Some(self.rss_peak.map_or(rss, |old| old.max(rss)));
                self.hwm_peak = Some(self.hwm_peak.map_or(hwm, |old| old.max(hwm)));
                self.rss_samples = self.rss_samples.and_then(|count| count.checked_add(1));
            }
            Err(_) => {
                self.rss_missed = true;
            }
        }
    }

    fn sample(&mut self) {
        self.gap();
        self.memory();
        match available(&self.volume) {
            Ok(bytes) => {
                self.minimum_available = self.minimum_available.min(bytes);
                self.scratch_samples = self.scratch_samples.and_then(|count| count.checked_add(1));
            }
            // Failed observations never cancel or replace the retained work/ACK future.
            Err(_) => {
                self.scratch_missed = true;
            }
        }
    }

    fn gap(&mut self) {
        let now = Instant::now();
        self.maximum_gap = self.maximum_gap.max(now.duration_since(self.last_sample));
        self.last_sample = now;
    }

    fn finish(mut self, label: &str, cleanup_confirmed: bool, removed: bool) -> Result<()> {
        self.gap();
        let removal_confirmed = !removed
            || matches!(std::fs::symlink_metadata(&self.root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound);
        let after = match available(&self.volume) {
            Ok(bytes) => Some(bytes),
            Err(_) => {
                self.scratch_missed = true;
                None
            }
        };
        if let Some(bytes) = after {
            self.minimum_available = self.minimum_available.min(bytes);
        }
        if self.pid == Some(std::process::id()) {
            self.memory();
        }
        drop(self.status.take());
        // VmRSS/VmHWM are kernel estimates, and polling can miss the final high water.
        // https://man7.org/linux/man-pages/man5/proc_pid_status.5.html
        let complete = !self.poisoned
            && removal_confirmed
            && after.is_some()
            && self.scratch_samples.is_some()
            && (!self.rss_requested
                || (self.rss_peak.is_some()
                    && self.hwm_peak.is_some()
                    && self.rss_samples.is_some()));
        eprintln!(
            "offline_scale_resources {}",
            serde_json::json!({
                "label":label,"pid":self.pid,"sample_interval_ms":250,"sampling":"cooperative_observed_maximum",
                "rss_source":if self.rss_requested { "linux_proc_status_estimate" } else { "external_exact_invocation_wrapper" },
                "rss_observed_max_bytes":self.rss_peak,
                "vm_hwm_observed_max_bytes":self.hwm_peak,"rss_samples":self.rss_samples,
                "rss_sample_missed":self.rss_missed,"rss_unavailable":self.unavailable,
                "scratch_scope":"pinned_scratch_filesystem_available_bytes",
                "scratch_available_before_bytes":self.before,"scratch_minimum_available_bytes":self.minimum_available,
                "scratch_available_after_bytes":after,
                "scratch_observed_maximum_available_drop_bytes":self.before.checked_sub(self.minimum_available),
                "other_filesystem_activity_may_contribute":true,
                "scratch_samples":self.scratch_samples,"scratch_sample_missed":self.scratch_missed,
                "cleanup_confirmed":cleanup_confirmed,"directory_removed":removed,"observations_available":complete,
                "observer_poisoned":self.poisoned,
                "elapsed_ms":self.started.elapsed().as_millis(),"maximum_sample_gap_ms":self.maximum_gap.as_millis(),
                "exact_peak":false,"product_working_budget_measurement":false
            })
        );
        ensure!(
            complete,
            "required resource observations unavailable for {label}"
        );
        Ok(())
    }
}
