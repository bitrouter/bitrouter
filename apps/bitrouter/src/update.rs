//! `bro update` — in-place self-updater built on cargo-dist's
//! `axoupdater`. Decision logic (install-method detection, release channel,
//! nudge-cache TTL) lives in small pure functions so it can be unit-tested
//! without touching the network or replacing the running binary.

use crate::commands::invocation;
use crate::output::reports::update::UpdateReport;
use crate::{daemon, style};
use anyhow::Result;
use axoupdater::{AxoUpdater, UpdateRequest};
use bitrouter_ai::artifact::{ArtifactIdentity, DistributionProfile};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How the running `bro` binary was installed, used to pick the right
/// upgrade path when there is no cargo-dist receipt to self-update from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallMethod {
    Homebrew,
    Cargo,
    Npm,
    Unknown,
}

/// True when `exe` is the binary the cargo-dist npm wrapper unpacks and execs.
/// The wrapper extracts the platform binary to `<pkg>/node_modules/.bin_real/`
/// and spawns it directly, so a process launched via `npm`/`npx` reports that
/// path from `current_exe()`. Matched on path components so it holds on both
/// `/`- and `\`-separated platforms.
fn is_npm_install(exe: &Path) -> bool {
    fn normal(c: Option<std::path::Component<'_>>) -> Option<&str> {
        match c {
            Some(std::path::Component::Normal(s)) => s.to_str(),
            _ => None,
        }
    }
    // .../ node_modules / .bin_real / <binary>
    let mut rev = exe.components().rev();
    rev.next(); // the binary file itself
    normal(rev.next()) == Some(".bin_real") && normal(rev.next()) == Some("node_modules")
}

/// Best-effort classification from the executable path and the resolved Cargo
/// home. npm installs run from `.../node_modules/.bin_real/...`; Homebrew
/// installs live under a `Cellar/bitrouter/...` path; `cargo install` places the
/// binary in `<CARGO_HOME>/bin`.
fn detect_install_method(exe: &Path, cargo_home: Option<&Path>) -> InstallMethod {
    // npm is checked first, and deliberately before Homebrew: a global npm
    // install under a Homebrew-managed Node lives beneath `/opt/homebrew/...`,
    // so the `/homebrew/` substring below would otherwise misclassify it.
    if is_npm_install(exe) {
        return InstallMethod::Npm;
    }
    let exe_str = exe.to_string_lossy();
    if exe_str.contains("/Cellar/bitrouter/") || exe_str.contains("/homebrew/") {
        return InstallMethod::Homebrew;
    }
    if cargo_home.is_some_and(|home| exe.starts_with(home.join("bin"))) {
        return InstallMethod::Cargo;
    }
    InstallMethod::Unknown
}

/// Only an unclassified executable may rely on a cargo-dist receipt. Receipts
/// are global user state and can describe a different installation method.
fn should_load_receipt(method: InstallMethod) -> bool {
    method == InstallMethod::Unknown
}

/// Stable lowercase label for the JSON `install_method` field.
fn method_label(method: InstallMethod) -> &'static str {
    match method {
        InstallMethod::Homebrew => "homebrew",
        InstallMethod::Cargo => "cargo",
        InstallMethod::Npm => "npm",
        InstallMethod::Unknown => "unknown",
    }
}

/// The exact command a user should run to upgrade a package-manager-managed
/// install. Unknown installs get the generic re-run-the-installer hint.
fn delegation_command(method: InstallMethod) -> String {
    match method {
        InstallMethod::Homebrew => "brew upgrade bitrouter".to_string(),
        InstallMethod::Cargo => "cargo install bitrouter --force".to_string(),
        // CI publishes prereleases to npm under the default `latest` tag; switch
        // to `@next` here if a dedicated prerelease dist-tag is ever introduced.
        InstallMethod::Npm => "npm i -g bitrouter@latest".to_string(),
        InstallMethod::Unknown => {
            "curl --proto '=https' --tlsv1.2 -LsSf \
             https://github.com/bitrouter/bitrouter/releases/latest/download/bitrouter-installer.sh | sh"
                .to_string()
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum UpdatePlan {
    ReleaseInstaller,
    Delegated {
        command: String,
        reason: Option<String>,
    },
}

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

/// Preserve the artifact builder's effective feature set verbatim. In
/// particular, do not infer a profile from the installation path or normalize
/// aliases/transitive features a second time here.
fn plan_update(
    method: InstallMethod,
    artifact: &ArtifactIdentity,
    spec: &VersionSpec,
) -> UpdatePlan {
    if !has_release_installer(artifact) {
        let profile = artifact.profile.as_str();
        let shell = CommandShell::native();
        let shell_label = shell.label();
        return UpdatePlan::Delegated {
            command: source_install_command(artifact, spec, shell),
            reason: Some(format!(
                "No matching release installer/receipt is available for the {profile} profile. \
                 Build from source with the exact installed features. Without --tag \
                 (including --stable), Cargo's \
                 registry version selection applies. Cargo installs into its configured bin \
                 directory; select that binary if this installation lives elsewhere. \
                 Run the printed command in {shell_label}."
            )),
        };
    }
    if should_load_receipt(method) {
        UpdatePlan::ReleaseInstaller
    } else {
        UpdatePlan::Delegated {
            command: delegation_command(method),
            reason: None,
        }
    }
}

/// A source update must explicitly disable defaults even for an empty feature
/// set. Cargo's version and feature selection contracts:
/// https://doc.rust-lang.org/cargo/commands/cargo-install.html
fn source_install_command(
    artifact: &ArtifactIdentity,
    spec: &VersionSpec,
    shell: CommandShell,
) -> String {
    let mut command = "cargo install bitrouter --force --no-default-features".to_string();
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

/// Options parsed from the `bro update` flags.
#[derive(Debug)]
pub struct UpdateOptions {
    pub check: bool,
    pub tag: Option<String>,
    pub stable: bool,
    pub restart: bool,
    pub yes: bool,
}

/// The result of `run`: the report to emit through the [`Output`] driver, plus
/// whether the dispatch layer should ask the newly installed binary to run a
/// safe handoff before emitting.
///
/// [`Output`]: crate::output::Output
#[derive(Debug)]
pub struct RunOutcome {
    /// The single result value for `bro update`, rendered to stdout.
    pub report: UpdateReport,
    /// A running daemon needs a safe handoff to pick up the new binary.
    pub restart_needed: bool,
}

impl RunOutcome {
    /// Record replacement identity after the installer has completed.
    /// Failed verification preserves the installation result but prevents an
    /// automatic handoff, including for legacy targets without `build-info`.
    pub fn record_target_verification(
        &mut self,
        result: std::result::Result<ArtifactIdentity, String>,
    ) {
        let verified = result.and_then(|target| {
            match self.report.target_version.as_deref() {
                Some(expected) if target.version == expected => {}
                Some(expected) => {
                    return Err(format!(
                        "installed artifact version {} does not match installer target {expected}",
                        target.version
                    ));
                }
                None => return Err("the installer target version is unknown".to_string()),
            }
            let Some(current) = self.report.current_artifact.as_ref() else {
                return Err("the invoking artifact identity is unknown".to_string());
            };
            if !crate::upgrade::same_capabilities(current, &target) {
                return Err(
                    "installed artifact capabilities (profile, features or target) do not match the invoking executable"
                        .to_string(),
                );
            }
            Ok(target)
        });
        match verified {
            Ok(target) => {
                self.report.target_artifact = Some(target);
                self.report.target_verification_error = None;
            }
            Err(error) => {
                self.report.target_artifact = None;
                if self.restart_needed {
                    self.report.daemon = Some("deferred");
                    self.report.daemon_reason = Some(format!(
                        "the running daemon was not handed off because the installed artifact could not be verified: {error}"
                    ));
                }
                self.report.target_verification_error = Some(error);
                self.restart_needed = false;
            }
        }
    }

    /// A terminal outcome that needs no daemon restart.
    fn done(report: UpdateReport) -> Self {
        Self {
            report,
            restart_needed: false,
        }
    }
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

/// The receipt helper checks only the installation prefix. Require the
/// canonical executable basename too, so a renamed copy beside `bro` cannot
/// update the separate official binary. Callers resolve symlinks first.
/// https://github.com/axodotdev/axoupdater/blob/v0.10.0/axoupdater/src/receipt.rs#L97-L136
fn has_official_executable_name(canonical_exe: &Path) -> bool {
    let expected = if cfg!(windows) { "bro.exe" } else { "bro" };
    canonical_exe
        .file_name()
        .is_some_and(|name| name == expected)
}

/// Configure an axoupdater for the `bitrouter` app. Used only for an unknown
/// install method: package-manager installs must delegate even if an unrelated
/// cargo-dist receipt exists on the machine.
fn build_updater(spec: VersionSpec) -> std::result::Result<AxoUpdater, &'static str> {
    let exe = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(
            |_| "The canonical executable path could not be verified; self-update was refused.",
        )?;
    if !has_official_executable_name(&exe) {
        return Err(
            "The canonical executable is not the official bro binary; self-update of a renamed copy was refused.",
        );
    }
    let mut updater = AxoUpdater::new_for("bitrouter");
    if let Err(e) = updater.load_receipt() {
        tracing::debug!(error = %e, "no install receipt; delegating to package manager");
        return Err("No usable cargo-dist install receipt is available for this installation.");
    }
    configure_owned_updater(&mut updater, spec)?;
    Ok(updater)
}

/// Check ownership before enabling forced updates or making any network call.
/// axoupdater 0.10.0 returns early from is_update_needed when always_update is
/// true, bypassing its usual ownership check:
/// https://github.com/axodotdev/axoupdater/blob/v0.10.0/axoupdater/src/lib.rs#L332-L339
/// Use the real receipt helper (including legacy prefix normalization):
/// https://github.com/axodotdev/axoupdater/blob/v0.10.0/axoupdater/src/receipt.rs#L97-L136
fn configure_owned_updater(
    updater: &mut AxoUpdater,
    spec: VersionSpec,
) -> std::result::Result<(), &'static str> {
    match updater.check_receipt_is_for_this_executable() {
        Ok(true) => {}
        Ok(false) => {
            return Err(
                "The cargo-dist receipt belongs to a different executable installation; self-update was refused.",
            );
        }
        Err(e) => {
            tracing::debug!(error = %e, "could not verify install receipt ownership");
            return Err(
                "Ownership of this executable could not be verified from the cargo-dist receipt; self-update was refused.",
            );
        }
    }
    updater.always_update(matches!(spec, VersionSpec::Tag(_)));
    updater.configure_version_specifier(to_request(spec));
    Ok(())
}

pub async fn run(opts: UpdateOptions, socket: &Path) -> Result<RunOutcome> {
    let artifact = crate::artifact::current();
    let current = artifact.version.clone();
    let spec = choose_spec(opts.tag.as_deref(), opts.stable);

    // 1. The running binary's path is authoritative. A global cargo-dist
    // receipt can outlive an older Cargo install while this invocation runs a
    // Homebrew or npm binary, so never let that receipt override a known
    // package-manager install.
    let exe = std::env::current_exe()?;
    let method = detect_install_method(&exe, cargo_home().as_deref());
    if let UpdatePlan::Delegated { command, reason } = plan_update(method, &artifact, &spec) {
        let mut report = UpdateReport::delegated(current, method_label(method), command);
        report.manual_reason = reason;
        return Ok(RunOutcome::done(report.with_current_artifact(artifact)));
    }

    let target_label = match &spec {
        VersionSpec::Tag(t) => format!("version {t}"),
        _ => "the latest release".to_string(),
    };

    // 2. Only a full-profile unknown install with verified receipt ownership
    // can use the release installer, including when --tag forces an update.
    let mut updater = match build_updater(spec) {
        Ok(updater) => updater,
        Err(reason) => {
            let mut report = UpdateReport::delegated(
                current,
                method_label(InstallMethod::Unknown),
                delegation_command(InstallMethod::Unknown),
            );
            report.manual_reason = Some(reason.to_string());
            return Ok(RunOutcome::done(report.with_current_artifact(artifact)));
        }
    };

    // 3. Dry run.
    if opts.check {
        let available = updater.is_update_needed().await?;
        let target = if available {
            updater.query_new_version().await?.map(|v| v.to_string())
        } else {
            None
        };
        return Ok(RunOutcome::done(
            UpdateReport::checked(current, available, target).with_current_artifact(artifact),
        ));
    }

    // 4. Confirm + swap. The prompt is interactive UI, not the command result,
    // so it goes to stderr — stdout carries only the final report.
    if !opts.yes && !confirm(&current, &target_label)? {
        return Ok(RunOutcome::done(
            UpdateReport::aborted(current).with_current_artifact(artifact),
        ));
    }
    let Some(result) = updater.run().await? else {
        return Ok(RunOutcome::done(
            UpdateReport::unchanged(current).with_current_artifact(artifact),
        ));
    };
    let new_version = result.new_version.to_string();

    // 5. The old process cannot safely preflight migrations compiled into the
    // newly installed binary. The dispatch layer invokes that binary to
    // attempt the handoff and sets the final daemon outcome before emitting.
    let daemon_running = daemon::endpoint_in_use(socket);
    let _legacy_restart_flag = opts.restart;
    let daemon = daemon_running.then_some("handoff_pending");
    Ok(RunOutcome {
        report: UpdateReport::updated(current, new_version, daemon).with_current_artifact(artifact),
        restart_needed: daemon_running,
    })
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

/// Best-effort "update available" line for `bro status`. Never returns an
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
        // pollutes a command's JSON stdout (e.g. `bro status`).
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
    if !should_load_receipt(detect_install_method(&exe, cargo_home().as_deref())) {
        return None;
    }
    let mut updater = build_updater(VersionSpec::LatestPrerelease).ok()?;
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
    use crate::output::CliReport;
    use std::path::Path;

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
            handoff_build_id: "test-build".into(),
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
    ) -> RunOutcome {
        RunOutcome {
            report: UpdateReport::updated(
                current.version.clone(),
                target_version.to_string(),
                daemon_running.then_some("handoff_pending"),
            )
            .with_current_artifact(current.clone()),
            restart_needed: daemon_running,
        }
    }

    #[test]
    fn verified_replacement_preserves_pending_handoff_and_distinct_identities() {
        let current = full_artifact();
        let mut target = current.clone();
        target.version = "1.0.0-alpha.20".into();
        target.source_revision = Some("new-revision".into());
        target.handoff_build_id = "new-build".into();
        for daemon_running in [false, true] {
            let mut outcome = completed_update(&current, &target.version, daemon_running);
            outcome.record_target_verification(Ok(target.clone()));
            assert_eq!(outcome.report.status, "updated");
            assert_eq!(outcome.report.current_artifact.as_ref(), Some(&current));
            assert_eq!(outcome.report.target_artifact.as_ref(), Some(&target));
            assert!(outcome.report.target_verification_error.is_none());
            assert_eq!(outcome.restart_needed, daemon_running);
            assert_eq!(
                outcome.report.daemon,
                daemon_running.then_some("handoff_pending")
            );
            assert!(outcome.report.daemon_reason.is_none());
            assert_eq!(outcome.report.exit_code(), 0);
        }
    }

    #[test]
    fn unverified_rollback_retains_installation_and_only_defers_an_existing_daemon() {
        let current = full_artifact();
        for error in [
            "legacy release does not support build-info",
            "decoding replacement build-info: invalid JSON",
            "replacement build-info timed out",
        ] {
            for daemon_running in [false, true] {
                let mut outcome = completed_update(&current, "1.0.0-alpha.18", daemon_running);
                outcome.record_target_verification(Err(error.to_string()));
                assert_eq!(outcome.report.status, "updated");
                assert_eq!(
                    outcome.report.target_version.as_deref(),
                    Some("1.0.0-alpha.18")
                );
                assert_eq!(outcome.report.current_artifact.as_ref(), Some(&current));
                assert!(outcome.report.target_artifact.is_none());
                assert_eq!(
                    outcome.report.target_verification_error.as_deref(),
                    Some(error)
                );
                assert!(!outcome.restart_needed);
                assert_eq!(outcome.report.daemon, daemon_running.then_some("deferred"));
                if daemon_running {
                    assert!(
                        outcome
                            .report
                            .daemon_reason
                            .as_deref()
                            .is_some_and(|reason| {
                                reason.contains("not handed off") && reason.contains(error)
                            })
                    );
                } else {
                    assert!(outcome.report.daemon_reason.is_none());
                }
                assert_eq!(outcome.report.exit_code(), i32::from(daemon_running));
            }
        }
    }

    #[test]
    fn mismatched_replacement_defers_resident_without_claiming_verified_identity() {
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
            assert_eq!(outcome.report.status, "updated");
            assert_eq!(
                outcome.report.target_version.as_ref(),
                Some(&target.version)
            );
            assert_eq!(outcome.report.current_artifact.as_ref(), Some(&current));
            assert!(outcome.report.target_artifact.is_none());
            assert!(
                outcome
                    .report
                    .target_verification_error
                    .as_deref()
                    .is_some_and(|error| error.contains(expected_error))
            );
            assert!(!outcome.restart_needed);
            assert_eq!(outcome.report.daemon, Some("deferred"));
            assert!(
                outcome
                    .report
                    .daemon_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("not handed off"))
            );
            assert_eq!(outcome.report.exit_code(), 1);
        }
    }

    #[test]
    fn full_profile_preserves_install_method_policy() {
        let full = full_artifact();
        for spec in [
            VersionSpec::Latest,
            VersionSpec::LatestPrerelease,
            VersionSpec::Tag("1.0.0-alpha.18".into()),
        ] {
            for method in [
                InstallMethod::Homebrew,
                InstallMethod::Cargo,
                InstallMethod::Npm,
            ] {
                assert_eq!(
                    plan_update(method, &full, &spec),
                    UpdatePlan::Delegated {
                        command: delegation_command(method),
                        reason: None,
                    }
                );
            }
            assert_eq!(
                plan_update(InstallMethod::Unknown, &full, &spec),
                UpdatePlan::ReleaseInstaller
            );
        }
    }

    #[test]
    fn reduced_profiles_delegate_for_every_install_method_and_channel() -> Result<()> {
        for (identity, features) in [
            (artifact(DistributionProfile::Gateway, &[]), vec![]),
            (
                artifact(DistributionProfile::Custom, &["core"]),
                vec!["core"],
            ),
            (
                artifact(
                    DistributionProfile::Custom,
                    &["core", "harness", "mcp", "otel-http", "tui"],
                ),
                vec!["core", "harness", "mcp", "otel-http", "tui"],
            ),
        ] {
            for method in [
                InstallMethod::Homebrew,
                InstallMethod::Cargo,
                InstallMethod::Npm,
                InstallMethod::Unknown,
            ] {
                for spec in [
                    VersionSpec::Latest,
                    VersionSpec::LatestPrerelease,
                    VersionSpec::Tag("v1.0.0-alpha.18".into()),
                ] {
                    let UpdatePlan::Delegated { command, reason } =
                        plan_update(method, &identity, &spec)
                    else {
                        anyhow::bail!("a reduced profile selected the full release installer");
                    };
                    assert!(
                        reason
                            .as_deref()
                            .is_some_and(|r| r.contains(identity.profile.as_str()))
                    );
                    assert!(
                        reason
                            .as_deref()
                            .is_some_and(|r| r.contains(CommandShell::native().label()))
                    );
                    let mut expected = vec![
                        "cargo",
                        "install",
                        "bitrouter",
                        "--force",
                        "--no-default-features",
                    ]
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                    if !features.is_empty() {
                        expected.extend(["--features".to_string(), features.join(",")]);
                    }
                    if matches!(spec, VersionSpec::Tag(_)) {
                        expected.extend(["--version".to_string(), "=1.0.0-alpha.18".to_string()]);
                    }
                    if cfg!(windows) {
                        let mut expected_command =
                            "cargo install bitrouter --force --no-default-features".to_string();
                        if !features.is_empty() {
                            expected_command
                                .push_str(&format!(" --features '{}'", features.join(",")));
                        }
                        if matches!(spec, VersionSpec::Tag(_)) {
                            expected_command.push_str(" --version '=1.0.0-alpha.18'");
                        }
                        assert_eq!(
                            command,
                            format!(
                                "& {{ $PSNativeCommandArgumentPassing = 'Standard'; {expected_command} }}"
                            )
                        );
                    } else {
                        assert_eq!(shell_words::split(&command)?, expected);
                    }
                }
            }
        }
        Ok(())
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
            "& { $PSNativeCommandArgumentPassing = 'Standard'; cargo install bitrouter --force --no-default-features }"
        );
        let custom = artifact(DistributionProfile::Custom, &["core", "mcp"]);
        assert_eq!(
            source_install_command(
                &custom,
                &VersionSpec::Tag("v1.0.0-alpha.18".into()),
                CommandShell::PowerShell,
            ),
            "& { $PSNativeCommandArgumentPassing = 'Standard'; cargo install bitrouter --force --no-default-features --features 'core,mcp' --version '=1.0.0-alpha.18' }"
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
                    "& {{ $PSNativeCommandArgumentPassing = 'Standard'; cargo install bitrouter --force --no-default-features --git https://github.com/bitrouter/bitrouter {quoted_argument} }}"
                )
            );
        }
    }

    #[test]
    fn official_basename_is_required_even_in_the_same_install_prefix() {
        let prefix = Path::new("/install/bin");
        let official = if cfg!(windows) { "bro.exe" } else { "bro" };
        assert!(has_official_executable_name(&prefix.join(official)));
        for renamed in [
            "bro-renamed",
            "bro-renamed.exe",
            "bro.exe.old",
            "bro-backup",
            "bitrouter",
            "bitrouter.exe",
            "another-tool",
        ] {
            assert!(!has_official_executable_name(&prefix.join(renamed)));
        }
        let other_platform = if cfg!(windows) { "bro" } else { "bro.exe" };
        assert!(!has_official_executable_name(&prefix.join(other_platform)));
        assert!(!has_official_executable_name(Path::new("")));
        assert!(!has_official_executable_name(Path::new("/")));
    }

    #[test]
    fn missing_receipt_ownership_refuses_forced_tag() {
        let mut updater = AxoUpdater::new_for("bitrouter");
        assert!(
            configure_owned_updater(&mut updater, VersionSpec::Tag("1.0.0-alpha.18".into()))
                .is_err()
        );
    }

    #[tokio::test]
    async fn wrong_receipt_refuses_forced_tag_before_enabling_always_update() -> Result<()> {
        let other_install = tempfile::tempdir()?;
        let prefix = other_install
            .path()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 test path"))?;
        for spec in [
            VersionSpec::LatestPrerelease,
            VersionSpec::Tag("1.0.0-alpha.18".into()),
        ] {
            let mut updater = AxoUpdater::new_for("bitrouter");
            // Exercise axoupdater's actual receipt helper without global env
            // changes, network calls, or invoking an installer.
            updater.set_install_dir(prefix);
            assert!(!updater.check_receipt_is_for_this_executable()?);
            assert!(configure_owned_updater(&mut updater, spec).is_err());
            // With no release source configured, this can only succeed by
            // refusing the foreign receipt before any network request. It
            // would be true if our --tag path had enabled always_update.
            assert!(!updater.is_update_needed().await?);
        }
        Ok(())
    }

    #[tokio::test]
    async fn owned_receipt_can_force_a_tag_without_querying_a_release() -> Result<()> {
        let exe = std::env::current_exe()?.canonicalize()?;
        let prefix = exe
            .parent()
            .and_then(Path::to_str)
            .ok_or_else(|| anyhow::anyhow!("test executable has no UTF-8 parent"))?;
        let mut updater = AxoUpdater::new_for("bitrouter");
        updater.set_install_dir(prefix);
        assert!(updater.check_receipt_is_for_this_executable()?);
        configure_owned_updater(&mut updater, VersionSpec::Tag("1.0.0-alpha.18".into()))
            .map_err(anyhow::Error::msg)?;
        // Forced-tag eligibility returns immediately; no source or installer
        // is configured and no update is performed.
        assert!(updater.is_update_needed().await?);
        Ok(())
    }

    #[test]
    fn detects_homebrew_from_cellar_path() {
        let exe = Path::new("/opt/homebrew/Cellar/bitrouter/1.0.0/bin/bro");
        assert_eq!(detect_install_method(exe, None), InstallMethod::Homebrew);
    }

    #[test]
    fn managed_homebrew_binary_does_not_load_a_global_receipt() {
        // A cargo-dist receipt is global to the user, not to an executable.
        // `run` must therefore delegate based on this path before it attempts
        // to load any receipt.
        let exe = Path::new("/opt/homebrew/Cellar/bitrouter/1.0.0/bin/bro");
        assert!(!should_load_receipt(detect_install_method(exe, None)));
    }

    #[test]
    fn detects_cargo_from_cargo_home_bin() {
        let exe = Path::new("/home/me/.cargo/bin/bro");
        let cargo_home = Path::new("/home/me/.cargo");
        assert_eq!(
            detect_install_method(exe, Some(cargo_home)),
            InstallMethod::Cargo
        );
    }

    #[test]
    fn detects_npm_from_bin_real_path() {
        let exe = Path::new("/usr/local/lib/node_modules/bitrouter/node_modules/.bin_real/bro");
        assert_eq!(detect_install_method(exe, None), InstallMethod::Npm);
    }

    #[test]
    fn npm_under_homebrew_node_is_npm_not_homebrew() {
        // A global npm install under a Homebrew-managed Node contains
        // `/homebrew/`; npm detection must win over the Homebrew heuristic.
        let exe = Path::new("/opt/homebrew/lib/node_modules/bitrouter/node_modules/.bin_real/bro");
        assert_eq!(detect_install_method(exe, None), InstallMethod::Npm);
    }

    #[test]
    fn unknown_when_no_signal_matches() {
        let exe = Path::new("/usr/local/bin/bro");
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
        assert_eq!(
            delegation_command(InstallMethod::Npm),
            "npm i -g bitrouter@latest"
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
        next.handoff_build_id = "another-build".into();
        assert!(cache_matches(&cache, &next));
        next.target = "aarch64-apple-darwin".into();
        assert!(!cache_matches(&cache, &next));
        next = full.clone();
        next.features.retain(|feature| feature != "tui");
        assert!(!cache_matches(&cache, &next));

        for reduced in [
            artifact(DistributionProfile::Gateway, &[]),
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
