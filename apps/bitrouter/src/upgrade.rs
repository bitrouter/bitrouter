//! Safe automatic handoff from an older CLI-owned local daemon.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::daemon::{self, DaemonCommand, DaemonResponse, DaemonStartOutcome};
use crate::paths::ConfigSource;

struct Hello {
    version: String,
    instance_id: String,
    build_id: String,
    cli_owned: bool,
}

struct CandidateEvidence {
    path: std::path::PathBuf,
    len: u64,
    modified: std::time::SystemTime,
}

struct OldProcess<'a> {
    pid: u32,
    listen: &'a str,
}

impl CandidateEvidence {
    fn capture() -> Result<Self> {
        let path = std::env::current_exe().context("locating candidate executable")?;
        let metadata = std::fs::metadata(&path).context("reading candidate executable metadata")?;
        Ok(Self {
            path,
            len: metadata.len(),
            modified: metadata.modified()?,
        })
    }

    fn unchanged(&self) -> Result<bool> {
        let current = Self::capture()?;
        Ok(current.path == self.path
            && current.len == self.len
            && current.modified == self.modified)
    }
}

async fn hello(socket: &Path) -> Result<Hello> {
    match daemon::send_command(socket, &DaemonCommand::HandoffHello).await {
        Ok(DaemonResponse::HandoffHello {
            version,
            protocol: 1,
            instance_id,
            build_id,
            cli_owned,
        }) if !instance_id.is_empty() && !build_id.is_empty() => Ok(Hello {
            version,
            instance_id,
            build_id,
            cli_owned,
        }),
        Ok(DaemonResponse::Error { message })
            if message.contains("unknown variant") && message.contains("handoff_hello") =>
        {
            bail!(
                "the running daemon cannot prove it is idle (legacy handoff protocol); use `bro restart` when its agent runs and requests are finished"
            )
        }
        Ok(DaemonResponse::Error { message }) => {
            bail!("daemon handoff handshake failed: {message}")
        }
        Ok(_) => bail!(
            "the running daemon has an incompatible handoff protocol; use `bro restart` after its work finishes"
        ),
        Err(error) => Err(error).context("probing daemon handoff protocol"),
    }
}

pub async fn lock(source: &ConfigSource) -> Result<File> {
    let digest = crate::daemon_locator::config_identity_digest(source)?;
    let path = source
        .home()
        .join(format!(".bitrouter-upgrade-{digest}.lock"));
    tokio::task::spawn_blocking(move || -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.lock()?;
        Ok(file)
    })
    .await?
}

/// Verify a compatible daemon or hand off an older one. A daemon without the
/// new protocol is never stopped automatically, even if it appears idle.
pub async fn ensure_compatible(source: &ConfigSource, socket: &Path, no_start: bool) -> Result<()> {
    let first = hello(socket).await?;
    if first.version == crate::VERSION && first.build_id == crate::HANDOFF_BUILD_ID {
        return Ok(());
    }
    let candidate = semver::Version::parse(crate::VERSION).context("parsing candidate version")?;
    let resident = semver::Version::parse(&first.version).context("parsing daemon version")?;
    ensure!(
        candidate >= resident,
        "daemon {} is newer than installed bro {}; automatic downgrade is unavailable",
        first.version,
        crate::VERSION
    );
    ensure!(
        !no_start,
        "daemon {} requires a safe handoff to bro {}, but --no-start forbids restart; use `bro restart` after its work finishes",
        first.version,
        crate::VERSION
    );
    ensure!(
        first.cli_owned,
        "daemon {} is managed by an external process; use its service manager to restart it",
        first.version
    );

    let _lock = lock(source).await?;
    let candidate_executable = CandidateEvidence::capture()?;
    // Another CLI may have completed the handoff while this one waited.
    let current = hello(socket).await?;
    if current.version == crate::VERSION && current.build_id == crate::HANDOFF_BUILD_ID {
        return Ok(());
    }
    ensure!(
        current.instance_id == first.instance_id
            && current.build_id == first.build_id
            && current.cli_owned,
        "daemon changed while waiting for the handoff lock; retry"
    );
    let located = crate::daemon_locator::locate_source(source).await?
        .context("daemon ownership could not be verified; use its service manager or explicit `bro restart`")?;
    ensure!(
        located.socket() == socket,
        "selected daemon endpoint changed during handoff"
    );
    let status = daemon::send_command(socket, &DaemonCommand::Status).await?;
    let (old_pid, old_listen) = match status {
        DaemonResponse::Status {
            pid,
            listen,
            config_state: Some(state),
            ..
        } if pid == located.pid()
            && matches!(state.running, crate::reload::RunningConfigState::InSync) =>
        {
            (pid, listen)
        }
        _ => bail!("daemon identity or running configuration could not be verified for handoff"),
    };

    let preflight = crate::upgrade_preflight::Preflight::check(source).await?;
    let token = match daemon::send_command(socket, &DaemonCommand::HandoffPrepare).await? {
        DaemonResponse::HandoffReady { token } => token,
        DaemonResponse::HandoffBusy { reason } => bail!(
            "daemon {} (pid {old_pid}) is busy: {reason}; retry after its work finishes",
            current.version
        ),
        other => bail!("daemon declined handoff: {other:?}"),
    };
    let result = handoff_under_gate(
        source,
        socket,
        &current,
        OldProcess {
            pid: old_pid,
            listen: &old_listen,
        },
        &token,
        &preflight,
        &candidate_executable,
    )
    .await;
    if result.is_err() {
        let _ = daemon::send_command(socket, &DaemonCommand::HandoffAbort { token }).await;
    }
    result
}

async fn handoff_under_gate(
    source: &ConfigSource,
    socket: &Path,
    old: &Hello,
    process: OldProcess<'_>,
    token: &str,
    preflight: &crate::upgrade_preflight::Preflight,
    candidate_executable: &CandidateEvidence,
) -> Result<()> {
    let checked = hello(socket).await?;
    ensure!(
        checked.instance_id == old.instance_id
            && checked.version == old.version
            && checked.build_id == old.build_id,
        "daemon changed during migration preflight"
    );
    let located = crate::daemon_locator::locate_source(source)
        .await?
        .context("daemon locator disappeared during handoff")?;
    ensure!(
        located.pid() == process.pid && located.socket() == socket,
        "daemon identity changed during handoff"
    );
    let backup = preflight.backup(source).await?;
    ensure!(
        candidate_executable.unchanged()?,
        "candidate executable changed during handoff preflight"
    );
    match daemon::send_command(
        socket,
        &DaemonCommand::HandoffStop {
            token: token.to_string(),
        },
    )
    .await?
    {
        DaemonResponse::Ok => {}
        other => bail!("daemon refused prepared stop: {other:?}"),
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if !daemon::endpoint_in_use(socket) && !process_is_alive(process.pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ensure!(
        !daemon::endpoint_in_use(socket) && !process_is_alive(process.pid),
        "old daemon pid {} did not exit and release its endpoint; recovery backup: {}",
        process.pid,
        backup.display()
    );
    let log = source.home().join("bitrouter.log");
    let ready = daemon::start_and_wait(source, &log, Some(socket), Duration::from_secs(20)).await;
    let info = match ready {
        Ok(DaemonStartOutcome::Ready(info)) => info,
        Ok(outcome) => bail!(
            "new daemon did not become ready ({outcome:?}); log: {}; recovery backup: {}",
            log.display(),
            backup.display()
        ),
        Err(error) => bail!(
            "new daemon launch failed: {error:#}; log: {}; recovery backup: {}",
            log.display(),
            backup.display()
        ),
    };
    let replacement = hello(socket).await.with_context(|| {
        format!(
            "replacement verification failed; log: {}; recovery backup: {}",
            log.display(),
            backup.display()
        )
    })?;
    ensure!(
        info.pid != process.pid
            && replacement.instance_id != old.instance_id
            && replacement.version == crate::VERSION
            && replacement.build_id == crate::HANDOFF_BUILD_ID
            && replacement.cli_owned,
        "replacement daemon identity/version is wrong; log: {}; recovery backup: {}",
        log.display(),
        backup.display()
    );
    ensure!(
        info.listen == process.listen,
        "replacement daemon listen address changed; log: {}; recovery backup: {}",
        log.display(),
        backup.display()
    );
    let mut health_address: std::net::SocketAddr = info.listen.parse().with_context(|| {
        format!(
            "invalid replacement listen address; log: {}; recovery backup: {}",
            log.display(),
            backup.display()
        )
    })?;
    if health_address.ip().is_unspecified() {
        health_address.set_ip(match health_address.ip() {
            std::net::IpAddr::V4(_) => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            std::net::IpAddr::V6(_) => std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        });
    }
    let health = format!("http://{health_address}/health");
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .with_context(|| {
            format!(
                "HTTP verifier failed; log: {}; recovery backup: {}",
                log.display(),
                backup.display()
            )
        })?
        .get(&health)
        .timeout(Duration::from_secs(3))
        .send()
        .await;
    ensure!(
        response.is_ok_and(|response| response.status().is_success()),
        "replacement daemon HTTP health check failed; log: {}; recovery backup: {}",
        log.display(),
        backup.display()
    );
    let new_location = crate::daemon_locator::locate_source(source)
        .await
        .with_context(|| format!("replacement locator verification failed; log: {}; recovery backup: {}", log.display(), backup.display()))?
        .with_context(|| format!("replacement daemon did not publish a verified locator; log: {}; recovery backup: {}", log.display(), backup.display()))?;
    ensure!(
        new_location.pid() == info.pid && new_location.socket() == socket,
        "replacement daemon locator identity is wrong; log: {}; recovery backup: {}",
        log.display(),
        backup.display()
    );
    tracing::info!(old_pid = process.pid, new_pid = info.pid, backup = %backup.display(), "automatic local daemon handoff completed");
    Ok(())
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")))
}
