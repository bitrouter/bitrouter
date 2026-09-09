//! Config source resolution for the OSS binary.
//!
//! When a CLI subcommand doesn't pass `-c <path>` explicitly, the
//! binary walks a fixed resolution order so it can be run from anywhere:
//!
//! 1. **Explicit `-c <path>`** — handed in by the caller. Used as-is. If
//!    the file is missing, we surface a clear error (do **not** silently
//!    fall through to zero-config — an explicit user choice deserves a
//!    real failure).
//! 2. **`./bitrouter.yaml`** in the current working directory.
//! 3. **`$BITROUTER_HOME/bitrouter.yaml`** — if the env var is set and
//!    points at a directory containing the file.
//! 4. **`~/.bitrouter/bitrouter.yaml`** — used as-is if present.
//! 5. **Zero-config in-memory defaults** — used when nothing on steps
//!    2-4 exists, with `~/.bitrouter` as the implicit home for the
//!    daemon's runtime artefacts (socket, pid, log, db). No file is
//!    written; `bro init` is the explicit way to scaffold a YAML.
//!
//! The two outcomes are surfaced as [`ConfigSource`] variants
//! ([`ConfigSource::File`] / [`ConfigSource::Default`]) so each
//! subcommand can decide whether to load from disk or build from
//! [`bitrouter_providers::zero_config`].
//!
//! On Windows `$HOME` is usually unset, so step 4/5 fall back to
//! `%USERPROFILE%` (→ `C:\Users\<name>\.bitrouter`). With neither set,
//! step 5 degrades to a clear error pointing at `$BITROUTER_HOME`. Tests
//! should always pass `-c <path>` explicitly so they never depend on the
//! live env.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use bitrouter_sdk::invocation;
use rand::Rng;

use crate::trajectory::canonical::CorrelationKey;

/// The fixed config filename inside any home directory.
const CONFIG_FILENAME: &str = "bitrouter.yaml";

/// Where a config comes from. Returned by [`resolve_config`].
///
/// - [`ConfigSource::File`] — a real `bitrouter.yaml` exists. Load it
///   via `config::load`.
/// - [`ConfigSource::Default`] — no file found. Build an in-memory
///   `Config` via [`bitrouter_providers::zero_config`]. The associated
///   `home` is the directory where the daemon should place its
///   runtime artefacts (socket / pid / log / db).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// A `bitrouter.yaml` resolved from one of cwd / `$BITROUTER_HOME` /
    /// `~/.bitrouter`. Path is absolute.
    File(PathBuf),
    /// No config file found; use zero-config defaults. The contained
    /// path is the implicit bitrouter home (typically `~/.bitrouter`)
    /// — created on demand by the daemon when it chdirs there.
    Default {
        /// The implicit home directory.
        home: PathBuf,
    },
}

impl ConfigSource {
    /// The home directory associated with this source — for `File` it's
    /// the config file's parent; for `Default` it's the implicit
    /// `~/.bitrouter` home. The daemon chdirs here on startup so the
    /// socket / pid / log / db all land in one place.
    pub fn home(&self) -> &Path {
        match self {
            Self::File(path) => path.parent().unwrap_or(Path::new(".")),
            Self::Default { home } => home,
        }
    }

    /// True if no file was found and zero-config defaults will be used.
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default { .. })
    }
}

/// Resolve the config source according to the documented order. Reads
/// the live environment (`current_dir`, `$BITROUTER_HOME`, `$HOME`, and
/// `%USERPROFILE%` on Windows as the `$HOME` fallback).
/// Does **not** write anything to disk — the [`ConfigSource::Default`]
/// branch is purely in-memory until a caller (typically `serve`) chdirs
/// into the implicit home.
///
/// For testable / dependency-injected resolution see
/// [`resolve_config_with`].
pub fn resolve_config(explicit: Option<&Path>) -> Result<ConfigSource> {
    let cwd = std::env::current_dir().ok();
    let bitrouter_home = std::env::var_os("BITROUTER_HOME").filter(|v| !v.is_empty());
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty());
    // Windows doesn't set `$HOME`; fall back to `%USERPROFILE%` so
    // `~/.bitrouter` resolves to `C:\Users\<name>\.bitrouter` and the daemon's
    // runtime artefacts (socket/pipe, pid, log, db) get a stable home without
    // the operator having to set `$BITROUTER_HOME` by hand.
    #[cfg(windows)]
    let home = home.or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()));
    let outcome = resolve_config_with(
        explicit,
        cwd.as_deref(),
        bitrouter_home.as_deref().map(Path::new),
        home.as_deref().map(Path::new),
    )?;
    // Always hand back absolute paths. Downstream code chdirs to the
    // bitrouter home so the daemon doesn't depend on the launcher's
    // CWD; a relative `-c ./foo.yaml` would get lost once the chdir
    // happens. Absolutising without following symlinks keeps the
    // displayed path readable.
    Ok(match outcome {
        ConfigSource::File(path) => ConfigSource::File(absolutize(path)),
        ConfigSource::Default { home } => ConfigSource::Default {
            home: absolutize(home),
        },
    })
}

/// Make `path` absolute by joining it onto the current working
/// directory if necessary. Does **not** follow symlinks
/// (`std::fs::canonicalize` would, which on macOS turns `/tmp` into
/// `/private/tmp` and surprises users).
fn absolutize(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path,
    }
}

/// Pure resolution logic. Takes the cwd / env-var values that the live
/// version reads from the process, returns the resolution decision
/// without performing any side effects.
pub fn resolve_config_with(
    explicit: Option<&Path>,
    cwd: Option<&Path>,
    bitrouter_home_env: Option<&Path>,
    home_env: Option<&Path>,
) -> Result<ConfigSource> {
    // 1. explicit -c path — use as-is, surface a clear error if missing.
    if let Some(path) = explicit {
        let p = path.to_path_buf();
        if !p.exists() {
            anyhow::bail!(
                "config file '{}' does not exist (passed via -c). \
                 Drop the flag to fall back to the resolution order \
                 (cwd → $BITROUTER_HOME → ~/.bitrouter → zero-config).",
                p.display()
            );
        }
        return Ok(ConfigSource::File(p));
    }

    // 2. cwd / bitrouter.yaml
    if let Some(cwd) = cwd {
        let candidate = cwd.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Ok(ConfigSource::File(candidate));
        }
    }

    // 3. $BITROUTER_HOME / bitrouter.yaml. If the env var is set, that
    // directory must contain the file — fail loudly otherwise rather
    // than silently falling through to zero-config. An operator who
    // set BITROUTER_HOME intended that directory to win.
    if let Some(env_home) = bitrouter_home_env {
        let candidate = env_home.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Ok(ConfigSource::File(candidate));
        }
        anyhow::bail!(
            "BITROUTER_HOME is set to '{}' but '{}' is missing there. \
             Either drop the env var or create the file (e.g. \
             `{} init -c $BITROUTER_HOME/{}`).",
            env_home.display(),
            CONFIG_FILENAME,
            invocation::name(),
            CONFIG_FILENAME,
        );
    }

    // 4. ~/.bitrouter / bitrouter.yaml — used as-is if present.
    // 5. Otherwise zero-config in-memory defaults with `~/.bitrouter`
    //    as the implicit home (created on demand by the daemon).
    let home = home_env.context(
        "could not determine home directory (no $HOME set); set $BITROUTER_HOME or pass -c <path>",
    )?;
    let home = home.join(".bitrouter");
    let candidate = home.join(CONFIG_FILENAME);
    if candidate.is_file() {
        return Ok(ConfigSource::File(candidate));
    }
    Ok(ConfigSource::Default { home })
}

/// Load a [`bitrouter_sdk::config::Config`] from a [`ConfigSource`].
/// `ConfigSource::File` reads from disk via the SDK's loader;
/// `ConfigSource::Default` builds the zero-config in-memory default
/// from [`bitrouter_providers::zero_config`].
///
/// This is the one place `serve` / `start` / `models` / `route` etc.
/// reach for a `Config` — every call site goes through here so the
/// zero-config story is wired in uniformly.
pub async fn load_config(source: &ConfigSource) -> Result<bitrouter_sdk::config::Config> {
    match source {
        ConfigSource::File(path) => bitrouter_sdk::config::load(path)
            .await
            .with_context(|| format!("loading {}", path.display())),
        ConfigSource::Default { .. } => {
            let mut cfg = bitrouter_providers::zero_config();
            // Layered on top of the env-var-driven auto-enable: a signed-in
            // user (credentials file present) gets the `bitrouter` provider
            // even without `$BITROUTER_API_KEY` in their shell.
            crate::cloud::enable_in_zero_config(&mut cfg);
            Ok(cfg)
        }
    }
}

/// Ensure the bitrouter home directory exists, creating it with `0o700`
/// permissions on Unix (the operator may drop secrets like `<home>/.env`
/// inside later). Idempotent. Called by the daemon on entry when
/// running zero-config so the runtime artefacts have a stable place to
/// live, and by `bro init` before writing the starter file.
pub fn ensure_home_directory(home: &Path) -> Result<()> {
    std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(home) {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(home, perms);
        }
    }
    Ok(())
}

/// Filename of the persisted anonymous install identifier inside the home.
const INSTALL_ID_FILENAME: &str = "installation.id";
const CORRELATION_KEY_FILENAME: &str = "correlation.key";
const CONTINUATION_KEY_FILENAME: &str = "continuation.key";
const INSTALL_LOCK_FILENAME: &str = ".installation.lock";

/// Read the stable anonymous install id from `<home>/installation.id`,
/// generating and persisting a fresh UUID v4 on first call. The id is
/// vendor-neutral telemetry plumbing: it lets opt-in exports be attributed to
/// an install without any account or PII. Idempotent — the same id is returned
/// on every subsequent call for a given home.
///
/// A malformed/empty existing file is treated as missing and rewritten.
pub fn get_or_create_install_id(home: &Path) -> Result<String> {
    ensure_home_directory(home)?;
    with_install_lock(home, || get_or_create_install_id_locked(home))
}

fn get_or_create_install_id_locked(home: &Path) -> Result<String> {
    let path = home.join(INSTALL_ID_FILENAME);
    if let Some(contents) = read_private_text(&path)? {
        let trimmed = contents.trim();
        if uuid::Uuid::parse_str(trimmed).is_ok() {
            return Ok(trimmed.to_owned());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if path.exists() {
        replace_private_file(&path, id.as_bytes())
            .with_context(|| format!("rewriting {}", path.display()))?;
    } else {
        create_private_file(&path, id.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(id)
}

/// Load the installation-private HMAC key used only for trajectory correlation,
/// atomically creating it on the first process start.
pub fn get_or_create_correlation_key(home: &Path) -> Result<CorrelationKey> {
    ensure_home_directory(home)?;
    with_install_lock(home, || {
        get_or_create_install_id_locked(home)?;
        let path = home.join(CORRELATION_KEY_FILENAME);
        if let Some(secret) = read_correlation_secret(&path)? {
            return CorrelationKey::from_bytes(secret);
        }

        let mut secret = [0_u8; 32];
        rand::rng().fill_bytes(&mut secret);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
        create_private_file(&path, encoded.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        CorrelationKey::from_bytes(secret)
    })
}

/// Load the installation-private provider-continuation encryption key,
/// atomically creating it only when continuation functionality is first used.
pub fn get_or_create_continuation_key(home: &Path) -> Result<[u8; 32]> {
    ensure_home_directory(home)?;
    with_install_lock(home, || {
        get_or_create_install_id_locked(home)?;
        let path = home.join(CONTINUATION_KEY_FILENAME);
        if let Some(encoded) = read_private_text(&path)? {
            let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded.trim())
                .context("existing continuation key is not valid base64")?;
            return decoded.try_into().map_err(|_| {
                anyhow::anyhow!("existing continuation key must contain exactly 32 bytes")
            });
        }

        let mut secret = [0_u8; 32];
        rand::rng().fill_bytes(&mut secret);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
        create_private_file(&path, encoded.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(secret)
    })
}

fn read_correlation_secret(path: &Path) -> Result<Option<[u8; 32]>> {
    let Some(encoded) = read_private_text(path)? else {
        return Ok(None);
    };
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded.trim())
        .context("existing correlation key is not valid base64")?;
    let secret = decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("existing correlation key must contain exactly 32 bytes"))?;
    Ok(Some(secret))
}

fn with_install_lock<T>(home: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_path = home.join(INSTALL_LOCK_FILENAME);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock_file = options
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    repair_private_permissions(&lock_path)?;
    lock_file
        .lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    operation()
}

fn read_private_text(path: &Path) -> Result<Option<String>> {
    if path.exists() {
        repair_private_permissions(path)?;
    }
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn repair_private_permissions(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("repairing permissions on {}", _path.display()))?;
    }
    Ok(())
}

fn create_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if path.exists() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    publish_private_file(path, contents, false)
}

fn replace_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    publish_private_file(path, contents, true)
}

fn publish_private_file(path: &Path, contents: &[u8], replace: bool) -> std::io::Result<()> {
    use std::io::Write;

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private file requires a parent directory",
        )
    })?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "private file requires a UTF-8 filename",
            )
        })?;
    let temporary = parent.join(format!(".{filename}.{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
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
    if !replace && path.exists() {
        let _ = std::fs::remove_file(&temporary);
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    let publication = if replace {
        atomic_replace(&temporary, path)
    } else {
        std::fs::rename(&temporary, path)
    };
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

/// Resolve the bitrouter home the same way the daemon's runtime artefacts do:
/// `$BITROUTER_HOME`, else `$HOME/.bitrouter`, with `%USERPROFILE%` as the
/// Windows `$HOME` fallback.
///
/// For callers that have no [`ConfigSource`] in scope — the telemetry opt-in,
/// and the session log, which is opened before any config is resolved.
pub fn runtime_home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("BITROUTER_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty());
    #[cfg(windows)]
    let home = home.or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()));
    let home =
        home.context("could not determine home directory (no $HOME set); set $BITROUTER_HOME")?;
    Ok(PathBuf::from(home).join(".bitrouter"))
}

/// The stable install id under [`runtime_home`]. Used by the telemetry opt-in.
pub fn install_id() -> Result<String> {
    get_or_create_install_id(&runtime_home()?)
}

/// Sentinel filename marking that the first-run telemetry notice has been shown.
const TELEMETRY_NOTICE_SENTINEL: &str = ".telemetry-notice-shown";

/// One-shot guard for the first-run telemetry notice. Returns `Ok(true)` the
/// first time it is called for a given home (creating the sentinel), `Ok(false)`
/// thereafter — so the caller prints the notice exactly once per install.
pub fn mark_telemetry_notice_shown(home: &Path) -> Result<bool> {
    let path = home.join(TELEMETRY_NOTICE_SENTINEL);
    if path.exists() {
        return Ok(false);
    }
    ensure_home_directory(home)?;
    std::fs::write(&path, "").with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-paths-test-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn install_id_is_generated_then_stable() {
        let home = unique_tmp("install-id");
        let first = get_or_create_install_id(&home).unwrap();
        assert!(!first.is_empty());
        // UUID v4 string form is 36 chars.
        assert_eq!(first.len(), 36);
        // A second call returns the same persisted id.
        let second = get_or_create_install_id(&home).unwrap();
        assert_eq!(first, second);
        // And it really is on disk.
        let on_disk = std::fs::read_to_string(home.join(INSTALL_ID_FILENAME)).unwrap();
        assert_eq!(on_disk.trim(), first);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn correlation_key_is_stable_private_and_beside_installation_id() {
        let home = unique_tmp("correlation-key");
        let first = get_or_create_correlation_key(&home).unwrap();
        let second = get_or_create_correlation_key(&home).unwrap();
        assert_eq!(first.key_id(), second.key_id());
        assert!(home.join(INSTALL_ID_FILENAME).is_file());
        assert!(home.join(CORRELATION_KEY_FILENAME).is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.join(CORRELATION_KEY_FILENAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn continuation_key_is_stable_private_and_corruption_fails_closed() {
        let home = unique_tmp("continuation-key");
        let first = get_or_create_continuation_key(&home).unwrap();
        let second = get_or_create_continuation_key(&home).unwrap();
        assert_eq!(first, second);
        let path = home.join(CONTINUATION_KEY_FILENAME);
        assert!(path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let invalid = b"not-a-continuation-key";
        std::fs::write(&path, invalid).unwrap();
        let error = get_or_create_continuation_key(&home).unwrap_err();
        assert!(error.to_string().contains("continuation key"));
        assert_eq!(std::fs::read(&path).unwrap(), invalid);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn concurrent_first_start_creates_one_atomic_correlation_key() {
        let home = unique_tmp("correlation-key-race");
        let handles = (0..8)
            .map(|_| {
                let home = home.clone();
                std::thread::spawn(move || {
                    get_or_create_correlation_key(&home)
                        .unwrap()
                        .key_id()
                        .to_owned()
                })
            })
            .collect::<Vec<_>>();
        let mut key_ids = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        key_ids.dedup();
        assert_eq!(key_ids.len(), 1);
        let encoded = std::fs::read_to_string(home.join(CORRELATION_KEY_FILENAME)).unwrap();
        assert!(!encoded.trim().is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn existing_install_and_key_permissions_are_repaired_on_read() {
        use std::os::unix::fs::PermissionsExt;

        let home = unique_tmp("private-file-repair");
        let install_path = home.join(INSTALL_ID_FILENAME);
        let key_path = home.join(CORRELATION_KEY_FILENAME);
        std::fs::write(&install_path, uuid::Uuid::new_v4().to_string()).unwrap();
        std::fs::write(
            &key_path,
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([23_u8; 32]),
        )
        .unwrap();
        std::fs::set_permissions(&install_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        get_or_create_correlation_key(&home).unwrap();

        for path in [install_path, key_path] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn install_id_repair_does_not_change_correlation_key_identity() {
        let home = unique_tmp("install-id-key-identity");
        let first = get_or_create_correlation_key(&home).unwrap();
        let original_key = std::fs::read(home.join(CORRELATION_KEY_FILENAME)).unwrap();
        std::fs::write(home.join(INSTALL_ID_FILENAME), "partial-id").unwrap();

        let repaired = get_or_create_correlation_key(&home).unwrap();

        assert_eq!(first.key_id(), repaired.key_id());
        let install = std::fs::read_to_string(home.join(INSTALL_ID_FILENAME)).unwrap();
        assert!(uuid::Uuid::parse_str(install.trim()).is_ok());
        assert_eq!(
            std::fs::read(home.join(CORRELATION_KEY_FILENAME)).unwrap(),
            original_key
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn corrupt_existing_correlation_key_fails_closed_without_replacement() {
        for (label, contents) in [
            ("empty", b"".as_slice()),
            ("not-base64", b"not a key".as_slice()),
            ("wrong-length", b"c2hvcnQ".as_slice()),
        ] {
            let home = unique_tmp(&format!("corrupt-key-{label}"));
            std::fs::write(
                home.join(INSTALL_ID_FILENAME),
                uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
            std::fs::write(home.join(CORRELATION_KEY_FILENAME), contents).unwrap();

            let error = match get_or_create_correlation_key(&home) {
                Ok(_) => panic!("an existing invalid correlation key must not rotate"),
                Err(error) => error,
            };

            assert!(
                error.to_string().contains("correlation key"),
                "{label}: {error}"
            );
            assert_eq!(
                std::fs::read(home.join(CORRELATION_KEY_FILENAME)).unwrap(),
                contents,
                "{label} key was replaced"
            );
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn missing_key_on_existing_install_initializes_once() {
        let home = unique_tmp("pre-feature-install");
        let install_id = uuid::Uuid::new_v4().to_string();
        std::fs::write(home.join(INSTALL_ID_FILENAME), &install_id).unwrap();

        let first = get_or_create_correlation_key(&home).unwrap();
        let encoded = std::fs::read(home.join(CORRELATION_KEY_FILENAME)).unwrap();
        let second = get_or_create_correlation_key(&home).unwrap();

        assert_eq!(first.key_id(), second.key_id());
        assert_eq!(
            std::fs::read(home.join(CORRELATION_KEY_FILENAME)).unwrap(),
            encoded
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(windows)]
    #[test]
    fn replacing_private_file_overwrites_existing_destination_on_windows() {
        let home = unique_tmp("windows-private-replace");
        let path = home.join("replace.private");
        std::fs::write(&path, "old").unwrap();

        replace_private_file(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn concurrent_recovery_of_empty_install_id_returns_one_stable_id() {
        let home = unique_tmp("install-id-empty-race");
        std::fs::write(home.join(INSTALL_ID_FILENAME), "").unwrap();
        let handles = (0..16)
            .map(|_| {
                let home = home.clone();
                std::thread::spawn(move || get_or_create_install_id(&home).unwrap())
            })
            .collect::<Vec<_>>();
        let mut ids = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 1);
        assert_eq!(
            std::fs::read_to_string(home.join(INSTALL_ID_FILENAME))
                .unwrap()
                .trim(),
            ids[0]
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn private_file_creation_never_publishes_partial_final_contents() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Barrier};

        let home = unique_tmp("private-file-publication");
        let path = home.join("large.private");
        let contents = vec![37_u8; 64 * 1024 * 1024];
        let expected_len = u64::try_from(contents.len()).unwrap();
        let started = Arc::new(Barrier::new(2));
        let finished = Arc::new(AtomicBool::new(false));
        let writer = {
            let path = path.clone();
            let started = Arc::clone(&started);
            let finished = Arc::clone(&finished);
            std::thread::spawn(move || {
                started.wait();
                let result = create_private_file(&path, &contents);
                finished.store(true, Ordering::Release);
                result
            })
        };
        started.wait();
        let mut observed_partial = false;
        while !finished.load(Ordering::Acquire) {
            if let Ok(metadata) = std::fs::metadata(&path)
                && metadata.len() != expected_len
            {
                observed_partial = true;
                break;
            }
            std::thread::yield_now();
        }
        writer.join().unwrap().unwrap();
        assert!(!observed_partial, "final path exposed a partial write");
        assert_eq!(std::fs::metadata(path).unwrap().len(), expected_len);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn telemetry_notice_marked_once() {
        let home = unique_tmp("tel-notice");
        assert!(mark_telemetry_notice_shown(&home).unwrap());
        assert!(!mark_telemetry_notice_shown(&home).unwrap());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn install_id_rewrites_empty_file() {
        let home = unique_tmp("install-id-empty");
        std::fs::write(home.join(INSTALL_ID_FILENAME), "  \n").unwrap();
        let id = get_or_create_install_id(&home).unwrap();
        assert_eq!(id.len(), 36);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn explicit_path_is_used_verbatim_when_it_exists() {
        let dir = unique_tmp("explicit-ok");
        let path = dir.join("bitrouter.yaml");
        std::fs::write(&path, "server: {listen: '127.0.0.1:0'}").unwrap();
        let resolved = resolve_config_with(Some(&path), None, None, None).unwrap();
        assert_eq!(resolved, ConfigSource::File(path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_missing_path_errors_clearly() {
        let dir = unique_tmp("explicit-missing");
        let path = dir.join("nope.yaml");
        let err = resolve_config_with(Some(&path), None, None, None).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cwd_bitrouter_yaml_wins_over_env_and_home() {
        let cwd = unique_tmp("cwd-hit");
        let path = cwd.join("bitrouter.yaml");
        std::fs::write(&path, "server: {listen: '127.0.0.1:0'}").unwrap();
        let env_home = unique_tmp("env-distractor");
        let home_env = unique_tmp("home-distractor");
        let resolved =
            resolve_config_with(None, Some(&cwd), Some(&env_home), Some(&home_env)).unwrap();
        assert_eq!(resolved, ConfigSource::File(path));
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&env_home);
        let _ = std::fs::remove_dir_all(&home_env);
    }

    #[test]
    fn bitrouter_home_env_resolves_when_file_exists() {
        let env_home = unique_tmp("env-hit");
        let path = env_home.join("bitrouter.yaml");
        std::fs::write(&path, "server: {listen: '127.0.0.1:0'}").unwrap();
        let cwd = unique_tmp("env-parking");
        let resolved = resolve_config_with(None, Some(&cwd), Some(&env_home), None).unwrap();
        assert_eq!(resolved, ConfigSource::File(path));
        let _ = std::fs::remove_dir_all(&env_home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn bitrouter_home_set_but_file_missing_errors_with_hint() {
        let env_home = unique_tmp("env-empty");
        let cwd = unique_tmp("env-empty-parking");
        let err = resolve_config_with(None, Some(&cwd), Some(&env_home), None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("BITROUTER_HOME") && msg.contains("missing"),
            "error should hint at BITROUTER_HOME: {msg}"
        );
        let _ = std::fs::remove_dir_all(&env_home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn falls_back_to_zero_config_when_nothing_else_matches() {
        let home_root = unique_tmp("home-fallback");
        // No cwd, no env, and ~/.bitrouter/bitrouter.yaml doesn't exist.
        // Resolution decides on zero-config defaults with the implicit
        // home pointing at ~/.bitrouter.
        let resolved = resolve_config_with(None, None, None, Some(&home_root)).unwrap();
        match resolved {
            ConfigSource::Default { home } => {
                assert_eq!(home, home_root.join(".bitrouter"));
            }
            other => panic!("expected ConfigSource::Default, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&home_root);
    }

    #[test]
    fn dot_bitrouter_existing_config_is_used_directly() {
        let home_root = unique_tmp("home-existing");
        let dot = home_root.join(".bitrouter");
        std::fs::create_dir_all(&dot).unwrap();
        let path = dot.join("bitrouter.yaml");
        std::fs::write(&path, "server: {listen: '127.0.0.1:0'}").unwrap();
        let resolved = resolve_config_with(None, None, None, Some(&home_root)).unwrap();
        assert_eq!(resolved, ConfigSource::File(path));
        let _ = std::fs::remove_dir_all(&home_root);
    }

    #[test]
    fn config_source_default_reports_its_home() {
        let source = ConfigSource::Default {
            home: PathBuf::from("/tmp/x"),
        };
        assert_eq!(source.home(), Path::new("/tmp/x"));
        assert!(source.is_default());
    }

    #[test]
    fn config_source_file_reports_parent_as_home() {
        let source = ConfigSource::File(PathBuf::from("/tmp/x/bitrouter.yaml"));
        assert_eq!(source.home(), Path::new("/tmp/x"));
        assert!(!source.is_default());
    }

    #[test]
    fn no_home_and_no_env_errors_with_helpful_message() {
        let err = resolve_config_with(None, None, None, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("BITROUTER_HOME") || msg.contains("HOME"),
            "error should mention how to recover: {msg}"
        );
    }
}
