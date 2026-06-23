//! Per-agent static facts and install machinery for `bitrouter spawn`.
//!
//! [`SpawnAgent`] is the set of coding-agent harnesses `spawn` can launch (v1
//! ships Claude Code only); [`AgentSpec`] is the resolved per-agent metadata the
//! launcher drives — the binary name, the routing env vars, and the
//! [agent-model IR](super::model_plan) tier → env-var mapping. The dispatch,
//! env-injection, and install code is written against this metadata rather than
//! hard-coded to one agent, so adding `codex` / `gemini` later is a matter of
//! extending the per-agent matches here.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::ValueEnum;

use crate::spawn::model_plan::ModelTier;
use crate::style::Palette;

/// The coding-agent harnesses `bitrouter spawn` can launch. v1 ships Claude
/// Code only; the enum is the extension point for `codex` / `gemini` later, so
/// the dispatch, env-injection, and install machinery is written against the
/// [`AgentSpec`] metadata rather than hard-coded to one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SpawnAgent {
    /// Anthropic's Claude Code CLI (`claude`).
    Claude,
}

impl SpawnAgent {
    /// Static metadata describing how to find, route, and install this agent.
    pub fn spec(self) -> AgentSpec {
        match self {
            SpawnAgent::Claude => AgentSpec {
                agent: self,
                // The display id matches the `--agent` value.
                id: "claude",
                // The executable name looked up on `PATH`.
                binary: "claude",
                // Claude Code reads its gateway endpoint from
                // `ANTHROPIC_BASE_URL`, and authenticates to that gateway with
                // `ANTHROPIC_AUTH_TOKEN` (sent as the `Authorization: Bearer`
                // header) — this is the documented way to route Claude Code
                // through a custom LLM gateway, and `Authorization: Bearer` is
                // exactly the inbound credential BitRouter expects (`brk_…`).
                // `ANTHROPIC_API_KEY` would instead be sent as `x-api-key`, the
                // first-party Anthropic header, which is not BitRouter's inbound
                // scheme — so we deliberately set the auth token, not the API key.
                // https://code.claude.com/docs/en/llm-gateway#authentication-methods
                base_url_env: "ANTHROPIC_BASE_URL",
                auth_token_env: "ANTHROPIC_AUTH_TOKEN",
            },
        }
    }

    /// Parse a user-facing model-tier label into a generic [`ModelTier`].
    ///
    /// The canonical, agent-neutral labels are `high` / `mid` / `low`; each
    /// agent may also accept its own familiar aliases. Claude Code maps the
    /// three Anthropic capability names onto the tiers: `opus` → high,
    /// `sonnet` → mid, `haiku` → low. Matching is case-insensitive and trims
    /// surrounding whitespace. Returns `None` for an unrecognised label.
    pub fn parse_tier(self, label: &str) -> Option<ModelTier> {
        let label = label.trim().to_ascii_lowercase();
        match self {
            SpawnAgent::Claude => match label.as_str() {
                "high" | "opus" => Some(ModelTier::High),
                "mid" | "sonnet" => Some(ModelTier::Mid),
                "low" | "haiku" => Some(ModelTier::Low),
                _ => None,
            },
        }
    }

    /// A human-readable list of the tier labels this agent accepts, for error
    /// messages.
    pub fn tier_labels(self) -> &'static str {
        match self {
            SpawnAgent::Claude => "high|mid|low (aliases: opus|sonnet|haiku)",
        }
    }
}

/// Resolved, per-agent static facts used by the spawn machinery.
#[derive(Debug, Clone, Copy)]
pub struct AgentSpec {
    /// Which agent this describes.
    pub agent: SpawnAgent,
    /// Catalog id / `--agent` value.
    pub id: &'static str,
    /// Executable name searched for on `PATH`.
    pub binary: &'static str,
    /// Env var the agent reads its gateway base URL from.
    pub base_url_env: &'static str,
    /// Env var the agent reads its gateway bearer token from (sent as
    /// `Authorization: Bearer`), which is BitRouter's inbound auth scheme.
    pub auth_token_env: &'static str,
}

impl AgentSpec {
    /// The environment variable this agent reads the model for `tier` from, or
    /// `None` when the agent does not model that tier.
    ///
    /// Claude Code remaps what each capability alias — and the picker's
    /// `Default` option — resolves to via the `ANTHROPIC_DEFAULT_*_MODEL`
    /// variables, so setting all three redirects the whole agent while
    /// preserving its tiered behaviour (heavy reasoning → opus slot, default
    /// work → sonnet slot, background → haiku slot). See the Claude Code model
    /// configuration reference:
    /// <https://code.claude.com/docs/en/model-config#environment-variables>.
    pub fn tier_env(&self, tier: ModelTier) -> Option<&'static str> {
        match self.agent {
            SpawnAgent::Claude => Some(match tier {
                ModelTier::High => "ANTHROPIC_DEFAULT_OPUS_MODEL",
                ModelTier::Mid => "ANTHROPIC_DEFAULT_SONNET_MODEL",
                ModelTier::Low => "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            }),
        }
    }
}

/// Locate an executable on `PATH`. Pure-`std` (no `which` crate) so the
/// `#![forbid(unsafe_code)]` lib stays dependency-light: split `$PATH` and
/// probe each entry. Returns the first match.
fn resolve_binary(name: &str) -> Option<PathBuf> {
    find_on_path(name, std::env::var_os("PATH"), &extra_search_dirs())
}

/// Core of [`resolve_binary`], factored out for testing. Searches `path` (an
/// `OsString` of `PATH`-separated dirs) followed by `extra` directories —
/// the latter covers the native installer's target (`~/.local/bin`), which is
/// often not yet on `PATH` in the shell that just ran the install.
fn find_on_path(name: &str, path: Option<OsString>, extra: &[PathBuf]) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(path) = path {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.extend(extra.iter().cloned());
    for dir in dirs {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        // On Windows, executables carry an extension. We probe the common
        // launcher extensions rather than parsing the full `%PATHEXT%` set —
        // agent CLIs ship as `.exe` or an npm `.cmd`/`.bat` shim, which these
        // cover; an exotic `%PATHEXT%` entry (`.com`, `.ps1`) would be missed.
        #[cfg(windows)]
        {
            for ext in ["exe", "cmd", "bat"] {
                let with_ext = dir.join(format!("{name}.{ext}"));
                if is_executable_file(&with_ext) {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

/// Directories to probe in addition to `PATH`. The Claude Code native
/// installer drops the binary in `~/.local/bin`, which a freshly-installed
/// shell session may not have on `PATH` yet.
fn extra_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home_dir() {
        dirs.push(home.join(".local").join("bin"));
    }
    dirs
}

/// True when `path` is a regular file we can plausibly execute. On Unix this
/// checks the executable permission bit; on other platforms, file existence.
fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Resolve the user's home directory without pulling in a crate: `$HOME` on
/// Unix, `%USERPROFILE%` on Windows.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Ensure `agent`'s binary is installed — locating it on `PATH` (+
/// `~/.local/bin`) and offering the official native installer when permitted —
/// and return its path. Shared by `bitrouter spawn` and `bitrouter login
/// anthropic` (which needs the `claude` CLI to sign the user in) so both go
/// through one detect-and-install path.
pub(crate) async fn ensure_agent_installed(agent: SpawnAgent, no_install: bool) -> Result<PathBuf> {
    let spec = agent.spec();
    match resolve_binary(spec.binary) {
        Some(path) => Ok(path),
        None => ensure_installed(&spec, no_install).await,
    }
}

/// The agent binary is missing. Offer to install it via the official native
/// installer when stdin is interactive and `--no-install` was not set;
/// otherwise return an actionable error listing the install command.
async fn ensure_installed(spec: &AgentSpec, no_install: bool) -> Result<PathBuf> {
    let install = InstallCommand::for_agent(spec.agent);

    let may_prompt = !no_install && std::io::stdin().is_terminal();
    if !may_prompt {
        anyhow::bail!(
            "agent '{}' is not installed (no `{}` on PATH).\n  Install it with:\n    {}",
            spec.id,
            spec.binary,
            install.display(),
        );
    }

    if !confirm_install(spec, &install)? {
        anyhow::bail!("aborted — '{}' was not installed", spec.id);
    }

    install.run().await?;

    // Re-resolve after install. The installer may have landed the binary in
    // `~/.local/bin` (covered by `extra_search_dirs`) even when that dir is
    // not on the current shell's `PATH`.
    resolve_binary(spec.binary).ok_or_else(|| {
        anyhow::anyhow!(
            "installed '{}' but still cannot find `{}` on PATH or in ~/.local/bin — \
             open a new shell (or add the install dir to PATH) and re-run",
            spec.id,
            spec.binary,
        )
    })
}

/// Print the install prompt and read a Y/n answer. Defaults to yes on a bare
/// <enter>. A closed stdin (EOF) is treated as "no" so we never hang.
fn confirm_install(spec: &AgentSpec, install: &InstallCommand) -> Result<bool> {
    use std::io::{BufRead, Write};
    let p = Palette::for_stderr();
    eprintln!(
        "{cyan}{bold}info:{reset} agent `{}` is not installed on this machine.",
        spec.id,
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );
    eprintln!("  Installer: {}", install.display());
    eprint!("Proceed to install? [Y/n]: ");
    std::io::stderr().flush().ok();

    let stdin = std::io::stdin();
    let mut line = String::new();
    let n = stdin
        .lock()
        .read_line(&mut line)
        .context("reading install confirmation from stdin")?;
    if n == 0 {
        // EOF — non-interactive; decline rather than block.
        eprintln!();
        return Ok(false);
    }
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// A platform-specific install command for an agent. Conditional compilation
/// makes exactly one variant visible per platform, so the help text and the
/// executed command never disagree with the host.
#[derive(Debug, Clone)]
pub struct InstallCommand {
    /// Program to run (`bash` / `powershell`).
    program: &'static str,
    /// Arguments to that program.
    args: Vec<String>,
    /// Human-readable one-liner, e.g. `curl -fsSL … | bash`.
    human: String,
}

impl InstallCommand {
    /// The official native installer for `agent` on the *current* platform.
    ///
    /// Sources (Claude Code quickstart, "Native Install"):
    /// <https://code.claude.com/docs/en/quickstart>
    /// - macOS / Linux: `curl -fsSL https://claude.ai/install.sh | bash`
    /// - Windows:       `irm https://claude.ai/install.ps1 | iex`
    pub fn for_agent(agent: SpawnAgent) -> Self {
        match agent {
            SpawnAgent::Claude => Self::claude(),
        }
    }

    #[cfg(not(windows))]
    fn claude() -> Self {
        let human = "curl -fsSL https://claude.ai/install.sh | bash".to_string();
        Self {
            program: "bash",
            args: vec![
                "-c".to_string(),
                "curl -fsSL https://claude.ai/install.sh | bash".to_string(),
            ],
            human,
        }
    }

    #[cfg(windows)]
    fn claude() -> Self {
        let human = "irm https://claude.ai/install.ps1 | iex".to_string();
        Self {
            program: "powershell",
            args: vec![
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "irm https://claude.ai/install.ps1 | iex".to_string(),
            ],
            human,
        }
    }

    /// The human-readable one-liner shown in prompts and error messages.
    pub fn display(&self) -> &str {
        &self.human
    }

    /// Execute the installer, inheriting stdio so the user sees its progress.
    /// Errors when the installer exits non-zero.
    async fn run(&self) -> Result<()> {
        let p = Palette::for_stderr();
        eprintln!(
            "{cyan}spawn:{reset} installing — {}",
            self.human,
            cyan = p.cyan,
            reset = p.reset,
        );
        let status = tokio::process::Command::new(self.program)
            .args(&self.args)
            .status()
            .await
            .with_context(|| format!("running installer: {}", self.human))?;
        if !status.success() {
            anyhow::bail!("installer exited with {status}: {}", self.human);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_spec_uses_anthropic_env_vars() {
        let spec = SpawnAgent::Claude.spec();
        assert_eq!(spec.binary, "claude");
        assert_eq!(spec.base_url_env, "ANTHROPIC_BASE_URL");
        assert_eq!(spec.auth_token_env, "ANTHROPIC_AUTH_TOKEN");
    }

    #[test]
    fn parse_tier_accepts_canonical_and_anthropic_aliases() {
        let a = SpawnAgent::Claude;
        // Canonical, agent-neutral labels.
        assert_eq!(a.parse_tier("high"), Some(ModelTier::High));
        assert_eq!(a.parse_tier("mid"), Some(ModelTier::Mid));
        assert_eq!(a.parse_tier("low"), Some(ModelTier::Low));
        // Anthropic-name aliases.
        assert_eq!(a.parse_tier("opus"), Some(ModelTier::High));
        assert_eq!(a.parse_tier("sonnet"), Some(ModelTier::Mid));
        assert_eq!(a.parse_tier("haiku"), Some(ModelTier::Low));
        // Case-insensitive + whitespace-trimmed.
        assert_eq!(a.parse_tier("  OPUS "), Some(ModelTier::High));
    }

    #[test]
    fn parse_tier_rejects_unknown_label() {
        assert_eq!(SpawnAgent::Claude.parse_tier("turbo"), None);
        assert_eq!(SpawnAgent::Claude.parse_tier(""), None);
    }

    #[test]
    fn tier_env_maps_claude_tiers_to_default_model_vars() {
        let spec = SpawnAgent::Claude.spec();
        assert_eq!(
            spec.tier_env(ModelTier::High),
            Some("ANTHROPIC_DEFAULT_OPUS_MODEL")
        );
        assert_eq!(
            spec.tier_env(ModelTier::Mid),
            Some("ANTHROPIC_DEFAULT_SONNET_MODEL")
        );
        assert_eq!(
            spec.tier_env(ModelTier::Low),
            Some("ANTHROPIC_DEFAULT_HAIKU_MODEL")
        );
    }

    #[test]
    fn install_command_is_the_official_native_installer() {
        let cmd = InstallCommand::for_agent(SpawnAgent::Claude);
        // Same canonical URL on every platform; the transport differs.
        assert!(cmd.display().contains("claude.ai/install"));
        #[cfg(not(windows))]
        {
            assert!(cmd.display().contains("install.sh"));
            assert!(cmd.display().contains("| bash"));
        }
        #[cfg(windows)]
        {
            assert!(cmd.display().contains("install.ps1"));
        }
    }

    #[test]
    fn find_on_path_locates_executable_in_listed_dir() {
        let dir = std::env::temp_dir().join(format!("bitrouter-spawn-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-agent");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([dir.as_os_str()]).unwrap();
        let found = find_on_path("fake-agent", Some(path), &[]);
        assert_eq!(found.as_deref(), Some(bin.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_on_path_returns_none_when_absent() {
        let path = std::env::join_paths([std::env::temp_dir().as_os_str()]).unwrap();
        assert!(find_on_path("definitely-not-a-real-binary-xyz", Some(path), &[]).is_none());
    }

    #[test]
    fn find_on_path_falls_back_to_extra_dirs() {
        // The post-install re-resolution relies on `extra` (e.g. ~/.local/bin)
        // even when PATH is empty — exercise that path explicitly.
        let dir =
            std::env::temp_dir().join(format!("bitrouter-spawn-extra-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-agent");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // PATH is None entirely; the binary is only reachable via `extra`.
        let found = find_on_path("fake-agent", None, std::slice::from_ref(&dir));
        assert_eq!(found.as_deref(), Some(bin.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
