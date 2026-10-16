//! CLI discovery and installation shared by provider login and harness launch.
//! Installer facts and confirmation behavior live here without depending on
//! harness resolution or terminal UI features.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::ValueEnum;

use crate::style::Palette;

/// The two CLIs BitRouter ships a native installer for. This enum is used by
/// the onboarding wizard's `--harness`, `providers login claude-code`, and the
/// deprecated `spawn --agent` alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SpawnAgent {
    /// Anthropic's Claude Code CLI (`claude`).
    Claude,
    /// OpenAI's Codex CLI (`codex`).
    Codex,
}

impl SpawnAgent {
    /// Static metadata describing how to find this agent.
    pub fn spec(self) -> AgentSpec {
        match self {
            SpawnAgent::Claude => AgentSpec {
                // The display id matches the `--agent` value.
                id: "claude",
                // The executable name looked up on `PATH`.
                binary: "claude",
            },
            SpawnAgent::Codex => AgentSpec {
                id: "codex",
                binary: "codex",
            },
        }
    }
}

/// Static identity and executable name for a CLI with a bundled installer.
#[derive(Debug, Clone, Copy)]
pub struct AgentSpec {
    /// CLI id / `--agent` value.
    pub id: &'static str,
    /// Executable name searched for on `PATH`.
    pub binary: &'static str,
}

/// Locate an executable on `PATH`. Pure-`std` (no `which` crate) so the
/// `#![forbid(unsafe_code)]` lib stays dependency-light: split `$PATH` and
/// probe each entry. Returns the first match.
pub(crate) fn resolve_binary(name: &str) -> Option<PathBuf> {
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

/// Ensure `agent`'s binary is installed, offering its official native installer
/// when permitted. Provider login uses this without resolving a harness.
pub(crate) async fn ensure_agent_installed(agent: SpawnAgent, no_install: bool) -> Result<PathBuf> {
    ensure_binary_installed(agent.spec().binary, None, no_install).await
}

/// Locate a CLI on `PATH` or in the native install directory, offering a bundled
/// installer when permitted. Callers can supply their upstream project URL for
/// the missing-binary error when no installer is bundled.
pub(crate) async fn ensure_binary_installed(
    binary: &str,
    project_url: Option<&str>,
    no_install: bool,
) -> Result<PathBuf> {
    match resolve_binary(binary) {
        Some(path) => Ok(path),
        None => ensure_installed(binary, project_url, no_install).await,
    }
}

/// The agent binary is missing. Offer to install it via the official native
/// installer when stdin is interactive and `--no-install` was not set;
/// otherwise return an actionable error listing the install command.
async fn ensure_installed(
    binary: &str,
    project_url: Option<&str>,
    no_install: bool,
) -> Result<PathBuf> {
    let Some(install) = InstallCommand::for_binary(binary) else {
        let upstream = match project_url {
            Some(url) => format!(" from {url}"),
            None => String::new(),
        };
        anyhow::bail!(
            "agent '{binary}' is not installed (no `{binary}` on PATH).\n  \
             BitRouter bundles no installer for it — install it{upstream}",
        );
    };

    let may_prompt = !no_install && std::io::stdin().is_terminal();
    if !may_prompt {
        anyhow::bail!(
            "agent '{binary}' is not installed (no `{binary}` on PATH).\n  Install it with:\n    {}",
            install.display(),
        );
    }

    if !confirm_install(binary, &install)? {
        anyhow::bail!("aborted — '{binary}' was not installed");
    }

    install.run().await?;

    // Re-resolve after install. The installer may have landed the binary in
    // `~/.local/bin` (covered by `extra_search_dirs`) even when that dir is
    // not on the current shell's `PATH`.
    resolve_binary(binary).ok_or_else(|| {
        anyhow::anyhow!(
            "installed '{binary}' but still cannot find `{binary}` on PATH or in ~/.local/bin — \
             open a new shell (or add the install dir to PATH) and re-run",
        )
    })
}

/// Print the install prompt and read a Y/n answer. Defaults to yes on a bare
/// <enter>. A closed stdin (EOF) is treated as "no" so we never hang.
fn confirm_install(binary: &str, install: &InstallCommand) -> Result<bool> {
    use std::io::{BufRead, Write};
    let p = Palette::for_stderr();
    eprintln!(
        "{cyan}{bold}info:{reset} agent `{binary}` is not installed on this machine.",
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
struct InstallCommand {
    /// Program to run (`bash` / `powershell`).
    program: &'static str,
    /// Arguments to that program.
    args: Vec<String>,
    /// Human-readable one-liner, e.g. `curl -fsSL … | bash`.
    human: String,
}

impl InstallCommand {
    /// The official native installer for `binary` on the *current* platform,
    /// or `None` when the user must install that CLI from upstream.
    ///
    /// Sources:
    /// - Claude Code quickstart, "Native Install":
    ///   <https://code.claude.com/docs/en/quickstart>
    /// - macOS / Linux: `curl -fsSL https://claude.ai/install.sh | bash`
    /// - Windows:       `irm https://claude.ai/install.ps1 | iex`
    /// - Codex quickstart:
    ///   <https://developers.openai.com/codex/quickstart>
    /// - macOS / Linux: `curl -fsSL https://chatgpt.com/codex/install.sh | sh`
    /// - Windows:       `irm https://chatgpt.com/codex/install.ps1 | iex`
    fn for_binary(binary: &str) -> Option<Self> {
        match binary {
            "claude" => Some(Self::claude()),
            "codex" => Some(Self::codex()),
            _ => None,
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

    #[cfg(not(windows))]
    fn codex() -> Self {
        let human = "curl -fsSL https://chatgpt.com/codex/install.sh | sh".to_string();
        Self {
            program: "sh",
            args: vec![
                "-c".to_string(),
                "curl -fsSL https://chatgpt.com/codex/install.sh | sh".to_string(),
            ],
            human,
        }
    }

    #[cfg(windows)]
    fn codex() -> Self {
        let human =
            r#"powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex""#
                .to_string();
        Self {
            program: "powershell",
            args: vec![
                "-ExecutionPolicy".to_string(),
                "ByPass".to_string(),
                "-c".to_string(),
                "irm https://chatgpt.com/codex/install.ps1 | iex".to_string(),
            ],
            human,
        }
    }

    /// The human-readable one-liner shown in prompts and error messages.
    fn display(&self) -> &str {
        &self.human
    }

    /// Execute the installer, inheriting stdio so the user sees its progress.
    /// Errors when the installer exits non-zero.
    async fn run(&self) -> Result<()> {
        let p = Palette::for_stderr();
        eprintln!(
            "{cyan}{bold}info:{reset} installing — {}",
            self.human,
            cyan = p.cyan,
            bold = p.bold,
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
    fn codex_spec_uses_codex_binary() {
        let spec = SpawnAgent::Codex.spec();
        assert_eq!(spec.binary, "codex");
        assert_eq!(spec.id, "codex");
    }

    #[test]
    fn install_command_is_the_official_native_installer() -> Result<()> {
        let cmd = InstallCommand::for_binary("claude").context("claude has a bundled installer")?;
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

        let codex = InstallCommand::for_binary("codex").context("codex has a bundled installer")?;
        assert!(codex.display().contains("chatgpt.com/codex/install"));
        // Other CLIs are installed from upstream.
        assert!(InstallCommand::for_binary("opencode").is_none());
        #[cfg(not(windows))]
        {
            assert!(codex.display().contains("install.sh"));
            assert!(codex.display().contains("| sh"));
        }
        #[cfg(windows)]
        {
            assert!(codex.display().contains("install.ps1"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn missing_native_cli_without_install_returns_official_command() -> Result<()> {
        for agent in SpawnAgent::value_variants() {
            let binary = agent.spec().binary;
            let install = InstallCommand::for_binary(binary).context("bundled installer")?;
            for project_url in [None, Some("https://example.com/upstream")] {
                let error = ensure_installed(binary, project_url, true)
                    .await
                    .err()
                    .context("missing CLI must fail when installation is disabled")?;
                assert_eq!(
                    error.to_string(),
                    format!(
                        "agent '{binary}' is not installed (no `{binary}` on PATH).\n  Install it with:\n    {}",
                        install.display(),
                    ),
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn missing_cli_without_bundled_installer_uses_callers_project_url() -> Result<()> {
        for no_install in [false, true] {
            let error = ensure_installed(
                "other-cli",
                Some("https://example.com/upstream"),
                no_install,
            )
            .await
            .err()
            .context("missing CLI without a bundled installer must fail")?;
            assert_eq!(
                error.to_string(),
                "agent 'other-cli' is not installed (no `other-cli` on PATH).\n  \
                 BitRouter bundles no installer for it — install it from https://example.com/upstream",
            );
        }
        Ok(())
    }

    #[test]
    fn find_on_path_locates_executable_in_listed_dir() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let bin = dir.path().join("fake-agent");
        std::fs::write(&bin, b"#!/bin/sh\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
        }
        let path = std::env::join_paths([dir.path()])?;
        let found = find_on_path("fake-agent", Some(path), &[]);
        assert_eq!(found.as_deref(), Some(bin.as_path()));
        Ok(())
    }

    #[test]
    fn find_on_path_returns_none_when_absent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = std::env::join_paths([dir.path()])?;
        assert!(find_on_path("definitely-not-a-real-binary-xyz", Some(path), &[]).is_none());
        Ok(())
    }

    #[test]
    fn find_on_path_falls_back_to_extra_dirs() -> Result<()> {
        // The post-install re-resolution relies on `extra` (e.g. ~/.local/bin)
        // even when PATH is empty — exercise that path explicitly.
        let dir = tempfile::tempdir()?;
        let bin = dir.path().join("fake-agent");
        std::fs::write(&bin, b"#!/bin/sh\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
        }
        // PATH is None entirely; the binary is only reachable via `extra`.
        let found = find_on_path("fake-agent", None, &[dir.path().to_path_buf()]);
        assert_eq!(found.as_deref(), Some(bin.as_path()));
        Ok(())
    }
}
