//! `bitrouter launch` — launch a coding-agent harness (Claude Code, Codex, …)
//! as an interactive native-TUI child process with its API base URL pointed at
//! the local BitRouter daemon. This is the interactive surface; headless
//! ACP sub-agents are `bitrouter spawn` (see [`crate::acp_cli`]). Both draw
//! their routing knowledge from the shared [`crate::harness`] catalog.
//!
//! The agent's traffic then routes through BitRouter without ever touching the
//! agent's own config files: instead of mutating `~/.claude/config.json` or
//! `~/.codex/config.toml` (the
//! "config takeover" model used by some switcher tools — invasive, needs
//! backup/restore and crash recovery), we set per-process environment variables
//! or one-shot CLI config overrides. Nothing on disk changes, and if BitRouter
//! is down the user simply runs the agent directly.
//!
//! Harnesses that expose neither an env var nor a CLI override (opencode, pi,
//! hermes, openclaw) are routed by *synthesizing* a throwaway config under the
//! repo's `.bitrouter/launch/` and pointing the harness at it with an env var
//! — still never touching the user's own config
//! ([`crate::harness::Harness::launch_overlay`]).
//!
//! CLI shape follows `cargo run`'s separator convention so there is no
//! ambiguity about which flags belong to which program:
//!
//! ```text
//!   bitrouter launch --agent claude [bitrouter opts] -- <args forwarded to claude>
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
//!
//! ## Codex integration
//!
//! - Codex custom providers are configured with `model_providers.<id>` and can
//!   be overridden per invocation with repeated `-c key=value` flags.
//! - Current Codex builds route custom providers through the Responses API, so
//!   the BitRouter provider uses `wire_api = "responses"` and a `/v1` base URL.
//!   See <https://developers.openai.com/codex/config-advanced#custom-model-providers>.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use bitrouter_sdk::error::BitrouterError;
use clap::ValueEnum;
use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;
use crate::style::Palette;

/// The two harnesses BitRouter ships a native installer for. `bitrouter
/// launch --agent` accepts the *whole* catalog (see [`resolve_launch_agent`]);
/// this enum survives only where a bundled installer is implied — the
/// onboarding wizard's `--harness`, `providers login claude-code`, and the
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
            // The gateway env/args this agent needs (ANTHROPIC_BASE_URL /
            // ANTHROPIC_AUTH_TOKEN for Claude, `-c` provider overrides for
            // Codex) live once in the shared `harness` catalog, keyed by the
            // interactive binary — see `run`.
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

/// Resolved, per-agent static facts used by the spawn machinery.
#[derive(Debug, Clone, Copy)]
pub struct AgentSpec {
    /// Catalog id / `--agent` value.
    pub id: &'static str,
    /// Executable name searched for on `PATH`. Also the key into the shared
    /// [`crate::harness`] catalog for this agent's routing knowledge.
    pub binary: &'static str,
}

/// Resolve a `bitrouter launch --agent` value to its catalog harness. Accepts
/// the interactive binary name (`claude`, `codex`, `opencode`, `pi`, `hermes`,
/// `openclaw`, `grok`, `agy`) and the catalog id (`antigravity`, `claude-acp`,
/// …). An unknown value is a caller mistake, so the error is a
/// [`BitrouterError::BadRequest`] — the CLI's error envelope reports it as
/// `kind: "bad_request"`, not `internal` — and lists what is available.
pub fn resolve_launch_agent(id: &str) -> Result<&'static crate::harness::Harness> {
    crate::harness::by_interactive_binary(id)
        .or_else(|| crate::harness::by_id(id).filter(|h| h.interactive_binary.is_some()))
        .ok_or_else(|| {
            let mut available: Vec<&str> = crate::harness::CATALOG
                .iter()
                .filter_map(|h| h.interactive_binary)
                .collect();
            available.sort_unstable();
            anyhow::Error::new(BitrouterError::bad_request(format!(
                "'{id}' is not a launchable harness — available: {}",
                available.join(", ")
            )))
        })
}

/// The `--agent` value a harness is displayed as: its interactive binary
/// (falling back to the catalog id, which cannot happen for a resolved
/// launch target).
fn harness_id(h: &crate::harness::Harness) -> &'static str {
    h.interactive_binary.unwrap_or(h.id)
}

/// What `bitrouter launch` injects into the child process. The env/args come
/// from the shared [`crate::harness`] catalog's [`RoutingOverlay`](crate::harness::RoutingOverlay), so the
/// interactive and ACP facets of a harness route identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildLaunch {
    /// Environment overrides for the child process.
    pub env: Vec<(String, String)>,
    /// Arguments inserted before the user's forwarded args.
    pub args_prefix: Vec<String>,
}

/// Resolve the gateway bearer credential for a `bitrouter launch` child by
/// precedence: a token the user already exported for the harness →
/// `BITROUTER_API_KEY` → a freshly minted per-launch token (valid under
/// `skip_auth: true`, the `bitrouter init` default; the harness merely needs
/// *some* credential to start).
///
/// The last rung used to be a fixed placeholder. Minting a unique token there
/// instead is what makes per-launch spend answerable (#795): `launch` already
/// injects this credential into every routed harness through whatever
/// mechanism that harness has, so it is the one field BitRouter controls
/// uniformly across exactly the set of harnesses that are metered at all —
/// which a custom header would not be (four of eight have no way to send one).
///
/// **A user-supplied credential always wins, and is never tagged.** The two
/// upper rungs are real authentication; rewriting them to attach attribution
/// would break `skip_auth: false` outright. Those launches fall back to the
/// unattributed window summary, and [`print_exit_summary`] says so rather than
/// implying a precision it does not have.
fn resolve_launch_token(parent_auth: Option<String>, bitrouter_key: Option<String>) -> String {
    parent_auth
        .or(bitrouter_key)
        .unwrap_or_else(mint_launch_token)
}

/// A fresh opaque per-launch attribution token.
fn mint_launch_token() -> String {
    format!(
        "{}{}",
        bitrouter_sdk::caller::LAUNCH_TOKEN_PREFIX,
        uuid::Uuid::new_v4().simple()
    )
}

/// Whether a resolved credential is one we minted — i.e. whether this launch's
/// requests will carry its own attribution, or fall back to the window.
fn is_launch_token(token: &str) -> bool {
    bitrouter_sdk::caller::launch_tag(Some(&format!("Bearer {token}"))).is_some()
}

/// Options gathered from the CLI for one `launch` invocation.
pub struct SpawnOptions {
    /// Which harness to launch, already resolved against the shared catalog
    /// (see [`resolve_launch_agent`]).
    pub agent: &'static crate::harness::Harness,
    /// Pin the harness's model. Applied through the harness's own mechanism —
    /// a model env var, a `-c model=` override, the synthesized config's
    /// default, or the harness's native flag for own-auth clients.
    pub model: Option<String>,
    /// Arguments forwarded verbatim to the agent binary (everything the
    /// caller put after `--`).
    pub agent_args: Vec<String>,
    /// Explicit base URL override. When `None` it is derived from the
    /// daemon's `server.listen`.
    pub base_url: Option<String>,
    /// When true, never offer to install a missing agent — error instead.
    /// (Set by `--no-install`, or implied when stdin is not a TTY.)
    pub no_install: bool,
    /// When true, never auto-start a local daemon when none is running — just
    /// warn. (Set by `--no-start`.) Has no effect for a non-local / `--base-url`
    /// target, which is never auto-started regardless.
    pub no_start: bool,
    /// Check the spawn environment and route without launching the agent.
    pub check: bool,
}

/// One preflight check outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnCheckStatus {
    Pass,
    Warn,
    Fail,
}

/// A single preflight row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpawnCheckRow {
    pub name: String,
    pub status: SpawnCheckStatus,
    pub message: String,
}

/// Result of `bitrouter spawn --check`.
#[derive(Debug, Clone, Serialize)]
pub struct SpawnCheckReport {
    pub agent: String,
    pub base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub checks: Vec<SpawnCheckRow>,
}

impl SpawnCheckReport {
    fn has_failures(&self) -> bool {
        self.checks
            .iter()
            .any(|c| matches!(c.status, SpawnCheckStatus::Fail))
    }
}

impl CliReport for SpawnCheckReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!(
            "preflight for {} via {}",
            self.agent, self.base_url
        ))?;
        if let Some(model) = &self.model {
            h.line(&format!("  model: {model}"))?;
        }
        h.blank()?;
        for check in &self.checks {
            h.line(&format!(
                "  {} {}: {}",
                match check.status {
                    SpawnCheckStatus::Pass => "✓",
                    SpawnCheckStatus::Warn => "!",
                    SpawnCheckStatus::Fail => "✗",
                },
                check.name,
                check.message
            ))?;
        }
        Ok(())
    }

    fn exit_code(&self) -> i32 {
        if self.has_failures() { 1 } else { 0 }
    }
}

/// Everything `launch` resolves before a child exists: the binary, the routing
/// overlay, the args forwarded after `--`, and the inputs the exit summary
/// needs.
///
/// This is the seam that keeps the display modes honest. Both
/// [`exec_inherited`] (plain `launch`) and the hosted mode consume one
/// `Prepared` built by one [`prepare`], so "the wrapped mode is a strict
/// superset of `launch` in child behavior" is true **by construction** rather
/// than by test discipline — there is one producer of the child's env and args,
/// and the display choice branches strictly after it.
///
/// `agent_args` lives here rather than being threaded separately precisely
/// because it is part of that guarantee: everything the user typed after `--`
/// must be inside the compared surface, not beside it.
pub struct Prepared<'a> {
    /// Resolved harness binary.
    pub binary: PathBuf,
    /// The routing overlay: env to set and args to prepend.
    pub launch: ChildLaunch,
    /// Args forwarded verbatim, appended after [`ChildLaunch::args_prefix`].
    pub agent_args: Vec<String>,
    /// The gateway base URL the harness was pointed at.
    pub base_url: String,
    /// The catalog harness being launched.
    pub harness: &'static crate::harness::Harness,
    /// Config source, for the exit summary's metering read.
    pub source: &'a crate::paths::ConfigSource,
    /// The attribution token this launch minted, when it owns the credential
    /// slot. `None` when the user supplied their own key — spend then falls
    /// back to the unattributed time window.
    pub launch_id: Option<String>,
    /// The model pinned for this launch, named in the hosted status row.
    pub model: Option<String>,
    /// Stamped before the child exists, so the exit summary's window covers
    /// exactly the wrapped session. Nothing between here and the spawn spends.
    pub session_start: chrono::DateTime<chrono::Utc>,
}

/// Run `bitrouter launch`: [`prepare`] the child, then run it with the
/// terminal inherited. On success this **does not return** — it exits the
/// process with the agent's exit code, the way a launcher like
/// `git <subcommand>` propagates its child's status.
pub async fn run(
    source: &crate::paths::ConfigSource,
    cfg: &bitrouter_sdk::config::Config,
    opts: SpawnOptions,
) -> Result<()> {
    let prepared = prepare(source, cfg, opts).await?;
    exec_inherited(prepared).await
}

/// Resolve the base URL from `cfg`, locate the agent binary (offering to
/// install it if missing and permitted), ensure the local daemon is up
/// (auto-starting it when down), assemble the routing overlay, and state what
/// the harness is actually getting (§796) — everything up to, but not
/// including, the child process.
pub async fn prepare<'a>(
    source: &'a crate::paths::ConfigSource,
    cfg: &bitrouter_sdk::config::Config,
    opts: SpawnOptions,
) -> Result<Prepared<'a>> {
    let harness = opts.agent;

    let base_url = match &opts.base_url {
        Some(explicit) => explicit.clone(),
        None => derive_base_url(&cfg.server.listen),
    };

    if matches!(harness.routing, crate::harness::Routing::CodexArgs) {
        let conflicts = codex_forwarded_config_args(&opts.agent_args);
        if !conflicts.is_empty() {
            anyhow::bail!(
                "codex forwarded config flags ({}) can override BitRouter's one-shot provider \
                 injection. Remove those -c/--config flags and run `bitrouter launch --agent \
                 codex --check` to inspect the route before launching.",
                conflicts.join(", ")
            );
        }
    }

    // Locate the binary; prompt-to-install when it's missing.
    let binary = ensure_harness_installed(harness, opts.no_install).await?;

    // Make sure the daemon the agent will talk to is up. For the local daemon
    // we own (derived base URL + a loopback/wildcard bind), probe its control
    // socket and auto-start it when down; for an explicit `--base-url` or a
    // non-local bind we can only warn — we can't start someone else's daemon.
    if opts.base_url.is_none() && listen_is_local(&cfg.server.listen) {
        ensure_local_daemon(source, cfg, opts.no_start).await;
    } else {
        let target = opts
            .base_url
            .as_deref()
            .and_then(listen_from_base_url)
            .unwrap_or_else(|| cfg.server.listen.clone());
        warn_if_daemon_unreachable(&target);
    }

    // Auth precedence (highest first): a bearer token the user already exported
    // for this harness → the BitRouter API key → a local placeholder. Only
    // env-routed harnesses (Claude) have such a var; Codex reads none.
    let parent_auth = match harness.routing {
        crate::harness::Routing::Env { auth_env, .. } => nonempty_env(auth_env),
        _ => None,
    };
    let token = resolve_launch_token(
        parent_auth,
        nonempty_env(crate::harness::BITROUTER_API_KEY_ENV),
    );
    // The daemon's advertised model ids fill the synthesized providers' model
    // lists — only the config-synthesis harnesses need them, so nothing else
    // pays for the probe. Best-effort: an unreachable daemon yields an empty
    // catalog and the harness keeps its own defaults.
    let catalog = if needs_model_catalog(harness) {
        fetch_daemon_models(&base_url).await
    } else {
        Vec::new()
    };
    let state_dir = launch_state_dir()?;
    let gateways = launch_gateways(cfg, &base_url);
    let overlay = harness
        .launch_overlay(
            &base_url,
            &token,
            opts.model.as_deref(),
            &catalog,
            &gateways,
            &state_dir,
        )
        .with_context(|| format!("assembling the '{}' launch overlay", harness.id))?;
    let launch = ChildLaunch {
        env: overlay.env,
        args_prefix: overlay.args,
    };

    let p = Palette::for_stderr();
    eprintln!("{}", startup_line(harness, &base_url, &gateways, &p));

    Ok(Prepared {
        binary,
        launch,
        agent_args: opts.agent_args,
        base_url,
        harness,
        source,
        launch_id: is_launch_token(&token).then(|| token.clone()),
        model: opts.model.clone(),
        // Timestamp the wrapped session so the exit summary can attribute
        // spend to exactly this run of the agent.
        session_start: chrono::Utc::now(),
    })
}

/// The one line `launch` prints before handing the terminal over (#796):
/// what the harness is actually getting, stated once where it is actionable.
///
/// Degrades **honestly**. A blank field reads as broken; `own-auth · not
/// routed · not metered` reads as true. The tools/skills ceiling is the
/// harness's — `pi` and `openclaw` expose no MCP mechanism to inject into —
/// and a user should learn that here rather than by noticing the tools are
/// missing later.
fn startup_line(
    harness: &crate::harness::Harness,
    base_url: &str,
    gateways: &[crate::harness::McpServer],
    p: &Palette,
) -> String {
    let agent_id = harness_id(harness);
    let head = format!(
        "{cyan}{bold}launch:{reset} {bold}{agent_id}{reset}",
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );

    // Own-auth harnesses are subscription clients whose sessions the daemon
    // itself borrows as providers — routing them through BitRouter would loop
    // back to the same backend on the same credential. Their traffic never
    // reaches the daemon, so there is nothing to meter.
    if matches!(harness.routing, crate::harness::Routing::OwnAuth) {
        return format!("{head} · own-auth · not routed · not metered");
    }

    let mut line = format!("{head} · routed via bitrouter ({base_url})");
    let named = |name: &str| gateways.iter().any(|s| s.name == name);
    if harness.injects_mcp() {
        line.push_str(&format!(
            " · tools {} skills {}",
            mark(named(crate::gateways::TOOLS_SERVER), p),
            mark(named(crate::gateways::SKILLS_SERVER), p),
        ));
    } else {
        line.push_str(&format!(
            " · tools {} skills {} ({agent_id} has no MCP mechanism)",
            mark(false, p),
            mark(false, p),
        ));
    }
    line
}

/// `✓` / `✗`, colored when the stream takes color. A missing capability is
/// dimmed, not red: it is a harness ceiling being reported, not a failure.
fn mark(on: bool, p: &Palette) -> String {
    if on {
        format!("{}✓{}", p.green, p.reset)
    } else {
        format!("{}✗{}", p.dim, p.reset)
    }
}

/// The child's argv tail: the routing prefix first, then everything the user
/// forwarded after `--`.
///
/// Every display mode builds its child from this one function. That is what
/// makes "the hosted mode passes identical args" a structural property rather
/// than a promise policed by tests — there is nowhere else for the argv to be
/// assembled differently.
fn child_args(launch: &ChildLaunch, agent_args: &[String]) -> Vec<String> {
    let mut args = launch.args_prefix.clone();
    args.extend_from_slice(agent_args);
    args
}

/// Env keys the host **sets**, because the emulator's real capabilities differ
/// from the outer terminal's. `TERM` must promise only what we render:
/// passing through `xterm-kitty` invites graphics sequences the emulator
/// cannot draw.
pub const HOSTED_ENV_SET: &[&str] = &["TERM", "COLORTERM"];

/// Env keys `portable-pty` may add when absent in the parent.
pub const HOSTED_ENV_MAY_ADD: &[&str] = &["SHELL"];

/// Env keys the host **unsets**: inherited values that would lie about the
/// terminal the child is actually talking to, and would steer harnesses onto
/// rendering paths the emulator does not implement.
pub const HOSTED_ENV_UNSET: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "WEZTERM_EXECUTABLE",
    "WEZTERM_PANE",
    "WEZTERM_UNIX_SOCKET",
    "ITERM_SESSION_ID",
    "ALACRITTY_SOCKET",
    "ALACRITTY_LOG",
    "ALACRITTY_WINDOW_ID",
    "LINES",
    "COLUMNS",
];

/// The child env for hosted mode: the routing overlay **verbatim**, plus the
/// terminal-identity corrections above.
///
/// The routing half is byte-identical to plain `launch` by construction — it
/// is the same [`ChildLaunch`] from the same [`prepare`]. Only terminal
/// identity may differ, and only by the three lists, which
/// `hosted_env_differs_only_by_the_allowlist` pins.
fn hosted_env(launch: &ChildLaunch) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
    ];
    for key in HOSTED_ENV_UNSET {
        // `CommandBuilder` inherits the parent environment, so a lying value
        // has to be actively overwritten; there is no "unset" in the overlay.
        env.push(((*key).to_string(), String::new()));
    }
    // The overlay lands last so routing always wins over terminal identity.
    env.extend(launch.env.iter().cloned());
    env
}

/// Run the prepared child **hosted** inside BitRouter's emulator, with the
/// status row pinned underneath (`--tui`).
#[cfg(unix)]
pub async fn exec_hosted(prepared: Prepared<'_>) -> Result<()> {
    let Prepared {
        binary,
        launch,
        agent_args,
        harness,
        source,
        session_start,
        launch_id,
        model,
        ..
    } = prepared;

    let socket = {
        let cfg = crate::paths::load_config(source).await?;
        crate::daemon::resolve_socket_path(
            source.home().join("bitrouter.yaml").as_path(),
            &cfg.server.control_socket,
        )
    };
    let ctx = crate::tui::host::HostContext {
        source,
        socket,
        harness,
        launch_id: launch_id.clone(),
        model,
    };
    let code = crate::tui::host::run(
        &binary.display().to_string(),
        &child_args(&launch, &agent_args),
        &hosted_env(&launch),
        ctx,
    )
    .await?;

    print_exit_summary(
        source,
        launch_id.as_deref(),
        session_start,
        &Palette::for_stderr(),
    )
    .await;
    std::process::exit(code);
}

/// Run the prepared child with the terminal **inherited** — the harness owns
/// the real tty, which is exactly what its authors tested against. Does not
/// return: it exits with the child's status.
pub async fn exec_inherited(prepared: Prepared<'_>) -> Result<()> {
    let Prepared {
        binary,
        launch,
        agent_args,
        harness,
        source,
        session_start,
        launch_id,
        ..
    } = prepared;
    let agent_id = harness_id(harness);

    let mut cmd = tokio::process::Command::new(&binary);
    cmd.args(child_args(&launch, &agent_args));
    for (k, v) in &launch.env {
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
        .with_context(|| format!("spawning agent '{agent_id}' ({})", binary.display()))?;

    print_exit_summary(
        source,
        launch_id.as_deref(),
        session_start,
        &Palette::for_stderr(),
    )
    .await;

    // Propagate the agent's exit code. A launcher should be transparent: the
    // shell sees the agent's status, not bitrouter's.
    std::process::exit(status.code().unwrap_or(1));
}

/// The cost-feed exit renderer: after the wrapped agent exits, report
/// what the session spent through the local daemon. Silent when the
/// metering database is absent or recorded nothing in the window (e.g.
/// a `--base-url` pointed at Cloud) — a launcher must never turn a
/// clean exit into noise or an error. Printed to stderr like every
/// other spawn diagnostic; stdout belongs to the child.
///
/// With `launch_id` the figure is **this launch's**, even with other agents
/// running concurrently. Without one — the user supplied their own gateway
/// credential, so nothing we minted rode along — it falls back to the time
/// window, which is every caller's traffic during the session, and the line
/// says so. The old wording called that "session spend" unconditionally; with
/// two agents running it was quietly wrong.
async fn print_exit_summary(
    source: &crate::paths::ConfigSource,
    launch_id: Option<&str>,
    session_start: chrono::DateTime<chrono::Utc>,
    p: &Palette,
) {
    use crate::metering::store::TimeWindow;
    let Some(store) = crate::metering::reader::open_readonly(source).await else {
        return;
    };
    let window = TimeWindow::Custom {
        start: session_start,
        end: chrono::Utc::now(),
    };
    let session = match launch_id {
        Some(id) => store.spend_summary_for_launch(id, window).await,
        None => store.spend_summary(window).await,
    };
    let (Ok(session), Ok(today)) = (session, store.spend_summary(TimeWindow::Today).await) else {
        return;
    };
    if session.requests == 0 {
        return;
    }
    let scope = if launch_id.is_some() {
        "session spend"
    } else {
        "spend since launch (all callers)"
    };
    eprintln!(
        "{cyan}{bold}launch:{reset} {scope} {bold}{}{reset} ({} requests) · today {}",
        crate::metering::fmt_usd(session.spend_micro_usd),
        session.requests,
        crate::metering::fmt_usd(today.spend_micro_usd),
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );
}

/// The gateway MCP servers a launched harness gets: the daemon's aggregate MCP
/// endpoint (`bitrouter_tools`, only when `mcp.aggregate` is enabled) and the
/// origin AgentSkills server (`bitrouter_skills`). See [`crate::gateways`].
///
/// Injection reaches only the harnesses that have a mechanism for it — claude
/// (`--mcp-config`), codex (`-c mcp_servers.*`), opencode and hermes (their
/// synthesized config files). `pi`, `openclaw`, `grok`, and `antigravity` have
/// no injectable MCP surface and ignore these servers; that ceiling is the
/// harnesses', not BitRouter's.
fn launch_gateways(
    cfg: &bitrouter_sdk::config::Config,
    base_url: &str,
) -> Vec<crate::harness::McpServer> {
    let auth = crate::harness::resolve_gateway_auth(
        std::env::var(crate::harness::BITROUTER_API_KEY_ENV).ok(),
        false,
    )
    .unwrap_or_else(|| crate::harness::PLACEHOLDER_API_KEY.to_string());
    let aggregate_route = cfg
        .mcp
        .aggregate
        .enabled
        .then(|| cfg.mcp.aggregate.route.clone());
    crate::gateways::gateway_servers(base_url, &auth, aggregate_route.as_deref())
}

/// Whether this harness's overlay needs the daemon's advertised model ids —
/// true exactly for the config-synthesis harnesses, whose synthesized provider
/// carries an explicit model list. Env/args and own-auth harnesses ignore it,
/// so they never pay for the `/v1/models` probe.
fn needs_model_catalog(h: &crate::harness::Harness) -> bool {
    matches!(
        h.routing,
        crate::harness::Routing::OpencodeConfig
            | crate::harness::Routing::PiConfigDir
            | crate::harness::Routing::HermesHome
            | crate::harness::Routing::OpenclawProfile
    )
}

/// Where `launch` writes the throwaway configs it synthesizes for the harnesses
/// env/args cannot route: `<cwd>/.bitrouter/launch/`. The `.bitrouter` dir
/// self-ignores, so nothing synthesized here ever lands in a diff. Only the
/// config-synthesis harnesses actually create anything under it.
fn launch_state_dir() -> Result<PathBuf> {
    let dot_dir = std::env::current_dir()
        .context("resolving the working directory for synthesized harness config")?
        .join(".bitrouter");
    // Best-effort — a missing/unwritable ignore file must not block a launch.
    let _ = ensure_self_ignored(&dot_dir);
    Ok(dot_dir.join("launch"))
}

/// Create `dot_dir` (and parents) if needed and drop a self-ignoring
/// `.gitignore` into it unless one already exists.
///
/// Nothing under `<repo>/.bitrouter/` belongs in version control, so the
/// directory is created with a `.gitignore` containing `*` — the same trick
/// cargo uses for `target/` — instead of trusting every repo to ignore it. A
/// `.gitignore` a user wrote themselves is never overwritten.
fn ensure_self_ignored(dot_dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dot_dir)?;
    let gitignore = dot_dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n")?;
    }
    Ok(())
}

/// Best-effort fetch of the daemon's advertised model ids (`GET /v1/models`),
/// which fill the synthesized providers' model lists so the harness's own
/// model picker shows what the daemon can serve. Empty when the daemon is
/// unreachable or answers something unexpected — the harness then keeps its
/// own defaults, and the caller has already warned about an unreachable
/// daemon.
async fn fetch_daemon_models(base_url: &str) -> Vec<String> {
    let url = format!("{}/v1/models", bitrouter_root_url(base_url));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };
    let Ok(resp) = client.get(&url).send().await else {
        return Vec::new();
    };
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    body["data"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Check a launch invocation without launching the child process.
pub async fn check(
    cfg: &bitrouter_sdk::config::Config,
    opts: &SpawnOptions,
) -> Result<SpawnCheckReport> {
    let harness = opts.agent;
    let agent_id = harness_id(harness);
    let binary = harness.interactive_binary.unwrap_or(harness.id);
    let is_codex = matches!(harness.routing, crate::harness::Routing::CodexArgs);
    let base_url = opts
        .base_url
        .clone()
        .unwrap_or_else(|| derive_base_url(&cfg.server.listen));
    // `--model` wins; codex also accepts a forwarded `--model/-m`.
    let model = opts.model.clone().or_else(|| {
        is_codex
            .then(|| codex_requested_model(&opts.agent_args))
            .flatten()
    });
    let mut checks = Vec::new();

    checks.push(match resolve_binary(binary) {
        Some(path) => SpawnCheckRow {
            name: "agent binary".to_string(),
            status: SpawnCheckStatus::Pass,
            message: format!("found {}", path.display()),
        },
        None => SpawnCheckRow {
            name: "agent binary".to_string(),
            status: SpawnCheckStatus::Fail,
            message: format!("{binary} is not on PATH"),
        },
    });

    // How this harness reaches the gateway — env/args, a synthesized config
    // file, or (own-auth) not at all.
    checks.push(SpawnCheckRow {
        name: "routing".to_string(),
        status: if matches!(harness.routing, crate::harness::Routing::OwnAuth) {
            SpawnCheckStatus::Warn
        } else {
            SpawnCheckStatus::Pass
        },
        message: match harness.routing {
            crate::harness::Routing::OwnAuth => format!(
                "'{}' is a subscription client the daemon borrows sessions from — it launches \
                 with its own auth, never routed through BitRouter",
                harness.id
            ),
            _ if harness.env_args_routable() => {
                format!("via daemon {base_url} [{}] by env/args", harness.id)
            }
            _ => format!(
                "via daemon {base_url} [{}] by config synthesized under .bitrouter/launch",
                harness.id
            ),
        },
    });

    checks.push(check_base_url(&base_url).await);

    if is_codex {
        let conflicts = codex_forwarded_config_args(&opts.agent_args);
        checks.push(if conflicts.is_empty() {
            SpawnCheckRow {
                name: "codex config overrides".to_string(),
                status: SpawnCheckStatus::Pass,
                message: "no forwarded -c/--config flags detected".to_string(),
            }
        } else {
            SpawnCheckRow {
                name: "codex config overrides".to_string(),
                status: SpawnCheckStatus::Fail,
                message: format!(
                    "forwarded {} can override BitRouter's provider injection",
                    conflicts.join(", ")
                ),
            }
        });

        checks.push(match &model {
            Some(model) => {
                codex_route_check(model, crate::commands::resolve_route(cfg, model).await)
            }
            None => SpawnCheckRow {
                name: "codex model route".to_string(),
                status: SpawnCheckStatus::Warn,
                message: "no --model given or forwarded; Codex will choose its default model"
                    .to_string(),
            },
        });
    }

    Ok(SpawnCheckReport {
        agent: agent_id.to_string(),
        base_url,
        model,
        checks,
    })
}

async fn check_base_url(base_url: &str) -> SpawnCheckRow {
    let health = format!("{}/health", bitrouter_root_url(base_url));
    match base_url_health(base_url).await {
        HealthProbe::Ok(status) => SpawnCheckRow {
            name: "bitrouter base url".to_string(),
            status: SpawnCheckStatus::Pass,
            message: format!("{health} responded {status}"),
        },
        HealthProbe::BadStatus(status) => SpawnCheckRow {
            name: "bitrouter base url".to_string(),
            status: SpawnCheckStatus::Fail,
            message: format!("{health} responded {status}"),
        },
        HealthProbe::Unreachable(e) => SpawnCheckRow {
            name: "bitrouter base url".to_string(),
            status: SpawnCheckStatus::Fail,
            message: format!("could not reach {health}: {e}"),
        },
    }
}

/// Outcome of a `GET {root}/health` probe.
enum HealthProbe {
    /// 2xx — the daemon is up.
    Ok(reqwest::StatusCode),
    /// Reached the daemon but it answered non-2xx.
    BadStatus(reqwest::StatusCode),
    /// Could not reach the endpoint at all.
    Unreachable(String),
}

/// Probe the daemon's `/health` endpoint behind `base_url`. Shared by
/// `spawn --check` and the ACP spawn path's fail-fast liveness gate.
async fn base_url_health(base_url: &str) -> HealthProbe {
    let health = format!("{}/health", bitrouter_root_url(base_url));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(e) => return HealthProbe::Unreachable(format!("could not build HTTP client: {e}")),
    };
    match client.get(&health).send().await {
        Ok(resp) if resp.status().is_success() => HealthProbe::Ok(resp.status()),
        Ok(resp) => HealthProbe::BadStatus(resp.status()),
        Err(e) => HealthProbe::Unreachable(e.to_string()),
    }
}

/// True when the daemon behind `base_url` answers `GET /health` with 2xx.
/// Used by the ACP spawn path to fail fast before any session side effect.
pub(crate) async fn base_url_reachable(base_url: &str) -> bool {
    matches!(base_url_health(base_url).await, HealthProbe::Ok(_))
}

fn bitrouter_root_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

fn codex_requested_model(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--model" || arg == "-m" {
            return iter.next().filter(|v| !v.is_empty()).cloned();
        }
        if let Some(value) = arg.strip_prefix("--model=")
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }
    None
}

fn codex_forwarded_config_args(args: &[String]) -> Vec<&'static str> {
    args.iter()
        .filter_map(|arg| match arg.as_str() {
            "-c" => Some("-c"),
            "--config" => Some("--config"),
            s if s.starts_with("--config=") => Some("--config"),
            _ => None,
        })
        .collect()
}

fn codex_route_check(model: &str, route: Result<Vec<crate::daemon::RouteHop>>) -> SpawnCheckRow {
    match route {
        Ok(chain) if chain.is_empty() => SpawnCheckRow {
            name: "codex model route".to_string(),
            status: SpawnCheckStatus::Fail,
            message: format!("{model} resolved to an empty route chain"),
        },
        Ok(chain) => {
            let providers = chain
                .iter()
                .map(|hop| format!("{}:{} ({})", hop.provider, hop.service_id, hop.api_protocol))
                .collect::<Vec<_>>()
                .join(" -> ");
            if chain
                .iter()
                .any(|hop| hop.api_protocol.eq_ignore_ascii_case("responses"))
            {
                SpawnCheckRow {
                    name: "codex model route".to_string(),
                    status: SpawnCheckStatus::Pass,
                    message: format!("{model} can route through Responses: {providers}"),
                }
            } else {
                SpawnCheckRow {
                    name: "codex model route".to_string(),
                    status: SpawnCheckStatus::Fail,
                    message: format!(
                        "{model} has no responses-compatible endpoint for Codex: {providers}"
                    ),
                }
            }
        }
        Err(e) => SpawnCheckRow {
            name: "codex model route".to_string(),
            status: SpawnCheckStatus::Fail,
            message: format!("could not resolve {model}: {e:#}"),
        },
    }
}

/// Read an environment variable, treating an unset *or empty* value as absent.
pub(crate) fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Derive the client-facing base URL from the daemon's `server.listen`
/// (`host:port`). Wildcard bind addresses are rewritten to loopback because a
/// client cannot *connect* to `0.0.0.0` / `::` — those mean "bind every
/// interface", not "reach me here".
pub(crate) fn derive_base_url(listen: &str) -> String {
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

/// True when `listen` binds a loopback / wildcard address — i.e. a daemon on
/// *this* host that `bitrouter spawn` may auto-start. A remote or LAN host is
/// someone else's daemon, which we can only warn about. Exact-match only:
/// `127.0.0.0/8` aliases (e.g. `127.0.0.2`) and IPv4-mapped IPv6 fall through to
/// the warn path — the fail-safe direction (never a wrong auto-start).
pub(crate) fn listen_is_local(listen: &str) -> bool {
    let (host, _port) = split_listen(listen);
    matches!(
        host,
        "127.0.0.1" | "0.0.0.0" | "" | "::1" | "[::1]" | "::" | "[::]" | "localhost"
    )
}

/// Extract the `host[:port]` authority from a base URL for a best-effort
/// reachability note. Returns `None` when there is no authority to probe.
pub(crate) fn listen_from_base_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest);
    (!authority.is_empty()).then(|| authority.to_string())
}

/// Ensure the local BitRouter daemon is up before launching the agent. Probes
/// the control socket; when nothing is listening (and `--no-start` was not
/// given) it prints a hint and auto-starts a detached `serve`, waiting for
/// readiness. Best-effort throughout: on any failure it warns and returns so
/// the agent still launches (and surfaces its own connection error) — matching
/// spawn's "never block the launch" stance.
pub(crate) async fn ensure_local_daemon(
    source: &crate::paths::ConfigSource,
    cfg: &bitrouter_sdk::config::Config,
    no_start: bool,
) {
    let socket = crate::daemon::socket_path_for(source, cfg);
    match crate::daemon::probe_status(&socket).await {
        // Already running — nothing to do.
        Ok(Some(_)) => {}
        // Definitively not reachable — auto-start unless opted out.
        Ok(None) => {
            let p = Palette::for_stderr();
            if no_start {
                warn_if_daemon_unreachable(&cfg.server.listen);
                return;
            }
            eprintln!(
                "{cyan}note:{reset} no BitRouter daemon is running — starting one…",
                cyan = p.cyan,
                reset = p.reset,
            );
            let log_path = source.home().join("bitrouter.log");
            match crate::daemon::start_and_wait(
                source,
                &log_path,
                Some(&socket),
                crate::daemon::DAEMON_READY_TIMEOUT,
            )
            .await
            {
                Ok(crate::daemon::DaemonStartOutcome::Ready(info)) => {
                    eprintln!(
                        "{cyan}note:{reset} BitRouter daemon ready (pid {})",
                        info.pid,
                        cyan = p.cyan,
                        reset = p.reset,
                    );
                }
                Ok(crate::daemon::DaemonStartOutcome::NotReadyInTime { pid }) => {
                    eprintln!(
                        "{cyan}note:{reset} daemon started (pid {pid}) but is not ready yet — \
                         the agent may need a moment; logs at {}",
                        log_path.display(),
                        cyan = p.cyan,
                        reset = p.reset,
                    );
                }
                Ok(crate::daemon::DaemonStartOutcome::Exited { status, log_tail }) => {
                    eprintln!(
                        "{cyan}note:{reset} daemon exited during startup ({status}) — \
                         launching the agent anyway",
                        cyan = p.cyan,
                        reset = p.reset,
                    );
                    crate::daemon::eprint_failure_log(&log_path, &log_tail);
                }
                Err(e) => {
                    eprintln!(
                        "{cyan}note:{reset} could not start the daemon ({e:#}) — \
                         launching the agent anyway",
                        cyan = p.cyan,
                        reset = p.reset,
                    );
                }
            }
        }
        // Reachable but the exchange errored — assume it's up; don't double-start.
        Err(e) => {
            tracing::debug!(error = %e, "daemon status probe errored; assuming up");
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
/// and return its path. Shared by `bitrouter spawn` and
/// `bitrouter providers login claude-code` (which needs the `claude` CLI to
/// sign the user in) so both go through one detect-and-install path.
pub(crate) async fn ensure_agent_installed(agent: SpawnAgent, no_install: bool) -> Result<PathBuf> {
    let spec = agent.spec();
    let harness = crate::harness::by_interactive_binary(spec.binary).ok_or_else(|| {
        anyhow::anyhow!(
            "no catalog harness for interactive binary '{}'",
            spec.binary
        )
    })?;
    ensure_harness_installed(harness, no_install).await
}

/// Locate a catalog harness's interactive binary, offering the official native
/// installer when BitRouter bundles one for it (claude, codex) and the caller
/// permits prompting. Harnesses with no bundled installer get an actionable
/// error pointing at their upstream project.
pub(crate) async fn ensure_harness_installed(
    harness: &'static crate::harness::Harness,
    no_install: bool,
) -> Result<PathBuf> {
    let binary = harness
        .interactive_binary
        .ok_or_else(|| anyhow::anyhow!("harness '{}' has no interactive binary", harness.id))?;
    match resolve_binary(binary) {
        Some(path) => Ok(path),
        None => ensure_installed(harness, binary, no_install).await,
    }
}

/// The agent binary is missing. Offer to install it via the official native
/// installer when stdin is interactive and `--no-install` was not set;
/// otherwise return an actionable error listing the install command.
async fn ensure_installed(
    harness: &'static crate::harness::Harness,
    binary: &'static str,
    no_install: bool,
) -> Result<PathBuf> {
    let Some(install) = InstallCommand::for_binary(binary) else {
        anyhow::bail!(
            "agent '{binary}' is not installed (no `{binary}` on PATH).\n  \
             BitRouter bundles no installer for it — install it from {}",
            harness.project_url,
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
    /// The official native installer for `binary` on the *current* platform,
    /// or `None` when BitRouter bundles no installer for that harness (the
    /// catalog's other interactive binaries — opencode, pi, hermes, openclaw,
    /// grok, agy — which the user installs from upstream).
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
    pub fn for_binary(binary: &str) -> Option<Self> {
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
    pub fn display(&self) -> &str {
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
    fn dot_dir_is_created_self_ignoring() {
        let base = tempfile::tempdir().expect("tempdir");
        let dot = base.path().join(".bitrouter");
        ensure_self_ignored(&dot).expect("ensure");
        assert_eq!(
            std::fs::read_to_string(dot.join(".gitignore")).expect("read"),
            "*\n"
        );
    }

    #[test]
    fn dot_dir_never_overwrites_an_existing_gitignore() {
        let base = tempfile::tempdir().expect("tempdir");
        let dot = base.path().join(".bitrouter");
        std::fs::create_dir_all(&dot).expect("mkdir");
        std::fs::write(dot.join(".gitignore"), "launch/\n").expect("write");
        ensure_self_ignored(&dot).expect("ensure");
        assert_eq!(
            std::fs::read_to_string(dot.join(".gitignore")).expect("read"),
            "launch/\n",
            "a user-authored ignore file is preserved"
        );
    }

    /// The gateway servers `launch` builds from a default config, and their
    /// arrival in each injectable harness's synthesized surface.
    #[test]
    fn child_args_put_the_routing_prefix_before_forwarded_args() {
        // Order is load-bearing: the harness's own flags must be able to
        // override BitRouter's prefix, never the other way round.
        let launch = ChildLaunch {
            env: vec![],
            args_prefix: vec!["-c".into(), "model=x".into()],
        };
        let forwarded = vec!["--search".to_string(), "-m".to_string(), "mine".to_string()];
        assert_eq!(
            child_args(&launch, &forwarded),
            ["-c", "model=x", "--search", "-m", "mine"]
        );
        // Nothing is dropped when there is no prefix, and nothing is invented
        // when there is nothing forwarded.
        assert_eq!(
            child_args(
                &ChildLaunch {
                    env: vec![],
                    args_prefix: vec![]
                },
                &forwarded
            ),
            forwarded
        );
        assert!(
            child_args(
                &ChildLaunch {
                    env: vec![],
                    args_prefix: vec![]
                },
                &[]
            )
            .is_empty()
        );
    }

    #[test]
    fn startup_line_states_routing_and_tool_availability() {
        let p = Palette::none();
        let cfg = bitrouter_sdk::config::Config::default();
        let gateways = launch_gateways(&cfg, "http://127.0.0.1:4356");
        let line = |id: &str| {
            startup_line(
                crate::harness::by_id(id).expect("catalog"),
                "http://127.0.0.1:4356",
                &gateways,
                &p,
            )
        };

        // Fully-capable harness: routed, both gateways land.
        let claude = line("claude-acp");
        assert!(claude.contains("routed via bitrouter"), "{claude}");
        assert!(claude.contains("tools ✓ skills ✓"), "{claude}");

        // Routed, but the harness has nowhere to put MCP servers. The reason
        // is stated — a bare ✗ would read as a BitRouter failure.
        let pi = line("pi-acp");
        assert!(pi.contains("routed via bitrouter"), "{pi}");
        assert!(pi.contains("tools ✗ skills ✗"), "{pi}");
        assert!(pi.contains("no MCP mechanism"), "{pi}");

        // Own-auth: degrade honestly rather than showing blanks.
        let grok = line("grok");
        assert!(
            grok.contains("own-auth · not routed · not metered"),
            "{grok}"
        );
        assert!(
            !grok.contains("tools"),
            "an unrouted harness must not advertise gateways at all: {grok}"
        );
    }

    #[test]
    fn startup_line_reports_a_disabled_aggregate_as_a_missing_tools_gateway() {
        // `mcp.aggregate` off means `bitrouter_tools` is never injected. The
        // line must say so rather than claiming tools the harness never got.
        let p = Palette::none();
        let skills_only = vec![crate::harness::McpServer {
            name: crate::gateways::SKILLS_SERVER.to_string(),
            transport: crate::harness::McpTransport::Stdio {
                command: "bitrouter".into(),
                args: vec![],
            },
        }];
        let line = startup_line(
            crate::harness::by_id("claude-acp").expect("catalog"),
            "http://127.0.0.1:4356",
            &skills_only,
            &p,
        );
        assert!(line.contains("tools ✗ skills ✓"), "{line}");
    }

    #[test]
    fn launch_wires_the_tools_and_skills_gateways_into_the_overlay() {
        let cfg = bitrouter_sdk::config::Config::default();
        let servers = launch_gateways(&cfg, "http://127.0.0.1:4356");
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["bitrouter_tools", "bitrouter_skills"],
            "the aggregate endpoint is on by default, so both gateways ride"
        );
        // The fleet bridge is gone: `launch` must never inject it.
        assert!(!names.contains(&"bitrouter_fleet"));

        // Every harness with an injectable MCP mechanism must actually carry
        // the servers through synthesis. The catalog's other arms (pi,
        // openclaw, grok, antigravity) have no mechanism and are excluded by
        // design.
        for id in ["claude-acp", "codex-acp", "opencode", "hermes-acp"] {
            let h = crate::harness::by_id(id).expect("catalog entry");
            let dir = tempfile::tempdir().expect("tempdir");
            let overlay = h
                .launch_overlay(
                    "http://127.0.0.1:4356",
                    "tok",
                    None,
                    &[],
                    &servers,
                    dir.path(),
                )
                .expect("overlay");
            // Injection surfaces either directly in the args (codex's
            // `-c mcp_servers.*`) or inside a synthesized file that an arg
            // (claude's `--mcp-config <path>`) or env var (opencode's
            // `OPENCODE_CONFIG`, hermes's `HERMES_HOME`) points at. Read every
            // path either side mentions and search the union.
            let mut rendered = overlay.args.join(" ");
            let paths = overlay
                .args
                .iter()
                .chain(overlay.env.iter().map(|(_, v)| v));
            for path in paths {
                let path = std::path::Path::new(path);
                for candidate in [path.to_path_buf(), path.join("config.yaml")] {
                    rendered.push_str(&std::fs::read_to_string(candidate).unwrap_or_default());
                }
            }
            for name in ["bitrouter_tools", "bitrouter_skills"] {
                assert!(
                    rendered.contains(name),
                    "{id}: gateway `{name}` never reached the harness"
                );
            }
        }
    }

    #[test]
    fn launch_gateways_drops_tools_when_the_aggregate_is_disabled() {
        let mut cfg = bitrouter_sdk::config::Config::default();
        cfg.mcp.aggregate.enabled = false;
        let servers = launch_gateways(&cfg, "http://127.0.0.1:4356");
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["bitrouter_skills"]);
    }

    #[test]
    fn listen_is_local_classifies_loopback_and_wildcard() {
        for local in [
            "127.0.0.1:4356",
            "0.0.0.0:4356",
            "[::1]:4356",
            "[::]:4356",
            "localhost:4356",
            "127.0.0.1",
        ] {
            assert!(listen_is_local(local), "{local} should be local");
        }
        for remote in [
            "router.internal:8080",
            "192.168.1.5:4356",
            "10.0.0.3:4356",
            "example.com:443",
        ] {
            assert!(!listen_is_local(remote), "{remote} should be remote");
        }
    }

    #[test]
    fn listen_from_base_url_extracts_authority() {
        assert_eq!(
            listen_from_base_url("http://127.0.0.1:4356").as_deref(),
            Some("127.0.0.1:4356")
        );
        assert_eq!(
            listen_from_base_url("https://router.example.com/v1").as_deref(),
            Some("router.example.com")
        );
        // No scheme → treated as a bare authority.
        assert_eq!(
            listen_from_base_url("127.0.0.1:4356").as_deref(),
            Some("127.0.0.1:4356")
        );
        assert_eq!(listen_from_base_url(""), None);
    }

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
    fn launch_token_precedence() {
        // User's exported token wins over everything.
        assert_eq!(
            resolve_launch_token(Some("user-token".into()), Some("brk_key".into())),
            "user-token"
        );
        // No exported token → fall back to the BitRouter API key.
        assert_eq!(
            resolve_launch_token(None, Some("brk_key".into())),
            "brk_key"
        );
        // Neither set → a freshly minted, per-launch attribution token, so
        // the harness still starts *and* its spend is answerable.
        let minted = resolve_launch_token(None, None);
        assert!(is_launch_token(&minted), "{minted}");
    }

    #[test]
    fn hosted_env_differs_only_by_the_allowlist() {
        // The requirement `--tui` lives or dies on: the routing half of the
        // child env is byte-identical to plain `launch`. Only terminal
        // identity may differ, and only by the documented lists — anything
        // else means the hosted child is being routed differently, which is
        // the drift the whole `prepare`/`exec` seam exists to prevent.
        let launch = ChildLaunch {
            env: vec![
                ("ANTHROPIC_BASE_URL".to_string(), "http://x:1".to_string()),
                ("ANTHROPIC_AUTH_TOKEN".to_string(), "brl_tok".to_string()),
            ],
            args_prefix: vec![],
        };
        let hosted = hosted_env(&launch);

        // Every routing pair survives verbatim.
        for (key, value) in &launch.env {
            assert!(
                hosted.iter().any(|(k, v)| k == key && v == value),
                "{key} must reach the hosted child unchanged"
            );
        }

        // And nothing outside the allowlist was invented.
        let allowed: Vec<&str> = HOSTED_ENV_SET
            .iter()
            .chain(HOSTED_ENV_MAY_ADD)
            .chain(HOSTED_ENV_UNSET)
            .copied()
            .collect();
        for (key, _) in &hosted {
            let from_overlay = launch.env.iter().any(|(k, _)| k == key);
            assert!(
                from_overlay || allowed.contains(&key.as_str()),
                "hosted mode set `{key}`, which is neither routing nor in the allowlist"
            );
        }
    }

    #[test]
    fn the_routing_overlay_outranks_terminal_identity() {
        // A harness that legitimately wants its own TERM (or any key we also
        // touch) must win: routing is the contract, terminal identity is our
        // correction to it.
        let launch = ChildLaunch {
            env: vec![("TERM".to_string(), "harness-choice".to_string())],
            args_prefix: vec![],
        };
        let hosted = hosted_env(&launch);
        let last_term = hosted
            .iter()
            .rfind(|(k, _)| k == "TERM")
            .map(|(_, v)| v.as_str());
        assert_eq!(last_term, Some("harness-choice"));
    }

    #[test]
    fn only_a_minted_credential_is_treated_as_attribution() {
        // A user's own key must never be re-read as a launch tag: it is real
        // authentication, and mislabelling it would both break `skip_auth:
        // false` and attribute someone else's traffic to this launch.
        assert!(!is_launch_token("user-token"));
        assert!(!is_launch_token("brk_key"));
        assert!(!is_launch_token(crate::harness::PLACEHOLDER_API_KEY));
        assert!(is_launch_token(&mint_launch_token()));
    }

    #[test]
    fn every_launch_mints_a_distinct_token() {
        // Two concurrent launches sharing a token would merge into one
        // bucket — the exact failure per-launch attribution exists to fix.
        let a = mint_launch_token();
        let b = mint_launch_token();
        assert_ne!(a, b);
    }

    #[test]
    fn launch_agents_map_to_catalog_harnesses() {
        // The interactive binary is the key into the shared routing catalog;
        // the overlay content itself is covered by `harness` tests.
        let claude = crate::harness::by_interactive_binary(SpawnAgent::Claude.spec().binary)
            .expect("claude maps to a catalog harness");
        assert_eq!(claude.id, "claude-acp");
        assert!(matches!(
            claude.routing,
            crate::harness::Routing::Env {
                base_url_env: "ANTHROPIC_BASE_URL",
                auth_env: "ANTHROPIC_AUTH_TOKEN",
                ..
            }
        ));

        let codex = crate::harness::by_interactive_binary(SpawnAgent::Codex.spec().binary)
            .expect("codex maps to a catalog harness");
        assert_eq!(codex.id, "codex-acp");
        assert!(matches!(codex.routing, crate::harness::Routing::CodexArgs));
    }

    #[test]
    fn resolve_launch_agent_accepts_every_interactive_catalog_harness() {
        // `launch --agent` takes every catalog harness with an interactive
        // binary, by binary name…
        for h in crate::harness::CATALOG {
            let Some(binary) = h.interactive_binary else {
                continue;
            };
            let resolved = resolve_launch_agent(binary).expect("resolves by binary");
            assert_eq!(resolved.id, h.id, "{binary}");
            // …and by catalog id (`antigravity` → `agy`).
            let by_id = resolve_launch_agent(h.id).expect("resolves by catalog id");
            assert_eq!(by_id.id, h.id);
        }
        // The full expected surface, spelled out so a catalog change is a
        // deliberate CLI change.
        let mut accepted: Vec<&str> = crate::harness::CATALOG
            .iter()
            .filter_map(|h| h.interactive_binary)
            .collect();
        accepted.sort_unstable();
        assert_eq!(
            accepted,
            vec![
                "agy", "claude", "codex", "grok", "hermes", "openclaw", "opencode", "pi"
            ]
        );
    }

    #[test]
    fn resolve_launch_agent_rejects_adapter_only_and_unknown_ids() {
        // gemini-cli has no interactive binary — it is not launchable.
        let err = resolve_launch_agent("gemini-cli").expect_err("not launchable");
        assert!(err.to_string().contains("gemini-cli"));

        let err = resolve_launch_agent("nope").expect_err("unknown id");
        let msg = err.to_string();
        assert!(msg.contains("is not a launchable harness"), "{msg}");
        // The message lists the fix.
        for id in [
            "claude", "codex", "opencode", "pi", "hermes", "openclaw", "grok", "agy",
        ] {
            assert!(msg.contains(id), "{msg} should list {id}");
        }
        // A typo is the caller's mistake, not a BitRouter fault: the error
        // envelope must report `bad_request`, never `internal`.
        for bad in ["gemini-cli", "nope"] {
            let err = resolve_launch_agent(bad).expect_err("rejected");
            assert_eq!(
                crate::output::error::envelope_from_anyhow(&err).error.kind,
                bitrouter_sdk::error::ErrorKind::BadRequest,
                "{bad}"
            );
        }
    }

    #[test]
    fn needs_model_catalog_only_for_config_synthesis_harnesses() {
        for id in ["opencode", "pi-acp", "hermes-acp", "openclaw"] {
            let h = crate::harness::by_id(id).expect("catalog harness");
            assert!(needs_model_catalog(h), "{id} synthesizes a model list");
        }
        for id in ["claude-acp", "codex-acp", "grok", "antigravity"] {
            let h = crate::harness::by_id(id).expect("catalog harness");
            assert!(!needs_model_catalog(h), "{id} needs no /v1/models probe");
        }
    }

    #[test]
    fn codex_spec_uses_codex_binary() {
        let spec = SpawnAgent::Codex.spec();
        assert_eq!(spec.binary, "codex");
        assert_eq!(spec.id, "codex");
    }

    #[test]
    fn codex_requested_model_reads_forwarded_model_args() {
        assert_eq!(
            codex_requested_model(&[
                "exec".to_string(),
                "--model".to_string(),
                "gpt-5.5".to_string()
            ])
            .as_deref(),
            Some("gpt-5.5")
        );
        assert_eq!(
            codex_requested_model(&[
                "exec".to_string(),
                "-m".to_string(),
                "local-model".to_string()
            ])
            .as_deref(),
            Some("local-model")
        );
        assert_eq!(codex_requested_model(&["exec".to_string()]), None);
    }

    #[test]
    fn codex_forwarded_config_args_are_flagged_before_launch() {
        let conflicts = codex_forwarded_config_args(&[
            "exec".to_string(),
            "-c".to_string(),
            "foo=1".to_string(),
        ]);
        assert_eq!(conflicts, vec!["-c"]);

        let conflicts = codex_forwarded_config_args(&[
            "exec".to_string(),
            "--config".to_string(),
            "model_provider=\"openai\"".to_string(),
        ]);
        assert_eq!(conflicts, vec!["--config"]);
    }

    #[test]
    fn bitrouter_root_url_strips_v1_for_preflight_health_probe() {
        assert_eq!(
            bitrouter_root_url("http://127.0.0.1:4356/v1"),
            "http://127.0.0.1:4356"
        );
        assert_eq!(
            bitrouter_root_url("http://127.0.0.1:4356"),
            "http://127.0.0.1:4356"
        );
    }

    #[test]
    fn codex_route_check_accepts_any_responses_provider() {
        let route = vec![crate::daemon::RouteHop {
            provider: "openai".to_string(),
            service_id: "gpt-5.5".to_string(),
            api_protocol: "responses".to_string(),
        }];
        let check = codex_route_check("gpt-5.5", Ok(route));
        assert_eq!(check.status, SpawnCheckStatus::Pass);
        assert!(check.message.contains("openai"));
    }

    #[test]
    fn codex_route_check_rejects_non_responses_provider() {
        let route = vec![crate::daemon::RouteHop {
            provider: "anthropic".to_string(),
            service_id: "claude-sonnet".to_string(),
            api_protocol: "messages".to_string(),
        }];
        let check = codex_route_check("claude-sonnet", Ok(route));
        assert_eq!(check.status, SpawnCheckStatus::Fail);
        assert!(check.message.contains("responses"));
    }

    #[test]
    fn install_command_is_the_official_native_installer() {
        let cmd = InstallCommand::for_binary("claude").expect("claude has a bundled installer");
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

        let codex = InstallCommand::for_binary("codex").expect("codex has a bundled installer");
        assert!(codex.display().contains("chatgpt.com/codex/install"));
        // The rest of the catalog is installed from upstream, not by us.
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
