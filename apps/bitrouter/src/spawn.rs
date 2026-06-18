//! `bitrouter spawn` — launch a coding-agent harness (Claude Code, …) as a
//! child process with its API base URL pointed at the local BitRouter daemon.
//!
//! The agent's traffic then routes through BitRouter without ever touching the
//! agent's own config files: instead of mutating `~/.claude/config.json` (the
//! "config takeover" model used by some switcher tools — invasive, needs
//! backup/restore and crash recovery), we set `ANTHROPIC_BASE_URL` in the
//! *child process environment only*. Nothing on disk changes, and if BitRouter
//! is down the user simply runs the agent directly.
//!
//! CLI shape follows `cargo run`'s separator convention so there is no
//! ambiguity about which flags belong to which program:
//!
//! ```text
//!   bitrouter spawn --agent claude [bitrouter opts] -- <args forwarded to claude>
//! ```
//!
//! Everything after `--` is handed to the agent binary verbatim.
//!
//! ## Claude Code integration
//!
//! - `ANTHROPIC_BASE_URL` redirects the Anthropic SDK Claude Code uses to an
//!   alternate endpoint. See the Claude Code settings reference:
//!   <https://code.claude.com/docs/en/settings#environment-variables>.
//! - Install commands are the official native installers documented in the
//!   Claude Code quickstart: <https://code.claude.com/docs/en/quickstart>.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::ValueEnum;

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

/// BitRouter's own API-key env var (`brk_…`). When set, we forward it to the
/// agent as the gateway bearer token so the agent authenticates to BitRouter
/// with the user's real credential instead of the placeholder.
const BITROUTER_API_KEY_ENV: &str = "BITROUTER_API_KEY";

/// A sentinel placeholder credential, injected into the agent's environment
/// only when the user has neither set the agent's auth-token var nor exported a
/// `BITROUTER_API_KEY`. It lets the harness start (it refuses to run without
/// *some* credential); under `skip_auth: true` (the `bitrouter init` default)
/// BitRouter ignores the value, and under auth the user is expected to export a
/// real `brk_…` key.
const PLACEHOLDER_API_KEY: &str = "bitrouter-local";

/// Options gathered from the CLI for one `spawn` invocation.
pub struct SpawnOptions {
    /// Which agent to launch.
    pub agent: SpawnAgent,
    /// Arguments forwarded verbatim to the agent binary (everything the
    /// caller put after `--`).
    pub agent_args: Vec<String>,
    /// Explicit base URL override. When `None` it is derived from the
    /// daemon's `server.listen`.
    pub base_url: Option<String>,
    /// When true, never offer to install a missing agent — error instead.
    /// (Set by `--no-install`, or implied when stdin is not a TTY.)
    pub no_install: bool,
}

/// Run `bitrouter spawn`. Resolves the base URL from `cfg`, locates the agent
/// binary (offering to install it if missing and permitted), then execs the
/// agent with the routing environment injected. On success this **does not
/// return** — it exits the process with the agent's exit code, the way a
/// launcher like `git <subcommand>` propagates its child's status.
pub async fn run(cfg: &bitrouter_sdk::config::Config, opts: SpawnOptions) -> Result<()> {
    let spec = opts.agent.spec();

    let base_url = match &opts.base_url {
        Some(explicit) => explicit.clone(),
        None => derive_base_url(&cfg.server.listen),
    };

    // Locate the binary; prompt-to-install when it's missing.
    let binary = match resolve_binary(spec.binary) {
        Some(path) => path,
        None => ensure_installed(&spec, opts.no_install).await?,
    };

    // Best-effort reachability note — never blocks the launch. The agent
    // would fail on its own if the daemon is down; surfacing it up front is
    // friendlier than a wall of HTTP errors inside the agent.
    warn_if_daemon_unreachable(&cfg.server.listen);

    // Auth-token precedence (highest first): an auth token the user already
    // exported for this agent → the BitRouter API key → a local placeholder.
    let parent_auth = nonempty_env(spec.auth_token_env);
    let bitrouter_key = nonempty_env(BITROUTER_API_KEY_ENV);
    let env = build_child_env(&spec, &base_url, parent_auth, bitrouter_key);

    let p = Palette::for_stderr();
    eprintln!(
        "{cyan}{bold}spawn:{reset} launching {bold}{}{reset} via BitRouter ({})",
        spec.id,
        base_url,
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );

    let mut cmd = tokio::process::Command::new(&binary);
    cmd.args(&opts.agent_args);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    // Inherit the parent's stdio so the agent owns the terminal directly
    // (Claude Code is an interactive TUI). Inheritance is the default for
    // `Command`, but we state it for clarity.
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawning agent '{}' ({})", spec.id, binary.display()))?;

    // Propagate the agent's exit code. A launcher should be transparent: the
    // shell sees the agent's status, not bitrouter's.
    std::process::exit(status.code().unwrap_or(1));
}

/// Build the environment overrides layered on top of the inherited parent
/// environment: always force the routing base URL, and resolve the gateway
/// bearer token by precedence.
///
/// Returned as an explicit list (rather than mutating the global env) so the
/// logic is unit-testable. `parent_auth` is any auth token the user already
/// exported for the agent; `bitrouter_key` is the value of `BITROUTER_API_KEY`.
/// Precedence: `parent_auth` → `bitrouter_key` → placeholder. We always set the
/// auth token (never just inherit) so that an inherited `ANTHROPIC_API_KEY`
/// (which in this repo is the *upstream* Anthropic provider key, not a valid
/// BitRouter inbound credential) cannot accidentally become the auth Claude
/// Code sends to the router.
fn build_child_env(
    spec: &AgentSpec,
    base_url: &str,
    parent_auth: Option<String>,
    bitrouter_key: Option<String>,
) -> Vec<(&'static str, String)> {
    let token = parent_auth
        .or(bitrouter_key)
        .unwrap_or_else(|| PLACEHOLDER_API_KEY.to_string());
    vec![
        (spec.base_url_env, base_url.to_string()),
        (spec.auth_token_env, token),
    ]
}

/// Read an environment variable, treating an unset *or empty* value as absent.
fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Derive the client-facing base URL from the daemon's `server.listen`
/// (`host:port`). Wildcard bind addresses are rewritten to loopback because a
/// client cannot *connect* to `0.0.0.0` / `::` — those mean "bind every
/// interface", not "reach me here".
fn derive_base_url(listen: &str) -> String {
    let (host, port) = split_listen(listen);
    format!("http://{}:{}", rewrite_host(host), port)
}

/// The default daemon port (mirrors `ServerConfig::default().listen`). Used
/// when `server.listen` carries a bare host with no `:port`.
const DEFAULT_PORT: &str = "4356";

/// Split a `server.listen` value into `(host, port)`, defaulting the port when
/// absent. Handles bracketed IPv6 (`[::1]:4356`, `[::1]`) so the `rsplit_once`
/// does not mistake a colon *inside* the brackets for the port separator.
fn split_listen(listen: &str) -> (&str, &str) {
    // Bracketed IPv6: the port (if any) follows the closing bracket.
    if listen.starts_with('[') {
        return match listen.rsplit_once("]:") {
            Some((host, port)) => (&listen[..host.len() + 1], port),
            // `[::1]` with no port.
            None => (listen, DEFAULT_PORT),
        };
    }
    match listen.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => (listen, DEFAULT_PORT),
    }
}

/// Rewrite a wildcard bind host to its loopback equivalent for a *client*
/// connection. `0.0.0.0` / empty → `127.0.0.1`; `::` / `[::]` → `[::1]`.
fn rewrite_host(host: &str) -> &str {
    match host {
        "0.0.0.0" | "" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        other => other,
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

/// Best-effort TCP reachability probe against the daemon's listen address.
/// Prints a one-line warning when nothing is listening; never errors.
fn warn_if_daemon_unreachable(listen: &str) {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    // Map the wildcard bind host to loopback for the *connect* attempt, same
    // as the base-URL derivation.
    let (host, port) = split_listen(listen);
    let probe = format!("{}:{}", rewrite_host(host), port);

    let reachable = probe
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .map(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok())
        .unwrap_or(false);

    if !reachable {
        let p = Palette::for_stderr();
        eprintln!(
            "{cyan}note:{reset} no BitRouter daemon appears to be listening on {probe} — \
             start one with `bitrouter start` (the agent will fail to reach it otherwise).",
            cyan = p.cyan,
            reset = p.reset,
        );
    }
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
    fn base_url_rewrites_wildcard_bind_to_loopback() {
        assert_eq!(derive_base_url("0.0.0.0:4356"), "http://127.0.0.1:4356");
        assert_eq!(derive_base_url("[::]:4356"), "http://[::1]:4356");
    }

    #[test]
    fn base_url_preserves_explicit_host() {
        assert_eq!(derive_base_url("127.0.0.1:4356"), "http://127.0.0.1:4356");
        assert_eq!(
            derive_base_url("router.internal:8080"),
            "http://router.internal:8080"
        );
        // A bracketed IPv6 literal keeps its brackets and port.
        assert_eq!(derive_base_url("[::1]:9000"), "http://[::1]:9000");
    }

    #[test]
    fn base_url_defaults_port_when_missing() {
        assert_eq!(derive_base_url("127.0.0.1"), "http://127.0.0.1:4356");
        // Bracketed IPv6 without a port must not split inside the brackets.
        assert_eq!(derive_base_url("[::1]"), "http://[::1]:4356");
    }

    #[test]
    fn child_env_always_sets_base_url() {
        let spec = SpawnAgent::Claude.spec();
        let env = build_child_env(&spec, "http://127.0.0.1:4356", None, None);
        assert!(
            env.iter()
                .any(|(k, v)| *k == "ANTHROPIC_BASE_URL" && v == "http://127.0.0.1:4356")
        );
    }

    /// Helper: pull the resolved auth-token value out of a built env.
    fn auth_token(env: &[(&'static str, String)]) -> Option<String> {
        env.iter()
            .find(|(k, _)| *k == "ANTHROPIC_AUTH_TOKEN")
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn child_env_auth_token_precedence() {
        let spec = SpawnAgent::Claude.spec();

        // User's explicit auth token wins over everything.
        let explicit = build_child_env(
            &spec,
            "http://x:1",
            Some("user-token".into()),
            Some("brk_key".into()),
        );
        assert_eq!(auth_token(&explicit).as_deref(), Some("user-token"));

        // No explicit token → fall back to the BitRouter API key.
        let from_key = build_child_env(&spec, "http://x:1", None, Some("brk_key".into()));
        assert_eq!(auth_token(&from_key).as_deref(), Some("brk_key"));

        // Neither set → the local placeholder so the harness still starts.
        let placeholder = build_child_env(&spec, "http://x:1", None, None);
        assert_eq!(
            auth_token(&placeholder).as_deref(),
            Some(PLACEHOLDER_API_KEY)
        );
    }

    #[test]
    fn child_env_never_inherits_api_key_as_auth() {
        // The auth token is ALWAYS set explicitly, so a stray inherited
        // ANTHROPIC_API_KEY can never silently become the inbound credential.
        let spec = SpawnAgent::Claude.spec();
        let env = build_child_env(&spec, "http://x:1", None, None);
        assert!(env.iter().any(|(k, _)| *k == "ANTHROPIC_AUTH_TOKEN"));
        assert!(env.iter().all(|(k, _)| *k != "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn claude_spec_uses_anthropic_env_vars() {
        let spec = SpawnAgent::Claude.spec();
        assert_eq!(spec.binary, "claude");
        assert_eq!(spec.base_url_env, "ANTHROPIC_BASE_URL");
        assert_eq!(spec.auth_token_env, "ANTHROPIC_AUTH_TOKEN");
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
