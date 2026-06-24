//! `bitrouter update` — in-place self-updater built on cargo-dist's
//! `axoupdater`. Decision logic (install-method detection, release channel,
//! nudge-cache TTL) lives in small pure functions so it can be unit-tested
//! without touching the network or replacing the running binary.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How the running `bitrouter` binary was installed, used to pick the right
/// upgrade path when there is no cargo-dist receipt to self-update from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    Homebrew,
    Cargo,
    Unknown,
}

/// Best-effort classification from the executable path and the resolved Cargo
/// home. Homebrew installs live under a `Cellar/bitrouter/...` path; `cargo
/// install` places the binary in `<CARGO_HOME>/bin`.
pub fn detect_install_method(exe: &Path, cargo_home: Option<&Path>) -> InstallMethod {
    let exe_str = exe.to_string_lossy();
    if exe_str.contains("/Cellar/bitrouter/") || exe_str.contains("/homebrew/") {
        return InstallMethod::Homebrew;
    }
    if cargo_home.is_some_and(|home| exe.starts_with(home.join("bin"))) {
        return InstallMethod::Cargo;
    }
    InstallMethod::Unknown
}

/// The exact command a user should run to upgrade a package-manager-managed
/// install. Unknown installs get the generic re-run-the-installer hint.
pub fn delegation_command(method: InstallMethod) -> String {
    match method {
        InstallMethod::Homebrew => "brew upgrade bitrouter".to_string(),
        InstallMethod::Cargo => "cargo install bitrouter --force".to_string(),
        InstallMethod::Unknown => {
            "curl --proto '=https' --tlsv1.2 -LsSf \
             https://github.com/bitrouter/bitrouter/releases/latest/download/bitrouter-installer.sh | sh"
                .to_string()
        }
    }
}

/// Resolved release target, decoupled from axoupdater's own request type so the
/// decision is unit-testable. Converted to an `axoupdater::UpdateRequest` at the
/// call site (a later task).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionSpec {
    /// Newest stable (non-prerelease) release.
    Latest,
    /// Newest release including prereleases — the default while pre-1.0.
    LatestPrerelease,
    /// A specific pinned tag (enables downgrade/rollback).
    Tag(String),
}

/// `--tag` always wins; otherwise `--stable` selects stable-only, and the
/// default follows prereleases (the project currently ships only `alpha.*`).
pub fn choose_spec(tag: Option<&str>, stable: bool) -> VersionSpec {
    match tag {
        Some(t) => VersionSpec::Tag(t.to_string()),
        None if stable => VersionSpec::Latest,
        None => VersionSpec::LatestPrerelease,
    }
}

/// Filename of the persisted update-check cache inside the bitrouter home.
const CACHE_FILENAME: &str = "update-check.json";

/// Persisted record of the last passive update check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCache {
    /// Unix seconds of the last network check.
    pub checked_at: i64,
    /// Latest version string seen at that check, if any.
    pub latest: Option<String>,
}

/// Whether the passive nudge should perform a fresh network check now. True
/// when never checked, or when more than `ttl_secs` have elapsed.
pub fn should_check(now: i64, last_checked: Option<i64>, ttl_secs: i64) -> bool {
    match last_checked {
        None => true,
        Some(prev) => now - prev >= ttl_secs,
    }
}

/// Read the cache from `<home>/update-check.json`. Any error (missing,
/// malformed) is treated as "no cache" — the nudge is best-effort.
pub fn read_cache(home: &Path) -> Option<UpdateCache> {
    let bytes = std::fs::read(home.join(CACHE_FILENAME)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist the cache to `<home>/update-check.json`, creating the home if needed.
pub fn write_cache(home: &Path, cache: &UpdateCache) -> Result<()> {
    crate::paths::ensure_home_directory(home)?;
    let bytes = serde_json::to_vec(cache)?;
    std::fs::write(home.join(CACHE_FILENAME), bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detects_homebrew_from_cellar_path() {
        let exe = Path::new("/opt/homebrew/Cellar/bitrouter/1.0.0/bin/bitrouter");
        assert_eq!(detect_install_method(exe, None), InstallMethod::Homebrew);
    }

    #[test]
    fn detects_cargo_from_cargo_home_bin() {
        let exe = Path::new("/home/me/.cargo/bin/bitrouter");
        let cargo_home = Path::new("/home/me/.cargo");
        assert_eq!(
            detect_install_method(exe, Some(cargo_home)),
            InstallMethod::Cargo
        );
    }

    #[test]
    fn unknown_when_no_signal_matches() {
        let exe = Path::new("/usr/local/bin/bitrouter");
        assert_eq!(detect_install_method(exe, None), InstallMethod::Unknown);
    }

    #[test]
    fn delegation_command_is_method_specific() {
        assert_eq!(
            delegation_command(InstallMethod::Homebrew),
            "brew upgrade bitrouter"
        );
        assert_eq!(
            delegation_command(InstallMethod::Cargo),
            "cargo install bitrouter --force"
        );
        assert!(delegation_command(InstallMethod::Unknown).contains("bitrouter-installer.sh"));
    }

    #[test]
    fn tag_takes_precedence_over_channel() {
        assert_eq!(
            choose_spec(Some("1.0.0-alpha.18"), true),
            VersionSpec::Tag("1.0.0-alpha.18".to_string())
        );
    }

    #[test]
    fn default_channel_includes_prereleases() {
        assert_eq!(choose_spec(None, false), VersionSpec::LatestPrerelease);
    }

    #[test]
    fn stable_flag_excludes_prereleases() {
        assert_eq!(choose_spec(None, true), VersionSpec::Latest);
    }

    const DAY: i64 = 24 * 60 * 60;

    #[test]
    fn checks_when_never_checked_before() {
        assert!(should_check(1_000_000, None, DAY));
    }

    #[test]
    fn skips_when_within_ttl() {
        assert!(!should_check(1_000_000 + 100, Some(1_000_000), DAY));
    }

    #[test]
    fn checks_again_after_ttl_elapsed() {
        assert!(should_check(1_000_000 + DAY + 1, Some(1_000_000), DAY));
    }

    #[test]
    fn cache_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("brupd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = UpdateCache {
            checked_at: 42,
            latest: Some("1.0.0-alpha.20".to_string()),
        };
        write_cache(&dir, &cache).unwrap();
        let read = read_cache(&dir).expect("cache present");
        assert_eq!(read.checked_at, 42);
        assert_eq!(read.latest.as_deref(), Some("1.0.0-alpha.20"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
