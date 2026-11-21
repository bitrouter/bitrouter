//! Service release updates, package-manager delegation and passive checks.

use crate::commands::invocation;
use crate::output::reports::update::UpdateReport;
use crate::{daemon, style};
use anyhow::Result;
use bitrouter_ai::artifact::{ArtifactIdentity, DistributionProfile};
use bitrouter_update::{Product, channel, install::Installation};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandShell {
    Posix,
    PowerShell,
}

impl CommandShell {
    fn native() -> Self {
        if cfg!(windows) {
            Self::PowerShell
        } else {
            Self::Posix
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Posix => "a POSIX-compatible shell",
            Self::PowerShell => "PowerShell 7.3 or newer",
        }
    }

    fn quote(self, argument: &str) -> String {
        match self {
            Self::Posix => shell_words::quote(argument).into_owned(),
            Self::PowerShell => {
                // PowerShell literal strings double single-quote delimiters,
                // including the four typographic variants its tokenizer accepts.
                // https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_quoting_rules
                // https://github.com/PowerShell/PowerShell/blob/v7.5.0/src/System.Management.Automation/engine/parser/CharTraits.cs#L251-L260
                let mut quoted = String::from("'");
                for character in argument.chars() {
                    if matches!(character, '\'' | '\u{2018}'..='\u{201b}') {
                        quoted.push(character);
                    }
                    quoted.push(character);
                }
                quoted.push('\'');
                quoted
            }
        }
    }
}

/// Only the full profile currently has a matching release installer/receipt.
/// Archive availability alone does not establish a safe self-update channel.
fn has_release_installer(artifact: &ArtifactIdentity) -> bool {
    artifact.profile == DistributionProfile::Full
}

fn source_reason(artifact: &ArtifactIdentity) -> String {
    format!(
        "No matching release installer/receipt is available for the {} profile. Build from source with the exact installed features. Without --tag, Cargo's registry version selection applies. Run the printed command in {}.",
        artifact.profile.as_str(),
        CommandShell::native().label()
    )
}

/// A source update must explicitly disable defaults even for an empty feature
/// set. Cargo's version and feature selection contracts:
/// https://doc.rust-lang.org/cargo/commands/cargo-install.html
fn source_install_command(
    artifact: &ArtifactIdentity,
    spec: &VersionSpec,
    shell: CommandShell,
) -> String {
    let mut command =
        "cargo install bitrouter --bin bitrouter --force --no-default-features".to_string();
    if !artifact.features.is_empty() {
        command.push_str(" --features ");
        command.push_str(&shell.quote(&artifact.features.join(",")));
    }
    if let VersionSpec::Tag(tag) = spec {
        let version = tag.strip_prefix('v').unwrap_or(tag);
        if semver::Version::parse(version).is_ok() {
            command.push_str(" --version ");
            command.push_str(&shell.quote(&format!("={version}")));
        } else {
            // A Git tag need not be a crates.io version. Keep it literal and
            // use --tag= so a leading hyphen cannot become another option.
            command.push_str(" --git https://github.com/bitrouter/bitrouter ");
            command.push_str(&shell.quote(&format!("--tag={tag}")));
        }
    }
    match shell {
        CommandShell::Posix => command,
        // PowerShell 7.3+ Standard mode preserves embedded quotes when passing
        // literal strings to cargo.exe. Use a child scope to leave the caller's
        // preference unchanged, even if it was set to Legacy.
        // https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_parsing#passing-arguments-that-contain-quote-characters
        CommandShell::PowerShell => {
            format!("& {{ $PSNativeCommandArgumentPassing = 'Standard'; {command} }}")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionSpec {
    /// Newest stable (non-prerelease) release.
    Latest,
    /// Newest release including prereleases.
    LatestPrerelease,
    /// A specific pinned tag (enables downgrade/rollback).
    Tag(String),
}

/// `--tag` always wins; otherwise `--stable` selects stable-only, and the
/// default follows prereleases.
fn choose_spec(tag: Option<&str>, stable: bool) -> VersionSpec {
    match tag {
        Some(t) => VersionSpec::Tag(t.to_string()),
        None if stable => VersionSpec::Latest,
        None => VersionSpec::LatestPrerelease,
    }
}

fn same_capabilities(candidate: &ArtifactIdentity, resident: &ArtifactIdentity) -> bool {
    candidate.profile == resident.profile
        && candidate.features == resident.features
        && candidate.target == resident.target
}

/// Options parsed from the `bitrouter update` flags.
#[derive(Debug)]
pub struct UpdateOptions {
    pub check: bool,
    pub tag: Option<String>,
    pub stable: bool,
    pub yes: bool,
}

impl UpdateReport {
    /// Record replacement identity after the installer has completed.
    /// A verification failure is reported with the completed installation result.
    pub fn record_target_verification(
        &mut self,
        result: std::result::Result<ArtifactIdentity, String>,
    ) {
        let verified = result.and_then(|target| {
            match self.target_version.as_deref() {
                Some(expected) if target.version == expected => {}
                Some(expected) => {
                    return Err(format!(
                        "installed artifact version {} does not match installer target {expected}",
                        target.version
                    ));
                }
                None => return Err("the installer target version is unknown".to_string()),
            }
            let Some(current) = self.current_artifact.as_ref() else {
                return Err("the invoking artifact identity is unknown".to_string());
            };
            if !same_capabilities(current, &target) {
                return Err(
                    "installed artifact capabilities (profile, features or target) do not match the invoking executable"
                        .to_string(),
                );
            }
            if self.target_artifact.as_ref().is_some_and(|staged| staged != &target) {
                return Err("installed executable differs from the verified staged artifact".to_string());
            }
            Ok(target)
        });
        match verified {
            Ok(target) => {
                self.target_artifact = Some(target);
                self.target_verification_error = None;
            }
            Err(error) => {
                self.target_artifact = None;
                self.target_verification_error = Some(error);
            }
        }
    }
}

/// Replace the service executable and report whether the daemon needs a restart.
///
/// # Errors
/// Refuses a mismatched installation receipt, release plan or staged executable.
pub async fn run(opts: UpdateOptions, socket: &Path) -> Result<UpdateReport> {
    let artifact = crate::artifact::current();
    let current = artifact.version.clone();
    let exe = std::env::current_exe()?.canonicalize()?;
    let method = bitrouter_update::install_method(&exe, bitrouter_update::cargo_home().as_deref());
    if !has_release_installer(&artifact) {
        let spec = choose_spec(opts.tag.as_deref(), opts.stable);
        let command = source_install_command(&artifact, &spec, CommandShell::native());
        let mut report = UpdateReport::delegated(current, method, command);
        report.manual_reason = Some(source_reason(&artifact));
        return Ok(report.with_current_artifact(artifact));
    }
    let install = match method {
        "homebrew" | "npm" => None,
        _ => Installation::load(Product::Service, &exe, &current)?,
    };
    let Some(install) = install else {
        let command = bitrouter_update::manual_command(Product::Service, method);
        let mut report = UpdateReport::delegated(current, method, command);
        report.manual_reason =
            Some("This executable has no matching self-managed installation.".into());
        return Ok(report.with_current_artifact(artifact));
    };
    let release = channel::resolve(
        Product::Service,
        &artifact.target,
        opts.tag.as_deref(),
        opts.stable,
    )
    .await?;
    let available = opts.tag.is_some() || release.version.cmp_precedence(&current.parse()?).is_gt();
    let new_version = release.version.to_string();
    if opts.check {
        return Ok(
            UpdateReport::checked(current, available, available.then_some(new_version))
                .with_current_artifact(artifact),
        );
    }
    if !available {
        install.finish_interrupted()?;
        return Ok(UpdateReport::unchanged(current).with_current_artifact(artifact));
    }
    if !opts.yes && !confirm(&current, &new_version)? {
        return Ok(UpdateReport::aborted(current).with_current_artifact(artifact));
    }
    let candidate = release.stage().await?;
    let identity: ArtifactIdentity = serde_json::from_value(candidate.identity.clone())?;
    anyhow::ensure!(
        same_capabilities(&artifact, &identity),
        "staged service capabilities mismatch"
    );
    install.apply(candidate)?;

    let daemon_running = daemon::endpoint_in_use(socket);
    let daemon = daemon_running.then_some("restart_needed");
    let mut report =
        UpdateReport::updated(current, new_version, daemon).with_current_artifact(artifact);
    report.target_artifact = Some(identity);
    Ok(report)
}

/// Interactive y/N confirmation. Defaults to no on empty input or non-tty EOF.
/// The prompt is written to stderr so stdout stays a clean JSON result.
fn confirm(current: &str, target: &str) -> Result<bool> {
    use std::io::Write;
    let p = style::Palette::for_stderr();
    eprint!(
        "Update BitRouter from {bold}{current}{reset} to {target}? [y/N] ",
        bold = p.bold,
        reset = p.reset,
    );
    std::io::stderr().flush()?;
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
    /// Identity of the installation whose release channel was checked. A
    /// legacy record without this field has unknown profile provenance.
    #[serde(default)]
    artifact: Option<ArtifactIdentity>,
}

fn cache_matches(cache: &UpdateCache, artifact: &ArtifactIdentity) -> bool {
    has_release_installer(artifact)
        && cache.artifact.as_ref().is_some_and(|cached| {
            cached.profile == artifact.profile
                && cached.target == artifact.target
                && cached.features == artifact.features
        })
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
    let artifact = crate::artifact::current();
    if !has_release_installer(&artifact) {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let cache = read_cache(home).filter(|cache| cache_matches(cache, &artifact));
    let last = cache.as_ref().map(|c| c.checked_at);

    let latest: Option<String> = if should_check(now, last, NUDGE_TTL_SECS) {
        match query_latest(&artifact).await {
            Some(v) => {
                let _ = write_cache(
                    home,
                    &UpdateCache {
                        checked_at: now,
                        latest: Some(v.clone()),
                        artifact: Some(artifact.clone()),
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
                        artifact: Some(artifact.clone()),
                    },
                );
                cache.and_then(|c| c.latest)
            }
        }
    } else {
        cache.and_then(|c| c.latest)
    };

    if let Some(latest) = latest.filter(|l| is_newer(l, &artifact.version)) {
        // Diagnostic, not the command result — goes to stderr so it never
        // pollutes a command's JSON stdout (e.g. `bitrouter status`).
        eprintln!();
        eprintln!(
            "  {dim}↑ {latest} available — run `{cli} update`{reset}",
            dim = p.dim,
            reset = p.reset,
            cli = invocation::name(),
        );
    }
}

/// Query the newest version tag, prereleases included, with a short timeout.
/// Returns `None` on any error or timeout — the nudge is strictly best-effort.
async fn query_latest(artifact: &ArtifactIdentity) -> Option<String> {
    if !has_release_installer(artifact) {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let method = bitrouter_update::install_method(&exe, bitrouter_update::cargo_home().as_deref());
    if matches!(method, "npm" | "homebrew") {
        return None;
    }
    Installation::load(Product::Service, &exe, &artifact.version).ok()??;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        channel::resolve(Product::Service, &artifact.target, None, false),
    )
    .await
    .ok()?
    .ok()
    .map(|release| release.version.to_string())
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
    use crate::output::CliReport;

    fn artifact(profile: DistributionProfile, features: &[&str]) -> ArtifactIdentity {
        ArtifactIdentity {
            version: "1.0.0-alpha.19".into(),
            source_revision: Some("test-revision".into()),
            target: "x86_64-unknown-linux-gnu".into(),
            features: features
                .iter()
                .map(|feature| (*feature).to_string())
                .collect(),
            profile,
        }
    }

    fn full_artifact() -> ArtifactIdentity {
        artifact(
            DistributionProfile::Full,
            &[
                "core",
                "db-mysql",
                "db-postgres",
                "harness",
                "mcp",
                "otel-http",
                "server-tools",
                "tui",
            ],
        )
    }

    fn completed_update(
        current: &ArtifactIdentity,
        target_version: &str,
        daemon_running: bool,
    ) -> UpdateReport {
        UpdateReport::updated(
            current.version.clone(),
            target_version.to_string(),
            daemon_running.then_some("restart_needed"),
        )
        .with_current_artifact(current.clone())
    }

    #[test]
    fn verified_replacement_reports_restart_and_distinct_identities() {
        let current = full_artifact();
        let mut target = current.clone();
        target.version = "1.0.0-alpha.20".into();
        target.source_revision = Some("new-revision".into());
        for daemon_running in [false, true] {
            let mut outcome = completed_update(&current, &target.version, daemon_running);
            outcome.record_target_verification(Ok(target.clone()));
            assert_eq!(outcome.status, "updated");
            assert_eq!(outcome.current_artifact.as_ref(), Some(&current));
            assert_eq!(outcome.target_artifact.as_ref(), Some(&target));
            assert!(outcome.target_verification_error.is_none());
            assert_eq!(outcome.daemon, daemon_running.then_some("restart_needed"));
            assert_eq!(outcome.exit_code(), 0);
        }
    }

    #[test]
    fn unverified_target_reports_the_installation_and_restart_requirement() {
        let current = full_artifact();
        for error in [
            "legacy release does not support build-info",
            "decoding replacement build-info: invalid JSON",
            "replacement build-info timed out",
        ] {
            for daemon_running in [false, true] {
                let mut outcome = completed_update(&current, "1.0.0-alpha.18", daemon_running);
                outcome.record_target_verification(Err(error.to_string()));
                assert_eq!(outcome.status, "updated");
                assert_eq!(outcome.target_version.as_deref(), Some("1.0.0-alpha.18"));
                assert_eq!(outcome.current_artifact.as_ref(), Some(&current));
                assert!(outcome.target_artifact.is_none());
                assert_eq!(outcome.target_verification_error.as_deref(), Some(error));
                assert_eq!(outcome.daemon, daemon_running.then_some("restart_needed"));
                assert_eq!(outcome.exit_code(), 1);
            }
        }
    }

    #[test]
    fn mismatched_replacement_does_not_claim_verified_identity() {
        let current = full_artifact();
        let mut target = current.clone();
        target.version = "1.0.0-alpha.20".into();
        let mut wrong_version = target.clone();
        wrong_version.version = "1.0.0-alpha.21".into();
        let mut wrong_profile = target.clone();
        wrong_profile.profile = DistributionProfile::Custom;
        let mut wrong_features = target.clone();
        wrong_features.features.retain(|feature| feature != "tui");
        let mut wrong_platform = target.clone();
        wrong_platform.target = "aarch64-apple-darwin".into();
        for (candidate, expected_error) in [
            (wrong_version, "version"),
            (wrong_profile, "capabilities"),
            (wrong_features, "capabilities"),
            (wrong_platform, "capabilities"),
        ] {
            let mut outcome = completed_update(&current, &target.version, true);
            outcome.record_target_verification(Ok(candidate));
            assert_eq!(outcome.status, "updated");
            assert_eq!(outcome.target_version.as_ref(), Some(&target.version));
            assert_eq!(outcome.current_artifact.as_ref(), Some(&current));
            assert!(outcome.target_artifact.is_none());
            assert!(
                outcome
                    .target_verification_error
                    .as_deref()
                    .is_some_and(|error| error.contains(expected_error))
            );
            assert_eq!(outcome.daemon, Some("restart_needed"));
            assert_eq!(outcome.exit_code(), 1);
        }
    }

    #[test]
    fn installed_revision_mismatch_fails_verification() {
        let current = full_artifact();
        let mut outcome = completed_update(&current, &current.version, true);
        outcome.target_artifact = Some(current.clone());
        let mut different = current;
        different.source_revision = Some("concurrent-release".into());
        outcome.record_target_verification(Ok(different));
        assert_eq!(outcome.daemon, Some("restart_needed"));
        assert!(outcome.target_artifact.is_none());
    }

    #[test]
    fn manual_source_tag_is_exact_and_shell_quoted() -> Result<()> {
        let custom = artifact(DistributionProfile::Custom, &["core", "mcp"]);
        for tag in [
            "--version=9.9.9",
            "release'$(touch /tmp/not-executed); candidate",
        ] {
            let command =
                source_install_command(&custom, &VersionSpec::Tag(tag.into()), CommandShell::Posix);
            let args = shell_words::split(&command)?;
            assert_eq!(
                args,
                vec![
                    "cargo".to_string(),
                    "install".to_string(),
                    "bitrouter".to_string(),
                    "--bin".to_string(),
                    "bitrouter".to_string(),
                    "--force".to_string(),
                    "--no-default-features".to_string(),
                    "--features".to_string(),
                    "core,mcp".to_string(),
                    "--git".to_string(),
                    "https://github.com/bitrouter/bitrouter".to_string(),
                    format!("--tag={tag}"),
                ]
            );
            assert!(command.ends_with(shell_words::quote(&format!("--tag={tag}")).as_ref()));
        }
        Ok(())
    }

    #[test]
    fn powershell_source_commands_preserve_profiles_and_exact_versions() {
        let gateway = artifact(DistributionProfile::Gateway, &[]);
        assert_eq!(
            source_install_command(&gateway, &VersionSpec::Latest, CommandShell::PowerShell),
            "& { $PSNativeCommandArgumentPassing = 'Standard'; cargo install bitrouter --bin bitrouter --force --no-default-features }"
        );
        let custom = artifact(DistributionProfile::Custom, &["core", "mcp"]);
        assert_eq!(
            source_install_command(
                &custom,
                &VersionSpec::Tag("v1.0.0-alpha.18".into()),
                CommandShell::PowerShell,
            ),
            "& { $PSNativeCommandArgumentPassing = 'Standard'; cargo install bitrouter --bin bitrouter --force --no-default-features --features 'core,mcp' --version '=1.0.0-alpha.18' }"
        );
    }

    #[test]
    fn powershell_source_tags_quote_metacharacters_and_smart_apostrophes() {
        let gateway = artifact(DistributionProfile::Gateway, &[]);
        for (tag, quoted_argument) in [
            ("--version=9.9.9", "'--tag=--version=9.9.9'"),
            ("", "'--tag='"),
            (
                "release'$(Get-Date);`&|\"quoted\"",
                "'--tag=release''$(Get-Date);`&|\"quoted\"'",
            ),
            (
                "release‘left’right‚base‛reversed",
                "'--tag=release‘‘left’’right‚‚base‛‛reversed'",
            ),
        ] {
            assert_eq!(
                source_install_command(
                    &gateway,
                    &VersionSpec::Tag(tag.into()),
                    CommandShell::PowerShell,
                ),
                format!(
                    "& {{ $PSNativeCommandArgumentPassing = 'Standard'; cargo install bitrouter --bin bitrouter --force --no-default-features --git https://github.com/bitrouter/bitrouter {quoted_argument} }}"
                )
            );
        }
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
    fn cache_round_trips_through_disk() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let full = full_artifact();
        let cache = UpdateCache {
            checked_at: 42,
            latest: Some("1.0.0-alpha.20".to_string()),
            artifact: Some(full.clone()),
        };
        write_cache(dir.path(), &cache)?;
        let read = read_cache(dir.path()).ok_or_else(|| anyhow::anyhow!("cache missing"))?;
        assert_eq!(read.checked_at, 42);
        assert_eq!(read.latest.as_deref(), Some("1.0.0-alpha.20"));
        assert!(cache_matches(&read, &full));
        assert_eq!(read.artifact, Some(full));
        Ok(())
    }

    #[test]
    fn legacy_cache_has_unknown_profile_and_cannot_supply_a_nudge() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join(CACHE_FILENAME),
            br#"{"checked_at":42,"latest":"99.0.0"}"#,
        )?;
        let legacy = read_cache(dir.path()).ok_or_else(|| anyhow::anyhow!("cache missing"))?;
        assert!(legacy.artifact.is_none());
        for identity in [
            full_artifact(),
            artifact(DistributionProfile::Gateway, &[]),
            artifact(
                DistributionProfile::GatewayShared,
                &["db-postgres", "multi-instance"],
            ),
            artifact(DistributionProfile::Custom, &["core"]),
        ] {
            assert!(!cache_matches(&legacy, &identity));
        }
        Ok(())
    }

    #[test]
    fn cache_requires_matching_supported_profile_target_and_features() {
        let full = full_artifact();
        let cache = UpdateCache {
            checked_at: 42,
            latest: Some("99.0.0".into()),
            artifact: Some(full.clone()),
        };
        assert!(cache_matches(&cache, &full));

        // Reusing the release check across versions of the same distribution
        // is fine; target and effective capabilities must still match exactly.
        let mut next = full.clone();
        next.version = "1.0.0-alpha.20".into();
        next.source_revision = None;
        assert!(cache_matches(&cache, &next));
        next.target = "aarch64-apple-darwin".into();
        assert!(!cache_matches(&cache, &next));
        next = full.clone();
        next.features.retain(|feature| feature != "tui");
        assert!(!cache_matches(&cache, &next));

        for reduced in [
            artifact(DistributionProfile::Gateway, &[]),
            artifact(
                DistributionProfile::GatewayShared,
                &["db-postgres", "multi-instance"],
            ),
            artifact(DistributionProfile::Custom, &["core"]),
        ] {
            assert!(!cache_matches(&cache, &reduced));
            let reduced_cache = UpdateCache {
                artifact: Some(reduced.clone()),
                ..cache.clone()
            };
            assert!(!cache_matches(&reduced_cache, &full));
            // Even a matching non-full cache cannot establish an installer
            // channel that does not exist.
            assert!(!cache_matches(&reduced_cache, &reduced));
        }
    }

    #[test]
    fn cache_with_incomplete_artifact_is_not_assumed_full() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut identity = serde_json::to_value(full_artifact())?;
        let object = identity
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("identity not an object"))?;
        let _ = object.remove("profile");
        std::fs::write(
            dir.path().join(CACHE_FILENAME),
            serde_json::to_vec(&serde_json::json!({
                "checked_at": 42,
                "latest": "99.0.0",
                "artifact": identity,
            }))?,
        )?;
        assert!(read_cache(dir.path()).is_none());
        Ok(())
    }
}
