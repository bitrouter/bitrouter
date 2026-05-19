//! Config file resolution for the OSS binary — v0-compatible.
//!
//! When a CLI subcommand doesn't pass `-c <path>` explicitly, the
//! binary walks a fixed resolution order so it can be run from anywhere:
//!
//! 1. **Explicit `-c <path>`** — handed in by the caller. Used as-is. If
//!    the file is missing, we surface a clear error (do **not** auto-
//!    scaffold over an explicit user choice).
//! 2. **`./bitrouter.yaml`** in the current working directory.
//! 3. **`$BITROUTER_HOME/bitrouter.yaml`** — if the env var is set and
//!    points at a directory containing the file.
//! 4. **`~/.bitrouter/bitrouter.yaml`** — auto-scaffolded with
//!    [`crate::commands::STARTER_CONFIG`] the first time the binary
//!    needs a config and finds none. Mirrors v0's behaviour at
//!    `bitrouter/src/runtime/paths.rs::resolve_home`.
//!
//! On Windows / non-Unix without `$HOME`, the last step degrades to a
//! clear error pointing at `$BITROUTER_HOME`. Tests should always pass
//! `-c <path>` explicitly so they never trigger the scaffold side
//! effect.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The fixed config filename inside any home directory.
const CONFIG_FILENAME: &str = "bitrouter.yaml";

/// Resolve the config file path according to the documented order. Reads
/// the live environment (`current_dir`, `$BITROUTER_HOME`, `$HOME`) and
/// auto-scaffolds `~/.bitrouter/bitrouter.yaml` if step 4 fires.
///
/// For testable / dependency-injected resolution see
/// [`resolve_config_with`].
pub fn resolve_config(explicit: Option<&Path>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().ok();
    let bitrouter_home = std::env::var_os("BITROUTER_HOME").filter(|v| !v.is_empty());
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty());
    let outcome = resolve_config_with(
        explicit,
        cwd.as_deref(),
        bitrouter_home.as_deref().map(Path::new),
        home.as_deref().map(Path::new),
    )?;
    let path = match outcome {
        Resolution::Existing(path) => path,
        Resolution::ScaffoldDefault { home, config_file } => {
            scaffold_default_home(&home, &config_file)
                .with_context(|| format!("scaffolding default home at {}", home.display()))?;
            crate::error_report::info(format_args!(
                "no config found in cwd or $BITROUTER_HOME; scaffolded a starter config at {}",
                config_file.display()
            ));
            config_file
        }
    };
    // Always hand back an absolute path. Downstream code chdirs to the
    // bitrouter home (the config file's parent) so the daemon doesn't
    // depend on the launcher's CWD; a relative `-c ./foo.yaml` would
    // get lost once the chdir happens. Absolutising here without
    // following symlinks keeps the displayed path readable.
    Ok(absolutize(path))
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

/// What `resolve_config_with` decided. Split out from the side-effectful
/// outer entry so tests can drive the pure resolution logic with
/// controlled inputs and assert on the outcome — including the "would
/// scaffold here" case — without ever touching `$HOME`.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    /// A config file that already exists. Use it as-is.
    Existing(PathBuf),
    /// Step 4 fired — caller should write `STARTER_CONFIG` to
    /// `config_file` (creating `home` first).
    ScaffoldDefault {
        /// The `~/.bitrouter` directory.
        home: PathBuf,
        /// The `~/.bitrouter/bitrouter.yaml` path to write.
        config_file: PathBuf,
    },
}

/// Pure resolution logic. Takes the cwd / env-var values that the live
/// version reads from the process, returns the resolution decision
/// without performing any side effects.
pub fn resolve_config_with(
    explicit: Option<&Path>,
    cwd: Option<&Path>,
    bitrouter_home_env: Option<&Path>,
    home_env: Option<&Path>,
) -> Result<Resolution> {
    // 1. explicit -c path — use as-is, surface a clear error if missing.
    if let Some(path) = explicit {
        let p = path.to_path_buf();
        if !p.exists() {
            anyhow::bail!(
                "config file '{}' does not exist (passed via -c). \
                 Drop the flag to fall back to the resolution order \
                 (cwd → $BITROUTER_HOME → ~/.bitrouter).",
                p.display()
            );
        }
        return Ok(Resolution::Existing(p));
    }

    // 2. cwd / bitrouter.yaml
    if let Some(cwd) = cwd {
        let candidate = cwd.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Ok(Resolution::Existing(candidate));
        }
    }

    // 3. $BITROUTER_HOME / bitrouter.yaml. If the env var is set, that
    // directory must contain the file — fail loudly otherwise rather
    // than silently falling through to the scaffold path. An operator
    // who set BITROUTER_HOME intended that directory to win.
    if let Some(env_home) = bitrouter_home_env {
        let candidate = env_home.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Ok(Resolution::Existing(candidate));
        }
        anyhow::bail!(
            "BITROUTER_HOME is set to '{}' but '{}' is missing there. \
             Either drop the env var or create the file.",
            env_home.display(),
            CONFIG_FILENAME
        );
    }

    // 4. ~/.bitrouter / bitrouter.yaml — scaffold the home + config on
    // first run. The actual fs write is done by the caller so this
    // function stays pure.
    let home = home_env.context(
        "could not determine home directory (no $HOME set); set $BITROUTER_HOME or pass -c <path>",
    )?;
    let home = home.join(".bitrouter");
    let candidate = home.join(CONFIG_FILENAME);
    if candidate.is_file() {
        return Ok(Resolution::Existing(candidate));
    }
    Ok(Resolution::ScaffoldDefault {
        home,
        config_file: candidate,
    })
}

/// Create the default home directory and write the starter config.
/// Tightens the home dir to `0o700` on Unix because the operator may
/// drop secrets in `<home>/.env` later. Idempotent: a partial home
/// (directory exists but config is missing) is filled in.
fn scaffold_default_home(home: &Path, config_file: &Path) -> Result<()> {
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
    std::fs::write(config_file, crate::commands::STARTER_CONFIG)
        .with_context(|| format!("writing {}", config_file.display()))?;
    Ok(())
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
    fn explicit_path_is_used_verbatim_when_it_exists() {
        let dir = unique_tmp("explicit-ok");
        let path = dir.join("bitrouter.yaml");
        std::fs::write(&path, "server: {listen: '127.0.0.1:0'}").unwrap();
        let resolved = resolve_config_with(Some(&path), None, None, None).unwrap();
        assert_eq!(resolved, Resolution::Existing(path));
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
        // Even with both env values set, the cwd hit wins because step 2
        // runs before step 3.
        let env_home = unique_tmp("env-distractor");
        let home_env = unique_tmp("home-distractor");
        let resolved =
            resolve_config_with(None, Some(&cwd), Some(&env_home), Some(&home_env)).unwrap();
        assert_eq!(resolved, Resolution::Existing(path));
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
        assert_eq!(resolved, Resolution::Existing(path));
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
    fn falls_back_to_dot_bitrouter_scaffold_when_nothing_else_matches() {
        let home_root = unique_tmp("home-fallback");
        // No cwd, no env — and the ~/.bitrouter/bitrouter.yaml file
        // doesn't exist yet, so resolution decides to scaffold.
        let resolved = resolve_config_with(None, None, None, Some(&home_root)).unwrap();
        match resolved {
            Resolution::ScaffoldDefault { home, config_file } => {
                assert_eq!(home, home_root.join(".bitrouter"));
                assert_eq!(
                    config_file,
                    home_root.join(".bitrouter").join("bitrouter.yaml")
                );
            }
            other => panic!("expected ScaffoldDefault, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&home_root);
    }

    #[test]
    fn dot_bitrouter_existing_config_is_used_without_scaffolding() {
        let home_root = unique_tmp("home-existing");
        let dot = home_root.join(".bitrouter");
        std::fs::create_dir_all(&dot).unwrap();
        let path = dot.join("bitrouter.yaml");
        std::fs::write(&path, "server: {listen: '127.0.0.1:0'}").unwrap();
        let resolved = resolve_config_with(None, None, None, Some(&home_root)).unwrap();
        assert_eq!(resolved, Resolution::Existing(path));
        let _ = std::fs::remove_dir_all(&home_root);
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
