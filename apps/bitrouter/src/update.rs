//! `bitrouter update` — in-place self-updater built on cargo-dist's
//! `axoupdater`. Decision logic (install-method detection, release channel,
//! nudge-cache TTL) lives in small pure functions so it can be unit-tested
//! without touching the network or replacing the running binary.

use crate::{daemon, style};
use anyhow::Result;
use axoupdater::{AxoUpdater, UpdateRequest};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How the running `bitrouter` binary was installed, used to pick the right
/// upgrade path when there is no cargo-dist receipt to self-update from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallMethod {
    Homebrew,
    Cargo,
    Unknown,
}

/// Best-effort classification from the executable path and the resolved Cargo
/// home. Homebrew installs live under a `Cellar/bitrouter/...` path; `cargo
/// install` places the binary in `<CARGO_HOME>/bin`.
fn detect_install_method(exe: &Path, cargo_home: Option<&Path>) -> InstallMethod {
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
fn delegation_command(method: InstallMethod) -> String {
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
/// decision is unit-testable. Converted to an `axoupdater::UpdateRequest` by
/// `to_request`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionSpec {
    /// Newest stable (non-prerelease) release.
    Latest,
    /// Newest release including prereleases — the default while pre-1.0.
    LatestPrerelease,
    /// A specific pinned tag (enables downgrade/rollback).
    Tag(String),
}

/// `--tag` always wins; otherwise `--stable` selects stable-only, and the
/// default follows prereleases (the project currently ships only `alpha.*`).
fn choose_spec(tag: Option<&str>, stable: bool) -> VersionSpec {
    match tag {
        Some(t) => VersionSpec::Tag(t.to_string()),
        None if stable => VersionSpec::Latest,
        None => VersionSpec::LatestPrerelease,
    }
}

/// Options parsed from the `bitrouter update` flags.
#[derive(Debug)]
pub struct UpdateOptions {
    pub check: bool,
    pub tag: Option<String>,
    pub stable: bool,
    pub restart: bool,
    pub yes: bool,
}

/// What the dispatch layer must still do after `run` returns.
#[derive(Debug)]
pub struct RunOutcome {
    /// A running daemon needs restarting to pick up the new binary.
    pub restart_needed: bool,
}

/// Current binary version, baked in at compile time.
fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Resolve `$CARGO_HOME`, falling back to `~/.cargo`, for install detection.
fn cargo_home() -> Option<std::path::PathBuf> {
    if let Some(h) = std::env::var_os("CARGO_HOME") {
        return Some(std::path::PathBuf::from(h));
    }
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cargo"))
}

/// Translate our channel decision into axoupdater's request type.
fn to_request(spec: VersionSpec) -> UpdateRequest {
    match spec {
        VersionSpec::Latest => UpdateRequest::Latest,
        VersionSpec::LatestPrerelease => UpdateRequest::LatestMaybePrerelease,
        VersionSpec::Tag(t) => UpdateRequest::SpecificTag(t),
    }
}

/// Configure an axoupdater for the `bitrouter` app. Prefers the dist install
/// receipt; if absent, returns `None` so the caller can delegate.
fn build_updater() -> Option<AxoUpdater> {
    let mut updater = AxoUpdater::new_for("bitrouter");
    if let Err(e) = updater.load_receipt() {
        tracing::debug!(error = %e, "no install receipt; delegating to package manager");
        return None;
    }
    Some(updater)
}

pub async fn run(opts: UpdateOptions, socket: &Path) -> Result<RunOutcome> {
    let p = style::Palette::for_stdout();
    let current = current_version();

    // 1. No receipt -> package-manager install; delegate, never clobber.
    let Some(mut updater) = build_updater() else {
        let exe = std::env::current_exe().unwrap_or_default();
        let method = detect_install_method(&exe, cargo_home().as_deref());
        println!(
            "{dim}bitrouter looks installed via {how}. Update with:{reset}\n    {cmd}",
            dim = p.dim,
            reset = p.reset,
            how = match method {
                InstallMethod::Homebrew => "Homebrew",
                InstallMethod::Cargo => "Cargo",
                InstallMethod::Unknown => "your package manager",
            },
            cmd = delegation_command(method),
        );
        return Ok(RunOutcome {
            restart_needed: false,
        });
    };

    // 2. Channel / pin.
    let spec = choose_spec(opts.tag.as_deref(), opts.stable);
    let target_label = match &spec {
        VersionSpec::Tag(t) => format!("version {t}"),
        _ => "the latest release".to_string(),
    };
    if let VersionSpec::Tag(_) = spec {
        updater.always_update(true);
    }
    updater.configure_version_specifier(to_request(spec));

    // 3. Dry run.
    if opts.check {
        if updater.is_update_needed().await? {
            match updater.query_new_version().await? {
                Some(v) => println!("update available: {current} -> {v}"),
                None => println!("update available (target version unknown)"),
            }
        } else {
            println!("up to date ({current})");
        }
        return Ok(RunOutcome {
            restart_needed: false,
        });
    }

    // 4. Confirm + swap.
    if !opts.yes && !confirm(&p, current, &target_label)? {
        println!("aborted");
        return Ok(RunOutcome {
            restart_needed: false,
        });
    }
    let Some(result) = updater.run().await? else {
        println!("already up to date ({current})");
        return Ok(RunOutcome {
            restart_needed: false,
        });
    };
    println!(
        "{green}✓{reset} updated {current} -> {new}",
        green = p.green,
        reset = p.reset,
        new = result.new_version,
    );

    // 5. Daemon awareness.
    let daemon_running = daemon::endpoint_in_use(socket);
    if daemon_running && !opts.restart {
        println!(
            "{dim}A daemon is running the old binary. Run `bitrouter restart` to serve {new}.{reset}",
            dim = p.dim,
            reset = p.reset,
            new = result.new_version,
        );
    }
    Ok(RunOutcome {
        restart_needed: daemon_running && opts.restart,
    })
}

/// Interactive y/N confirmation. Defaults to no on empty input or non-tty EOF.
fn confirm(p: &style::Palette, current: &str, target: &str) -> Result<bool> {
    use std::io::Write;
    print!(
        "Update bitrouter from {bold}{current}{reset} to {target}? [y/N] ",
        bold = p.bold,
        reset = p.reset,
    );
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(false);
    }
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Filename of the persisted update-check cache inside the bitrouter home.
const CACHE_FILENAME: &str = "update-check.json";

/// Persisted record of the last passive update check.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateCache {
    /// Unix seconds of the last network check.
    checked_at: i64,
    /// Latest version string seen at that check, if any.
    latest: Option<String>,
}

/// Whether the passive nudge should perform a fresh network check now. True
/// when never checked, or when more than `ttl_secs` have elapsed.
fn should_check(now: i64, last_checked: Option<i64>, ttl_secs: i64) -> bool {
    match last_checked {
        None => true,
        Some(prev) => now - prev >= ttl_secs,
    }
}

/// Read the cache from `<home>/update-check.json`. Any error (missing,
/// malformed) is treated as "no cache" — the nudge is best-effort.
fn read_cache(home: &Path) -> Option<UpdateCache> {
    let bytes = std::fs::read(home.join(CACHE_FILENAME)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist the cache to `<home>/update-check.json`, creating the home if needed.
fn write_cache(home: &Path, cache: &UpdateCache) -> Result<()> {
    crate::paths::ensure_home_directory(home)?;
    let bytes = serde_json::to_vec(cache)?;
    std::fs::write(home.join(CACHE_FILENAME), bytes)?;
    Ok(())
}

/// Env var that disables the passive update nudge entirely.
const NUDGE_DISABLE_ENV: &str = "BITROUTER_NO_UPDATE_CHECK";
/// Nudge network check cadence: once per day.
const NUDGE_TTL_SECS: i64 = 24 * 60 * 60;

/// Best-effort "update available" line for `bitrouter status`. Never returns an
/// error and never blocks meaningfully: it respects the opt-out env var, checks
/// the network at most once per `NUDGE_TTL_SECS` (cached under `home`), and
/// swallows every failure.
pub async fn maybe_nudge(home: &Path, p: &style::Palette) {
    if std::env::var_os(NUDGE_DISABLE_ENV).is_some() {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let cache = read_cache(home);
    let last = cache.as_ref().map(|c| c.checked_at);

    let latest: Option<String> = if should_check(now, last, NUDGE_TTL_SECS) {
        match query_latest().await {
            Some(v) => {
                let _ = write_cache(
                    home,
                    &UpdateCache {
                        checked_at: now,
                        latest: Some(v.clone()),
                    },
                );
                Some(v)
            }
            None => {
                // Record the attempt so we don't hammer the network on failure.
                let _ = write_cache(
                    home,
                    &UpdateCache {
                        checked_at: now,
                        latest: None,
                    },
                );
                cache.and_then(|c| c.latest)
            }
        }
    } else {
        cache.and_then(|c| c.latest)
    };

    if let Some(latest) = latest.filter(|l| is_newer(l, current_version())) {
        // Diagnostic, not the command result — goes to stderr so it never
        // pollutes a command's JSON stdout (e.g. `bitrouter status`).
        eprintln!();
        eprintln!(
            "  {dim}↑ {latest} available — run `bitrouter update`{reset}",
            dim = p.dim,
            reset = p.reset,
        );
    }
}

/// Query the newest version tag, prereleases included, with a short timeout.
/// Returns `None` on any error or timeout — the nudge is strictly best-effort.
async fn query_latest() -> Option<String> {
    let mut updater = build_updater()?;
    updater.configure_version_specifier(UpdateRequest::LatestMaybePrerelease);
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        updater.query_new_version(),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
    .flatten()
    .map(|v| v.to_string())
}

/// True when `candidate` is a strictly greater semver than `current`. Parse
/// failures yield `false` so a malformed tag never nags the user.
fn is_newer(candidate: &str, current: &str) -> bool {
    match (
        semver::Version::parse(candidate),
        semver::Version::parse(current),
    ) {
        (Ok(c), Ok(cur)) => c > cur,
        _ => false,
    }
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
    fn is_newer_compares_prerelease_semver() {
        assert!(is_newer("1.0.0-alpha.20", "1.0.0-alpha.19"));
        assert!(!is_newer("1.0.0-alpha.19", "1.0.0-alpha.19"));
        assert!(!is_newer("1.0.0-alpha.18", "1.0.0-alpha.19"));
        assert!(!is_newer("garbage", "1.0.0-alpha.19"));
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
