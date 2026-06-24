//! `bitrouter update` — in-place self-updater built on cargo-dist's
//! `axoupdater`. Decision logic (install-method detection, release channel,
//! nudge-cache TTL) lives in small pure functions so it can be unit-tested
//! without touching the network or replacing the running binary.

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
}
