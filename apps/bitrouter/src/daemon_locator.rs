//! Stable discovery for a local daemon's actual control endpoint.
//!
//! The configured control socket is saved state. A running daemon keeps the
//! socket it bound at startup, so editing or breaking `bitrouter.yaml` must not
//! make lifecycle and inspection commands lose that process. Each daemon
//! publishes a small private locator beside its logical configuration source.
//! Clients trust it only when the endpoint reports the recorded process id and
//! boot-local server instance id.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::daemon::{self, DaemonCommand, DaemonResponse};
use crate::paths::ConfigSource;

const LOCATOR_VERSION: u8 = 1;
const CONFIG_FILENAME: &str = "bitrouter.yaml";
#[cfg(not(test))]
const LOCATOR_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const LOCATOR_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LocatorSourceKind {
    File,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LocatorRecord {
    version: u8,
    config_identity: String,
    source: LocatorSourceKind,
    socket: String,
    pid: u32,
    server_instance_id: String,
}

#[derive(Debug, Clone)]
struct ConfigIdentity {
    logical_path: PathBuf,
    home: PathBuf,
    digest: String,
}

impl ConfigIdentity {
    fn from_config_path(path: &Path) -> Result<Self> {
        let absolute = absolutize(path)?;
        let filename = absolute.file_name().ok_or_else(|| {
            anyhow::anyhow!(
                "configuration path '{}' has no file name",
                absolute.display()
            )
        })?;
        let parent = absolute.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "configuration path '{}' has no parent directory",
                absolute.display()
            )
        })?;
        // Canonicalize only the parent. This keeps a symlink used as the
        // logical config filename stable across deletion/recreation while
        // avoiding identity changes when its target changes.
        let home = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        let logical_path = home.join(filename);
        let digest = identity_digest(&logical_path);
        Ok(Self {
            logical_path,
            home,
            digest,
        })
    }

    fn for_source(source: &ConfigSource) -> Result<Self> {
        match source {
            ConfigSource::File(path) => Self::from_config_path(path),
            ConfigSource::Default { home } => Self::from_config_path(&home.join(CONFIG_FILENAME)),
        }
    }

    fn locator_path(&self) -> PathBuf {
        self.home
            .join(format!(".bitrouter-daemon-{}.json", self.digest))
    }

    fn lock_path(&self) -> PathBuf {
        self.home
            .join(format!(".bitrouter-daemon-{}.lock", self.digest))
    }
}

pub(crate) fn config_identity_digest(source: &ConfigSource) -> Result<String> {
    Ok(ConfigIdentity::for_source(source)?.digest)
}

/// A verified running daemon found through its source-specific locator.
#[derive(Debug, Clone)]
pub struct LocatedDaemon {
    source: ConfigSource,
    socket: PathBuf,
    pid: u32,
}

impl LocatedDaemon {
    /// Configuration source that owns the locator.
    pub fn source(&self) -> &ConfigSource {
        &self.source
    }

    /// Actual control endpoint bound by the running daemon.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Process id verified through the control endpoint.
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

/// Registration kept alive by `serve` for the lifetime of one daemon.
///
/// Dropping it removes the locator only when the on-disk record still belongs
/// to this exact process incarnation. A replaced record is left untouched.
pub struct DaemonLocatorRegistration {
    identity: ConfigIdentity,
    record: LocatorRecord,
}

impl Drop for DaemonLocatorRegistration {
    fn drop(&mut self) {
        if let Err(error) = remove_if_current(&self.identity, &self.record) {
            tracing::warn!(error = %error, "removing daemon locator failed");
        }
    }
}

/// Publish the actual endpoint for a daemon process.
///
/// The returned registration must remain alive until daemon shutdown.
pub fn publish(
    source: &ConfigSource,
    socket: &Path,
    server_instance_id: &str,
) -> Result<DaemonLocatorRegistration> {
    if server_instance_id.is_empty() {
        anyhow::bail!("daemon locator requires a server instance id");
    }
    let socket = socket.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "control socket path '{}' is not valid UTF-8",
            socket.display()
        )
    })?;
    let identity = ConfigIdentity::for_source(source)?;
    crate::paths::ensure_home_directory(&identity.home)?;
    let record = LocatorRecord {
        version: LOCATOR_VERSION,
        config_identity: identity.digest.clone(),
        source: match source {
            ConfigSource::File(_) => LocatorSourceKind::File,
            ConfigSource::Default { .. } => LocatorSourceKind::Default,
        },
        socket: socket.to_string(),
        pid: std::process::id(),
        server_instance_id: server_instance_id.to_string(),
    };
    write_record(&identity, &record)?;
    Ok(DaemonLocatorRegistration { identity, record })
}

/// Find a running daemon for the config selector a CLI command would use.
///
/// Only identities in the existing config precedence chain are considered.
/// Locator files are never scanned, and an explicit config selects exactly
/// one identity even when the file has since been deleted.
pub async fn locate(config: Option<&Path>) -> Result<Option<LocatedDaemon>> {
    for identity in resolution_identities(config)? {
        if let Some(located) = locate_identity(&identity).await {
            return Ok(Some(located));
        }
    }
    Ok(None)
}

/// Find a running daemon for one already resolved configuration source.
pub async fn locate_source(source: &ConfigSource) -> Result<Option<LocatedDaemon>> {
    let identity = ConfigIdentity::for_source(source)?;
    Ok(locate_identity(&identity).await)
}

/// Verify that an endpoint belongs to the expected process incarnation.
///
/// `serve` uses this after binding the control socket and before publishing
/// its locator, so a client can never observe a locator for an unbound socket.
pub async fn endpoint_matches(socket: &Path, pid: u32, server_instance_id: &str) -> bool {
    let response = tokio::time::timeout(
        LOCATOR_PROBE_TIMEOUT,
        daemon::send_command(socket, &DaemonCommand::Status),
    )
    .await;
    matches!(
        response,
        Ok(Ok(DaemonResponse::Status {
            pid: actual_pid,
            config_state: Some(state),
            ..
        })) if actual_pid == pid
            && state.server_instance_id.as_deref() == Some(server_instance_id)
    )
}

/// Resolve the currently selected source while retaining a deleted explicit
/// file (or a deleted `$BITROUTER_HOME/bitrouter.yaml`) as an inspectable
/// source. Commands that need to start or reload still load the file and fail;
/// passive status can report the source as missing and the daemon as stopped.
pub fn selected_source(config: Option<&Path>) -> Result<ConfigSource> {
    match crate::paths::resolve_config(config) {
        Ok(source) => Ok(source),
        Err(error) => {
            if let Some(path) = config {
                return Ok(ConfigSource::File(
                    ConfigIdentity::from_config_path(path)?.logical_path,
                ));
            }
            if let Some(home) = std::env::var_os("BITROUTER_HOME").filter(|value| !value.is_empty())
            {
                return Ok(ConfigSource::File(
                    ConfigIdentity::from_config_path(&PathBuf::from(home).join(CONFIG_FILENAME))?
                        .logical_path,
                ));
            }
            Err(error)
        }
    }
}

async fn locate_identity(identity: &ConfigIdentity) -> Option<LocatedDaemon> {
    let record = match read_record(identity) {
        Ok(Some(record)) => record,
        Ok(None) => return None,
        Err(error) => {
            tracing::debug!(error = %error, "ignoring unreadable daemon locator");
            return None;
        }
    };
    if !record_is_well_formed(identity, &record) {
        tracing::debug!(path = %identity.locator_path().display(), "ignoring malformed daemon locator");
        return None;
    }
    let socket = PathBuf::from(&record.socket);
    let response = match tokio::time::timeout(
        LOCATOR_PROBE_TIMEOUT,
        daemon::send_command(&socket, &DaemonCommand::Status),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            tracing::debug!(error = %error, "ignoring unreachable daemon locator");
            return None;
        }
        Err(_) => {
            tracing::debug!(socket = %socket.display(), "ignoring unresponsive daemon locator");
            return None;
        }
    };
    if !status_matches(&record, &response) {
        tracing::debug!(path = %identity.locator_path().display(), "ignoring stale daemon locator");
        return None;
    }
    // Do not return an endpoint from a record that changed while the probe was
    // in flight. The replacement may name another daemon incarnation.
    if !matches!(read_record(identity), Ok(Some(current)) if current == record) {
        return None;
    }
    let source = match record.source {
        LocatorSourceKind::File => ConfigSource::File(identity.logical_path.clone()),
        LocatorSourceKind::Default => ConfigSource::Default {
            home: identity.home.clone(),
        },
    };
    Some(LocatedDaemon {
        source,
        socket,
        pid: record.pid,
    })
}

fn status_matches(record: &LocatorRecord, response: &DaemonResponse) -> bool {
    match response {
        DaemonResponse::Status {
            pid, config_state, ..
        } => {
            *pid == record.pid
                && config_state
                    .as_ref()
                    .and_then(|state| state.server_instance_id.as_deref())
                    == Some(record.server_instance_id.as_str())
        }
        _ => false,
    }
}

fn record_is_well_formed(identity: &ConfigIdentity, record: &LocatorRecord) -> bool {
    record.version == LOCATOR_VERSION
        && record.config_identity == identity.digest
        && record.pid != 0
        && !record.socket.is_empty()
        && !record.server_instance_id.is_empty()
}

fn resolution_identities(explicit: Option<&Path>) -> Result<Vec<ConfigIdentity>> {
    if let Some(path) = explicit {
        return Ok(vec![ConfigIdentity::from_config_path(path)?]);
    }

    match crate::paths::resolve_config(None) {
        // A currently existing config is authoritative. A locator left by a
        // deleted file elsewhere in the search chain must not shadow it.
        Ok(ConfigSource::File(path)) => Ok(vec![ConfigIdentity::from_config_path(&path)?]),
        Ok(ConfigSource::Default { home }) => {
            let mut identities = Vec::new();
            let mut seen = BTreeSet::new();
            // With no file anywhere in the chain, retain the logical cwd
            // identity so a daemon whose cwd config was deleted is findable.
            if let Ok(cwd) = std::env::current_dir() {
                push_identity(&mut identities, &mut seen, &cwd.join(CONFIG_FILENAME))?;
            }
            push_identity(&mut identities, &mut seen, &home.join(CONFIG_FILENAME))?;
            Ok(identities)
        }
        Err(error) => {
            if let Some(home) = std::env::var_os("BITROUTER_HOME").filter(|value| !value.is_empty())
            {
                return Ok(vec![ConfigIdentity::from_config_path(
                    &PathBuf::from(home).join(CONFIG_FILENAME),
                )?]);
            }
            Err(error)
        }
    }
}

fn push_identity(
    identities: &mut Vec<ConfigIdentity>,
    seen: &mut BTreeSet<String>,
    path: &Path,
) -> Result<()> {
    let identity = ConfigIdentity::from_config_path(path)?;
    if seen.insert(identity.digest.clone()) {
        identities.push(identity);
    }
    Ok(())
}

fn absolutize(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("resolving relative configuration path")?
        .join(path))
}

#[cfg(unix)]
fn identity_digest(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    hex::encode(Sha256::digest(path.as_os_str().as_bytes()))
}

#[cfg(windows)]
fn identity_digest(path: &Path) -> String {
    hex::encode(Sha256::digest(
        path.to_string_lossy().to_ascii_lowercase().as_bytes(),
    ))
}

fn read_record(identity: &ConfigIdentity) -> Result<Option<LocatorRecord>> {
    let path = identity.locator_path();
    if !path.exists() {
        return Ok(None);
    }
    with_lock(identity, || match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    })
}

fn write_record(identity: &ConfigIdentity, record: &LocatorRecord) -> Result<()> {
    with_lock(identity, || {
        let mut contents = serde_json::to_vec(record).context("serializing daemon locator")?;
        contents.push(b'\n');
        replace_private_file(&identity.locator_path(), &contents)
            .with_context(|| format!("writing {}", identity.locator_path().display()))
    })
}

fn remove_if_current(identity: &ConfigIdentity, expected: &LocatorRecord) -> Result<()> {
    with_lock(identity, || {
        let path = identity.locator_path();
        let current = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<LocatorRecord>(&bytes).ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        if current.as_ref() == Some(expected) {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("removing {}", path.display()));
                }
            }
        }
        Ok(())
    })
}

fn with_lock<T>(identity: &ConfigIdentity, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let path = identity.lock_path();
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("repairing permissions on {}", path.display()))?;
    }
    file.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    operation()
}

fn replace_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "daemon locator requires a parent directory",
        )
    })?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "daemon locator requires a UTF-8 filename",
            )
        })?;
    let temporary = parent.join(format!(".{filename}.{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    let publication = atomic_replace(&temporary, path);
    if let Err(error) = publication {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    atomicwrites::replace_atomic(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reload::{
        AuxiliaryConfigState, ConfigSourceKind, ConfigurationState, RunningConfigState,
        SavedConfigState,
    };

    fn status(pid: u32, server_instance_id: Option<&str>) -> DaemonResponse {
        DaemonResponse::Status {
            pid,
            daemon_version: None,
            handoff_protocol: None,
            handoff_build_id: None,
            handoff_activity: None,
            cli_owned: None,
            listen: "127.0.0.1:4356".to_string(),
            models: 0,
            providers: Vec::new(),
            saved_routers: None,
            running_routers: None,
            router_restart_required: None,
            config_state: Some(ConfigurationState {
                server_instance_id: server_instance_id.map(str::to_string),
                generation: Some(0),
                source: ConfigSourceKind::File,
                saved: SavedConfigState::Available,
                running: RunningConfigState::InSync,
                reload_required_fields: Vec::new(),
                restart_required_fields: Vec::new(),
                named_policy: AuxiliaryConfigState::NotConfigured,
                access_policies: AuxiliaryConfigState::NotConfigured,
                last_reload: None,
                mixed_state_history: Vec::new(),
            }),
        }
    }

    #[test]
    fn status_must_match_pid_and_daemon_incarnation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let identity = ConfigIdentity::from_config_path(&directory.path().join(CONFIG_FILENAME))?;
        let record = LocatorRecord {
            version: LOCATOR_VERSION,
            config_identity: identity.digest,
            source: LocatorSourceKind::File,
            socket: directory.path().join("control.sock").display().to_string(),
            pid: 41,
            server_instance_id: "boot-a".to_string(),
        };
        assert!(status_matches(&record, &status(41, Some("boot-a"))));
        assert!(!status_matches(&record, &status(42, Some("boot-a"))));
        assert!(!status_matches(&record, &status(41, Some("boot-b"))));
        assert!(!status_matches(&record, &status(41, None)));
        Ok(())
    }

    #[test]
    fn config_identity_survives_file_deletion_and_recreation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(CONFIG_FILENAME);
        std::fs::write(&path, "server: {}\n")?;
        let before = resolution_identities(Some(&path))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("explicit config identity is missing"))?;
        std::fs::remove_file(&path)?;
        let missing = resolution_identities(Some(&path))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("deleted config identity is missing"))?;
        std::fs::write(&path, "server: { listen: 127.0.0.1:4356 }\n")?;
        let recreated = resolution_identities(Some(&path))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("recreated config identity is missing"))?;
        assert_eq!(before.digest, missing.digest);
        assert_eq!(before.digest, recreated.digest);
        assert_eq!(before.locator_path(), missing.locator_path());
        Ok(())
    }

    #[test]
    fn configs_in_one_directory_have_distinct_locators() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let first = ConfigIdentity::from_config_path(&directory.path().join("first.yaml"))?;
        let second = ConfigIdentity::from_config_path(&directory.path().join("second.yaml"))?;
        assert_ne!(first.digest, second.digest);
        assert_ne!(first.locator_path(), second.locator_path());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn registration_is_private_and_old_drop_preserves_replacement() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir()?;
        let source = ConfigSource::File(directory.path().join(CONFIG_FILENAME));
        let socket = directory.path().join("control.sock");
        let first = publish(&source, &socket, "boot-a")?;
        let path = first.identity.locator_path();
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );
        let second = publish(&source, &socket, "boot-b")?;
        drop(first);
        let current = read_record(&second.identity)?
            .ok_or_else(|| anyhow::anyhow!("replacement locator unexpectedly removed"))?;
        assert_eq!(current.server_instance_id, "boot-b");
        drop(second);
        assert!(!path.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unresponsive_stale_endpoint_is_bounded_and_ignored() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = ConfigSource::File(directory.path().join(CONFIG_FILENAME));
        let socket = directory.path().join("hung.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let registration = publish(&source, &socket, "boot-hung")?;
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await?;
            std::future::pending::<()>().await;
            Ok::<(), std::io::Error>(())
        });

        let located =
            tokio::time::timeout(std::time::Duration::from_secs(1), locate_source(&source))
                .await
                .context("locator probe did not respect its timeout")??;
        assert!(located.is_none());
        server.abort();
        drop(registration);
        Ok(())
    }
}
