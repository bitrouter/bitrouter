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
//!    written; `bitrouter init` is the explicit way to scaffold a YAML.
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
             `bitrouter init -c $BITROUTER_HOME/{}`).",
            env_home.display(),
            CONFIG_FILENAME,
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
/// live, and by `bitrouter init` before writing the starter file.
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

/// Read the stable anonymous install id from `<home>/installation.id`,
/// generating and persisting a fresh UUID v4 on first call. The id is
/// vendor-neutral telemetry plumbing: it lets opt-in exports be attributed to
/// an install without any account or PII. Idempotent — the same id is returned
/// on every subsequent call for a given home.
///
/// A malformed/empty existing file is treated as missing and rewritten.
pub fn get_or_create_install_id(home: &Path) -> Result<String> {
    let path = home.join(INSTALL_ID_FILENAME);
    if let Ok(contents) = std::fs::read_to_string(&path) {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    ensure_home_directory(home)?;
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&path, &id).with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

/// Resolve the bitrouter home the same way the daemon's runtime artefacts do
/// (`$BITROUTER_HOME`, else `$HOME/.bitrouter`, with `%USERPROFILE%` as the
/// Windows `$HOME` fallback) and return its stable install id. Used by the
/// telemetry opt-in, which has no [`ConfigSource`] in scope.
pub fn install_id() -> Result<String> {
    let home = if let Some(h) = std::env::var_os("BITROUTER_HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(h)
    } else {
        let home = std::env::var_os("HOME").filter(|v| !v.is_empty());
        #[cfg(windows)]
        let home = home.or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()));
        let home = home.context(
            "could not determine home directory (no $HOME set); set $BITROUTER_HOME",
        )?;
        PathBuf::from(home).join(".bitrouter")
    };
    get_or_create_install_id(&home)
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
