//! Safe automatic handoff from an older CLI-owned local daemon.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use bitrouter_ai::artifact::ArtifactIdentity;

use crate::daemon::{self, DaemonCommand, DaemonResponse, DaemonStartOutcome};
use crate::paths::ConfigSource;

/// Version two requires compiled-distribution evidence. Older clients reject
/// this version before preparing a handoff, so they cannot unknowingly replace
/// a reduced daemon with their full executable.
pub const HANDOFF_PROTOCOL: u32 = 2;

/// Whether two executables have the same deployment selection. Version and
/// source identity are checked separately when deciding whether to hand off.
pub fn same_capabilities(candidate: &ArtifactIdentity, resident: &ArtifactIdentity) -> bool {
    candidate.profile == resident.profile
        && candidate.features == resident.features
        && candidate.target == resident.target
}

struct Hello {
    version: String,
    instance_id: String,
    build_id: String,
    cli_owned: bool,
    artifact: ArtifactIdentity,
}

struct CandidateEvidence {
    path: std::path::PathBuf,
    len: u64,
    modified: std::time::SystemTime,
}

struct OldProcess<'a> {
    pid: u32,
    listen: &'a str,
    log: &'a Path,
    preserve_listen: bool,
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
    let reply = daemon::send_command(socket, &DaemonCommand::HandoffHello)
        .await
        .context("probing daemon handoff protocol")?;
    decode_hello(reply)
}

fn decode_hello(reply: DaemonResponse) -> Result<Hello> {
    match reply {
        DaemonResponse::HandoffHello {
            version,
            protocol: HANDOFF_PROTOCOL,
            instance_id,
            build_id,
            cli_owned,
            artifact: Some(artifact),
        } if !instance_id.is_empty()
            && !build_id.is_empty()
            && !artifact.target.is_empty()
            && artifact.features.windows(2).all(|pair| pair[0] < pair[1])
            && artifact.version == version
            && artifact.handoff_build_id == build_id =>
        {
            Ok(Hello {
                version,
                instance_id,
                build_id,
                cli_owned,
                artifact,
            })
        }
        DaemonResponse::Error { message }
            if message.contains("unknown variant") && message.contains("handoff_hello") =>
        {
            bail!(
                "the running daemon cannot prove it is idle (legacy handoff protocol); use `bro restart` when its agent runs and requests are finished"
            )
        }
        DaemonResponse::Error { message } => {
            bail!("daemon handoff handshake failed: {message}")
        }
        _ => bail!(
            "the running daemon has an incompatible handoff protocol; use `bro restart` after its work finishes"
        ),
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
    ensure_compatible_inner(source, socket, no_start, None, None).await
}

/// An explicit restart may change a known distribution only under the same
/// idle gate, ownership and preflight rules as an automatic handoff. A legacy
/// daemon retains the existing explicit restart path; no automatic inference
/// of its compiled capabilities is made. `Some` retains the restart lock for
/// that path; `None` means a profile handoff already completed under the lock.
pub async fn prepare_restart(
    source: &ConfigSource,
    socket: &Path,
    log: &Path,
) -> Result<Option<File>> {
    let lock = lock(source).await?;
    if !daemon::endpoint_in_use(socket) {
        return Ok(Some(lock));
    }
    let reply = daemon::send_command(socket, &DaemonCommand::HandoffHello).await?;
    match reply {
        reply @ DaemonResponse::HandoffHello {
            protocol: HANDOFF_PROTOCOL,
            ..
        } => {
            let resident = decode_hello(reply)?;
            if same_capabilities(&crate::artifact::current(), &resident.artifact) {
                return Ok(Some(lock));
            }
            ensure_compatible_inner(source, socket, false, Some(log), Some(lock)).await?;
            Ok(None)
        }
        DaemonResponse::HandoffHello { .. } => Ok(Some(lock)),
        DaemonResponse::Error { message }
            if message.contains("unknown variant") && message.contains("handoff_hello") =>
        {
            Ok(Some(lock))
        }
        other => bail!("daemon profile could not be inspected before restart: {other:?}"),
    }
}

async fn ensure_compatible_inner(
    source: &ConfigSource,
    socket: &Path,
    no_start: bool,
    explicit_profile_log: Option<&Path>,
    held_lock: Option<File>,
) -> Result<()> {
    let first = hello(socket).await?;
    ensure!(
        explicit_profile_log.is_some()
            || same_capabilities(&crate::artifact::current(), &first.artifact),
        "daemon distribution differs from this executable (daemon {} [{}], installed {} [{}]); common inspection remains available; use the matching executable for execution or explicitly run `bro restart` after active work finishes",
        first.artifact.profile.as_str(),
        first.artifact.features.join(","),
        crate::artifact::current().profile.as_str(),
        crate::artifact::current().features.join(",")
    );
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

    let _lock = match held_lock {
        Some(lock) => lock,
        None => lock(source).await?,
    };
    let candidate_executable = CandidateEvidence::capture()?;
    // Another CLI may have completed the handoff while this one waited.
    let current = hello(socket).await?;
    if current.version == crate::VERSION && current.build_id == crate::HANDOFF_BUILD_ID {
        return Ok(());
    }
    ensure!(
        current.instance_id == first.instance_id
            && current.build_id == first.build_id
            && current.artifact == first.artifact
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
            && (explicit_profile_log.is_some()
                || matches!(
                    state.running,
                    bitrouter_ai::control::reload::RunningConfigState::InSync
                )) =>
        {
            (pid, listen)
        }
        _ => bail!("daemon identity or running configuration could not be verified for handoff"),
    };

    let preflight = if explicit_profile_log.is_some() {
        crate::upgrade_preflight::Preflight::check_explicit(source).await?
    } else {
        crate::upgrade_preflight::Preflight::check(source).await?
    };
    let token = match daemon::send_command(socket, &DaemonCommand::HandoffPrepare).await? {
        DaemonResponse::HandoffReady { token } => token,
        DaemonResponse::HandoffBusy { reason } => bail!(
            "daemon {} (pid {old_pid}) is busy: {reason}; retry after its work finishes",
            current.version
        ),
        other => bail!("daemon declined handoff: {other:?}"),
    };
    let default_log = source.home().join("bitrouter.log");
    let result = handoff_under_gate(
        source,
        socket,
        &current,
        OldProcess {
            pid: old_pid,
            listen: &old_listen,
            log: explicit_profile_log.unwrap_or(&default_log),
            preserve_listen: explicit_profile_log.is_none(),
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
            && checked.build_id == old.build_id
            && checked.artifact == old.artifact,
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
    let replacement_socket = preflight.control_socket();
    ensure!(
        replacement_socket == socket || !daemon::endpoint_in_use(replacement_socket),
        "replacement control endpoint is already in use; the resident daemon has not been stopped"
    );
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
    let log = process.log;
    let ready = daemon::start_and_wait(
        source,
        log,
        Some(replacement_socket),
        Duration::from_secs(20),
    )
    .await;
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
    let replacement = hello(replacement_socket).await.with_context(|| {
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
            && replacement.artifact == crate::artifact::current()
            && replacement.cli_owned,
        "replacement daemon identity/version is wrong; log: {}; recovery backup: {}",
        log.display(),
        backup.display()
    );
    ensure!(
        !process.preserve_listen || info.listen == process.listen,
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
        new_location.pid() == info.pid && new_location.socket() == replacement_socket,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_gateway_does_not_match_local_gateway_or_custom_handoff_identity() {
        use bitrouter_ai::artifact::DistributionProfile;

        let mut shared = crate::artifact::current();
        shared.profile = DistributionProfile::GatewayShared;
        shared.features = vec!["db-postgres".into(), "multi-instance".into()];
        assert!(same_capabilities(&shared, &shared));
        let mut local = shared.clone();
        local.profile = DistributionProfile::Gateway;
        local.features.clear();
        assert!(!same_capabilities(&shared, &local));
        let mut old_custom = shared.clone();
        old_custom.profile = DistributionProfile::Custom;
        assert!(!same_capabilities(&shared, &old_custom));
    }

    fn reply(protocol: u32, artifact: Option<ArtifactIdentity>) -> DaemonResponse {
        DaemonResponse::HandoffHello {
            version: crate::VERSION.into(),
            protocol,
            instance_id: "fixture-instance".into(),
            build_id: crate::HANDOFF_BUILD_ID.into(),
            cli_owned: true,
            artifact,
        }
    }

    #[test]
    fn handoff_requires_current_protocol_and_consistent_artifact_evidence() -> Result<()> {
        let current = crate::artifact::current();
        let accepted = decode_hello(reply(HANDOFF_PROTOCOL, Some(current.clone())))?;
        assert_eq!(accepted.artifact, current);
        assert!(decode_hello(reply(1, Some(current.clone()))).is_err());
        assert!(decode_hello(reply(HANDOFF_PROTOCOL, None)).is_err());

        let mut wrong = current.clone();
        wrong.version = "0.0.0".into();
        assert!(decode_hello(reply(HANDOFF_PROTOCOL, Some(wrong))).is_err());
        let mut wrong = current.clone();
        wrong.handoff_build_id = "another-build".into();
        assert!(decode_hello(reply(HANDOFF_PROTOCOL, Some(wrong))).is_err());
        let mut wrong = current;
        wrong.features = vec!["tui".into(), "harness".into()];
        assert!(decode_hello(reply(HANDOFF_PROTOCOL, Some(wrong))).is_err());
        Ok(())
    }

    #[test]
    fn legacy_hello_does_not_infer_gateway_from_absent_artifact() -> Result<()> {
        let legacy = serde_json::json!({
            "resp":"handoff_hello", "version":crate::VERSION, "protocol":1,
            "instance_id":"legacy", "build_id":crate::HANDOFF_BUILD_ID, "cli_owned":true,
        });
        let reply: DaemonResponse = serde_json::from_value(legacy)?;
        assert!(matches!(
            &reply,
            DaemonResponse::HandoffHello { artifact: None, .. }
        ));
        assert!(decode_hello(reply).is_err());
        Ok(())
    }
}
