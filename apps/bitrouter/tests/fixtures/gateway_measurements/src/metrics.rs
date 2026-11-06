//! Point observations of the owned process, never its descendants or the host.

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum Metric<T> {
    Available {
        source: &'static str,
        value: T,
    },
    Unavailable {
        source: &'static str,
        reason: String,
    },
}

impl<T> Metric<T> {
    fn from_result(source: &'static str, result: Result<T>) -> Self {
        match result {
            Ok(value) => Self::Available { source, value },
            Err(error) => Self::Unavailable {
                source,
                reason: format!("{error:#}"),
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct CpuTime {
    total_ms: f64,
    resolution_ms: f64,
}

#[derive(Debug, Serialize)]
pub(super) struct Observation {
    pid: u32,
    // Monotonic time immediately after the CPU read, relative to warmup end.
    at_ms: f64,
    observation_ms: f64,
    pub(super) rss_kib: u64,
    cpu_time: Metric<CpuTime>,
    threads: Metric<u64>,
    open_fds: Metric<u64>,
}

#[derive(Debug, Serialize)]
struct CpuUsage {
    elapsed_ms: f64,
    cpu_ms: f64,
    one_core_percent: f64,
    resolution_ms: f64,
}

#[derive(Debug, Serialize)]
pub(super) struct Window {
    pub(super) observations: Vec<Observation>,
    cpu: Metric<CpuUsage>,
}

impl Window {
    pub(super) fn new(observations: Vec<Observation>) -> Self {
        let cpu = (|| {
            ensure!(observations.len() >= 2, "CPU window needs two observations");
            let first = observations.first().context("missing first observation")?;
            let last = observations.last().context("missing last observation")?;
            let (Metric::Available { value: start, .. }, Metric::Available { value: end, .. }) =
                (&first.cpu_time, &last.cpu_time)
            else {
                anyhow::bail!("CPU endpoint unavailable; see observations");
            };
            let elapsed_ms = last.at_ms - first.at_ms;
            let cpu_ms = end.total_ms - start.total_ms;
            ensure!(
                elapsed_ms > 0.0 && cpu_ms >= 0.0,
                "non-monotonic CPU window"
            );
            Ok(CpuUsage {
                elapsed_ms,
                cpu_ms,
                one_core_percent: 100.0 * cpu_ms / elapsed_ms,
                resolution_ms: start.resolution_ms.max(end.resolution_ms),
            })
        })();
        Self {
            observations,
            cpu: Metric::from_result("process CPU delta / monotonic wall interval", cpu),
        }
    }
}

pub(super) struct Sampler {
    origin: Instant,
    #[cfg(target_os = "linux")]
    ticks_per_second: Result<u64, String>,
}

impl Sampler {
    pub(super) async fn new() -> Self {
        Self {
            origin: Instant::now(),
            #[cfg(target_os = "linux")]
            ticks_per_second: async {
                let value = command("/usr/bin/getconf", &["CLK_TCK"]).await?;
                let ticks = value.trim().parse::<u64>()?;
                ensure!(ticks > 0, "invalid CLK_TCK");
                Ok::<_, anyhow::Error>(ticks)
            }
            .await
            .map_err(|error| format!("{error:#}")),
        }
    }

    pub(super) async fn observe(&self, pid: u32) -> Result<Observation> {
        let started = Instant::now();
        let (cpu_time, threads, at_ms) = self.cpu_and_threads(pid).await;
        // Keep the v1 RSS requirement: an absent/dead process cannot yield a
        // successful sample. Optional metrics carry errors, never fake zeroes.
        let rss = command("/bin/ps", &["-o", "rss=", "-p", &pid.to_string()]).await?;
        let rss_kib = rss.trim().parse::<u64>()?;
        ensure!(rss_kib > 0, "empty RSS sample");
        let open_fds = open_fds(pid).await;
        Ok(Observation {
            pid,
            at_ms,
            observation_ms: started.elapsed().as_secs_f64() * 1000.0,
            rss_kib,
            cpu_time,
            threads,
            open_fds,
        })
    }

    async fn cpu_and_threads(&self, pid: u32) -> (Metric<CpuTime>, Metric<u64>, f64) {
        #[cfg(target_os = "linux")]
        {
            // utime/stime are process ticks; num_threads is field 20. Do not
            // include cutime/cstime (children) or mistake FDSize for open FDs.
            // https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .context("read process stat")
                .and_then(|text| linux_stat(&text, pid));
            let at_ms = self.origin.elapsed().as_secs_f64() * 1000.0;
            let cpu = (|| {
                let stat = stat.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?;
                let ticks = self
                    .ticks_per_second
                    .as_ref()
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let resolution_ms = 1000.0 / *ticks as f64;
                Ok(CpuTime {
                    total_ms: stat.cpu_ticks as f64 * resolution_ms,
                    resolution_ms,
                })
            })();
            (
                Metric::from_result("/proc/PID/stat + getconf CLK_TCK", cpu),
                Metric::from_result("/proc/PID/stat num_threads", stat.map(|v| v.threads)),
                at_ms,
            )
        }
        #[cfg(target_os = "macos")]
        {
            // ps %cpu is a decaying average, not an interval measurement.
            // Read cumulative user+system time and difference two observations.
            // -M selects a fixed thread format even when -o is supplied.
            // https://github.com/apple-oss-distributions/adv_cmds/blob/main/ps/ps.1
            let cpu = command("/bin/ps", &["-o", "time=", "-p", &pid.to_string()])
                .await
                .and_then(|text| ps_cpu_time(text.trim()));
            let at_ms = self.origin.elapsed().as_secs_f64() * 1000.0;
            let threads = command("/bin/ps", &["-M", "-p", &pid.to_string()])
                .await
                .and_then(|text| mac_threads(&text, pid));
            (
                Metric::from_result("ps time (user + system)", cpu),
                Metric::from_result("ps -M thread rows", threads),
                at_ms,
            )
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        (
            Metric::from_result(
                "unsupported Unix",
                Err(anyhow::anyhow!("CPU backend unavailable for PID {pid}")),
            ),
            Metric::from_result(
                "unsupported Unix",
                Err(anyhow::anyhow!("thread backend unavailable")),
            ),
            self.origin.elapsed().as_secs_f64() * 1000.0,
        )
    }
}

async fn command(program: &str, arguments: &[&str]) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new(program)
            .args(arguments)
            .env_clear()
            .env("LC_ALL", "C")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("metric command timeout")?
    .with_context(|| format!("run {program}"))?;
    ensure!(
        output.status.success(),
        "{program} failed: {}",
        output.status
    );
    ensure!(
        output.stderr.is_empty(),
        "{program} emitted diagnostics; observation may be incomplete"
    );
    String::from_utf8(output.stdout).context("metric output is not UTF-8")
}

async fn open_fds(pid: u32) -> Metric<u64> {
    #[cfg(target_os = "linux")]
    {
        // Count descriptors, not mapped files or allocated descriptor slots.
        // https://man7.org/linux/man-pages/man5/proc_pid_fd.5.html
        let result = (|| {
            let mut count = 0;
            for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
                entry?
                    .file_name()
                    .to_str()
                    .context("non-UTF8 fd")?
                    .parse::<u64>()?;
                count += 1;
            }
            ensure!(count > 0, "empty descriptor inventory");
            Ok(count)
        })();
        Metric::from_result("/proc/PID/fd numeric entries", result)
    }
    #[cfg(target_os = "macos")]
    {
        // Field output distinguishes numeric FDs from cwd/txt/memory mappings.
        // https://github.com/lsof-org/lsof/blob/master/Lsof.8
        let result = command(
            "/usr/sbin/lsof",
            &["-nP", "-a", "-p", &pid.to_string(), "-Fpf"],
        )
        .await
        .and_then(|text| lsof_fds(&text, pid));
        Metric::from_result("lsof -Fpf numeric descriptors", result)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    Metric::from_result(
        "unsupported Unix",
        Err(anyhow::anyhow!("FD backend unavailable for PID {pid}")),
    )
}

#[cfg(any(target_os = "linux", test))]
struct LinuxStat {
    cpu_ticks: u64,
    threads: u64,
}

#[cfg(any(target_os = "linux", test))]
fn linux_stat(text: &str, pid: u32) -> Result<LinuxStat> {
    let (identity, fields) = text.rsplit_once(')').context("stat comm missing")?;
    let (actual, _) = identity.split_once(" (").context("stat PID missing")?;
    ensure!(actual.parse::<u32>()? == pid, "stat PID mismatch");
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    let field =
        |index: usize| -> Result<u64> { Ok(fields.get(index).context("truncated stat")?.parse()?) };
    let cpu_ticks = field(11)?
        .checked_add(field(12)?)
        .context("CPU tick overflow")?;
    let threads = field(17)?;
    ensure!(threads > 0, "empty thread inventory");
    Ok(LinuxStat { cpu_ticks, threads })
}

#[cfg(any(target_os = "macos", test))]
fn ps_cpu_time(text: &str) -> Result<CpuTime> {
    let (minutes, seconds) = text.split_once(':').context("missing CPU time separator")?;
    ensure!(
        !minutes.is_empty() && minutes.bytes().all(|b| b.is_ascii_digit()),
        "invalid CPU minutes"
    );
    let (whole, fraction) = seconds
        .split_once('.')
        .context("missing CPU time precision")?;
    ensure!(
        whole.len() == 2
            && whole.bytes().all(|b| b.is_ascii_digit())
            && !fraction.is_empty()
            && fraction.len() <= 6
            && fraction.bytes().all(|b| b.is_ascii_digit()),
        "invalid CPU seconds"
    );
    let minutes = minutes.parse::<u64>()?;
    let seconds = seconds.parse::<f64>()?;
    ensure!((0.0..60.0).contains(&seconds), "CPU seconds out of range");
    Ok(CpuTime {
        total_ms: minutes as f64 * 60_000.0 + seconds * 1000.0,
        resolution_ms: 1000.0 / 10_u64.pow(fraction.len() as u32) as f64,
    })
}

#[cfg(any(target_os = "macos", test))]
fn mac_threads(text: &str, pid: u32) -> Result<u64> {
    let mut lines = text.lines();
    let header = lines.next().context("missing ps thread header")?;
    // macOS leaves USER and TT blank on continuation thread rows. Preserve
    // the fixed-width PID field instead of shifting columns with whitespace.
    let pid_end = header
        .match_indices("PID")
        .find_map(|(start, word)| {
            let end = start + word.len();
            let before = header[..start].chars().next_back();
            let after = header[end..].chars().next();
            (before.is_none_or(char::is_whitespace) && after.is_none_or(char::is_whitespace))
                .then_some(end)
        })
        .context("missing PID column")?;
    let mut count = 0;
    for line in lines {
        let prefix = line.get(..pid_end).context("missing thread PID")?;
        ensure!(
            prefix.as_bytes().last().is_some_and(u8::is_ascii_digit)
                && line
                    .as_bytes()
                    .get(pid_end)
                    .is_none_or(u8::is_ascii_whitespace),
            "incomplete thread PID field"
        );
        let actual = prefix
            .split_whitespace()
            .next_back()
            .context("missing thread PID")?;
        ensure!(actual.parse::<u32>()? == pid, "thread PID mismatch");
        count += 1;
    }
    ensure!(count > 0, "empty thread inventory");
    Ok(count)
}

#[cfg(any(target_os = "macos", test))]
fn lsof_fds(text: &str, pid: u32) -> Result<u64> {
    let mut lines = text.lines();
    ensure!(
        lines.next() == Some(format!("p{pid}").as_str()),
        "lsof PID missing or changed"
    );
    let mut descriptors = std::collections::BTreeSet::new();
    for line in lines {
        let value = line.strip_prefix('f').context("unexpected lsof field")?;
        if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
            ensure!(
                descriptors.insert(value.parse::<u64>()?),
                "duplicate descriptor"
            );
        } else {
            ensure!(
                matches!(value, "cwd" | "rtd" | "txt" | "mem" | "DEL"),
                "unavailable or unknown lsof descriptor"
            );
        }
    }
    ensure!(!descriptors.is_empty(), "empty descriptor inventory");
    Ok(descriptors.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_windows_use_wall_delta_and_never_fill_missing_endpoints_with_zero() -> Result<()> {
        let observed = |at_ms, total_ms| Observation {
            pid: 42,
            at_ms,
            observation_ms: 1.0,
            rss_kib: 1,
            cpu_time: Metric::from_result(
                "test",
                Ok(CpuTime {
                    total_ms,
                    resolution_ms: 10.0,
                }),
            ),
            threads: Metric::from_result("test", Ok(1)),
            open_fds: Metric::from_result("test", Ok(3)),
        };
        let window = Window::new(vec![observed(100.0, 50.0), observed(1100.0, 170.0)]);
        let Metric::Available { value, .. } = window.cpu else {
            anyhow::bail!("missing CPU delta");
        };
        assert_eq!(
            (value.elapsed_ms, value.cpu_ms, value.one_core_percent),
            (1000.0, 120.0, 12.0)
        );
        let mut missing = observed(1100.0, 0.0);
        missing.cpu_time = Metric::from_result("test", Err(anyhow::anyhow!("missing")));
        assert!(matches!(
            Window::new(vec![observed(100.0, 50.0), missing]).cpu,
            Metric::Unavailable { .. }
        ));
        assert!(matches!(
            Window::new(vec![observed(100.0, 50.0), observed(1100.0, 40.0)]).cpu,
            Metric::Unavailable { .. }
        ));
        Ok(())
    }

    #[test]
    fn unix_parsers_preserve_units_and_reject_incomplete_inventory() -> Result<()> {
        let stat = linux_stat(
            "42 (name ) with spaces) S 1 2 3 4 5 6 7 8 9 10 123 45 99 99 20 0 7",
            42,
        )?;
        assert_eq!((stat.cpu_ticks, stat.threads), (168, 7));
        assert!(linux_stat("42 (bad) S 1", 42).is_err());
        assert!(linux_stat("43 (bad) S 1", 42).is_err());
        let cpu = ps_cpu_time("1234:56.78")?;
        assert!((cpu.total_ms - 74_096_780.0).abs() < 0.001);
        assert_eq!(cpu.resolution_ms, 10.0);
        for bad in ["", "0", "0:00", "0:60.00", "0:NaN", "-1:01.00", "0:01.x"] {
            assert!(ps_cpu_time(bad).is_err(), "{bad}");
        }
        assert_eq!(
            mac_threads("USER PID TT %CPU\nuser  42 ?? 0.0\nuser  42 ?? 0.0\n", 42)?,
            2
        );
        assert!(mac_threads("USER PID\n", 42).is_err());
        assert!(mac_threads("USER PID\nuser 43\n", 42).is_err());
        assert_eq!(
            lsof_fds("p42\nfcwd\nftxt\nftxt\nf0\nf12\nf1000001\n", 42)?,
            3
        );
        for bad in [
            "",
            "p43\nf0\n",
            "p42\nfcwd\n",
            "p42\nfNOFD\n",
            "p42\nf0\nf0\n",
        ] {
            assert!(lsof_fds(bad, 42).is_err(), "{bad}");
        }
        Ok(())
    }

    #[test]
    fn mac_thread_rows_preserve_blank_continuation_columns() -> Result<()> {
        let header = "USER     PID   TT   %CPU STAT PRI     STIME     UTIME COMMAND\n";
        let first = "archer 59644   ??    7.5 R    31T   0:00.01   0:00.02 node\n";
        let second = "       59644         0.0 S    31T   0:00.00   0:00.00 \n";
        let observed = format!("{header}{first}{second}");
        assert_eq!(mac_threads(&observed, 59644)?, 2);
        for invalid in [
            "       59645         0.0 S    31T   0:00.00   0:00.00 \n",
            "       596440        0.0 S    31T   0:00.00   0:00.00 \n",
            "                    0.0 S    31T   0:00.00   0:00.00 \n",
            "59644               0.0 S    31T   0:00.00   0:00.00 \n",
            "       59\n",
        ] {
            assert!(mac_threads(&format!("{header}{first}{invalid}"), 59644).is_err());
        }
        assert!(mac_threads(" PPID\n   42\n", 42).is_err());
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mac_thread_inventory_observes_the_owned_runtime() -> Result<()> {
        let pid = std::process::id();
        let output = command("/bin/ps", &["-M", "-p", &pid.to_string()]).await?;
        ensure!(mac_threads(&output, pid)? >= 3, "runtime threads missing");
        Ok(())
    }

    #[tokio::test]
    async fn local_process_observation_or_explicit_unavailability() -> Result<()> {
        let observation = Sampler::new().await.observe(std::process::id()).await?;
        assert!(observation.rss_kib > 0);
        for metric in [&observation.threads, &observation.open_fds] {
            match metric {
                Metric::Available { value, .. } => assert!(*value > 0),
                Metric::Unavailable { reason, .. } => assert!(!reason.is_empty()),
            }
        }
        let absent: Metric<u64> =
            Metric::from_result("test", Err(anyhow::anyhow!("permission denied")));
        let json = serde_json::to_value(absent)?;
        assert_eq!(json["status"], "unavailable");
        assert!(json.get("value").is_none());
        assert!(matches!(
            Window::new(Vec::new()).cpu,
            Metric::Unavailable { .. }
        ));
        Ok(())
    }
}
