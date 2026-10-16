//! `bro launch` — launch a coding-agent harness (Claude Code, Codex, …)
//! as an interactive native-TUI child process. Routed harnesses are pointed at
//! the local BitRouter daemon; own-auth harnesses launch directly. This is the
//! interactive surface; headless ACP sub-agents are `bro spawn` (see
//! [`crate::acp_cli`]). Both draw their routing knowledge from the shared
//! [`crate::harness`] catalog.
//!
//! Routed agent traffic goes through BitRouter without ever touching the
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
//! ([`crate::harness::catalog::Harness::launch_overlay`]).
//! Own-auth harnesses (`grok`, `agy`) are subscription clients the daemon
//! borrows as providers, so `launch` does not redirect or meter them.
//!
//! CLI shape follows `cargo run`'s separator convention so there is no
//! ambiguity about which flags belong to which program:
//!
//! ```text
//!   bro launch --agent claude [bitrouter opts] -- <args forwarded to claude>
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

use std::path::PathBuf;
use std::time::Duration;

use crate::commands::invocation;
use crate::gateway::error::BitrouterError;
use anyhow::{Context, Result};
use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;
use crate::providers::cli_install::{ensure_binary_installed, resolve_binary};
use crate::style::Palette;

/// Resolve a `bro launch --agent` value to its catalog harness. Accepts
/// the interactive binary name (`claude`, `codex`, `opencode`, `pi`) and the
/// catalog id (`claude-acp`, `pi-acp`, …). An unknown value is a caller
/// mistake, so the error is a [`BitrouterError::BadRequest`] — the CLI's error
/// envelope reports it as `kind: "bad_request"`, not `internal` — and lists
/// what is available.
pub fn resolve_launch_agent(id: &str) -> Result<&'static crate::harness::catalog::Harness> {
    let known = crate::harness::catalog::by_interactive_binary(id)
        .or_else(|| crate::harness::catalog::by_id(id).filter(|h| h.interactive_binary.is_some()));
    known.ok_or_else(|| {
        anyhow::Error::new(BitrouterError::bad_request(format!(
            "'{id}' is not a launchable harness — available: {}",
            launchable().join(", ")
        )))
    })
}

/// The interactive binaries `launch` supports, sorted for a stable message.
fn launchable() -> Vec<&'static str> {
    let mut names: Vec<&str> = crate::harness::catalog::CATALOG
        .iter()
        .filter(|h| h.launch_supported())
        .filter_map(|h| h.interactive_binary)
        .collect();
    names.sort_unstable();
    names
}

/// The `--agent` value a harness is displayed as: its interactive binary
/// (falling back to the catalog id, which cannot happen for a resolved
/// launch target).
fn harness_id(h: &crate::harness::catalog::Harness) -> &'static str {
    h.interactive_binary.unwrap_or(h.id)
}

/// What `bro launch` injects into the child process. The env/args come
/// from the shared [`crate::harness`] catalog's [`RoutingOverlay`](crate::harness::catalog::RoutingOverlay), so the
/// interactive and ACP facets of a harness route identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildLaunch {
    /// Environment overrides for the child process.
    pub env: Vec<(String, String)>,
    /// Arguments inserted before the user's forwarded args.
    pub args_prefix: Vec<String>,
}

/// Resolve the gateway bearer credential for a `bro launch` child by
/// precedence: a token the user already exported for the harness →
/// `BITROUTER_API_KEY` → a freshly minted per-launch token (valid under
/// `skip_auth: true`, the `bro init` default; the harness merely needs
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
pub(crate) fn resolve_launch_token(
    parent_auth: Option<String>,
    bitrouter_key: Option<String>,
) -> String {
    parent_auth
        .or(bitrouter_key)
        .unwrap_or_else(mint_launch_token)
}

/// A fresh opaque per-launch attribution token.
fn mint_launch_token() -> String {
    format!(
        "{}{}",
        crate::gateway::caller::LAUNCH_TOKEN_PREFIX,
        uuid::Uuid::new_v4().simple()
    )
}

/// Whether a resolved credential is one we minted — i.e. whether this launch's
/// requests will carry its own attribution, or fall back to the window.
pub(crate) fn is_launch_token(token: &str) -> bool {
    crate::gateway::caller::launch_tag(Some(&format!("Bearer {token}"))).is_some()
}

/// Options gathered from the CLI for one `launch` invocation.
pub struct SpawnOptions {
    /// Which harness to launch, already resolved against the shared catalog
    /// (see [`resolve_launch_agent`]).
    pub agent: &'static crate::harness::catalog::Harness,
    /// Pin the harness's model. Applied through the harness's own mechanism —
    /// a model env var, a `-c model=` override, the synthesized config's
    /// default, or the harness's native flag for own-auth clients.
    pub model: Option<String>,
    /// Arguments forwarded verbatim to the agent binary (everything the
    /// caller put after `--`).
    pub agent_args: Vec<String>,
    /// Explicit BitRouter daemon base URL override for routed harnesses. When
    /// `None` it is derived from the daemon's `server.listen`; own-auth
    /// harnesses are not redirected.
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

/// Result of `bro spawn --check`.
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
    /// The catalog harness being launched.
    pub harness: &'static crate::harness::catalog::Harness,
    /// Config source, for the exit summary's metering read.
    pub source: &'a crate::paths::ConfigSource,
    /// The attribution token this launch minted, when it owns the credential
    /// slot. `None` when the user supplied their own key — spend then falls
    /// back to the unattributed time window.
    pub launch_id: Option<String>,
    /// Stamped before the child exists, so the exit summary's window covers
    /// exactly the wrapped session. Nothing between here and the spawn spends.
    pub session_start: chrono::DateTime<chrono::Utc>,
}

/// Run `bro launch`: [`prepare`] the child, then run it with the
/// terminal inherited. On success this **does not return** — it exits the
/// process with the agent's exit code, the way a launcher like
/// `git <subcommand>` propagates its child's status.
pub async fn run(
    source: &crate::paths::ConfigSource,
    cfg: &crate::config::Config,
    opts: SpawnOptions,
) -> Result<()> {
    let prepared = prepare(source, cfg, opts).await?;
    exec_inherited(prepared).await
}

/// Resolve the base URL from `cfg`, locate the agent binary (offering to
/// install it if missing and permitted), ensure the local daemon is up when a
/// routed harness will use it (auto-starting it when down), assemble the launch
/// overlay, and state what the harness is actually getting (§796) — everything
/// up to, but not including, the child process.
pub async fn prepare<'a>(
    source: &'a crate::paths::ConfigSource,
    cfg: &crate::config::Config,
    opts: SpawnOptions,
) -> Result<Prepared<'a>> {
    let harness = opts.agent;

    let base_url = match &opts.base_url {
        Some(explicit) => explicit.clone(),
        None => derive_base_url(&cfg.server.listen),
    };

    if matches!(harness.routing, crate::harness::catalog::Routing::CodexArgs) {
        let conflicts = codex_forwarded_config_args(&opts.agent_args);
        if !conflicts.is_empty() {
            anyhow::bail!(
                "codex forwarded config flags ({}) can override BitRouter's one-shot provider \
                 injection. Remove those -c/--config flags and run `{cli} launch --agent codex \
                 --check` to inspect the route before launching.",
                conflicts.join(", "),
                cli = invocation::name()
            );
        }
    }

    // Locate the binary; prompt-to-install when it's missing.
    let binary = ensure_harness_installed(harness, opts.no_install).await?;

    // Make sure the daemon a routed harness will talk to is up. For the local
    // daemon we own (derived base URL + a loopback/wildcard bind), probe its
    // control socket and auto-start it when down; for an explicit `--base-url`
    // or a non-local bind we can only warn — we can't start someone else's
    // daemon. Own-auth harnesses are not redirected, but the probe still gives
    // a useful daemon-readiness signal for provider borrowing.
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
        crate::harness::catalog::Routing::Env { auth_env, .. } => nonempty_env(auth_env),
        _ => None,
    };
    let token = resolve_launch_token(
        parent_auth,
        nonempty_env(crate::harness::catalog::BITROUTER_API_KEY_ENV),
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
    let gateways = launch_gateways(
        cfg,
        &base_url,
        opts.base_url.is_none() && listen_is_local(&cfg.server.listen),
    );
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
        harness,
        source,
        launch_id: is_launch_token(&token).then(|| token.clone()),
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
    harness: &crate::harness::catalog::Harness,
    base_url: &str,
    gateways: &[crate::harness::catalog::McpServer],
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
    if matches!(harness.routing, crate::harness::catalog::Routing::OwnAuth) {
        return format!("{head} · own-auth · not routed · not metered");
    }

    format!(
        "{head} · routed via bitrouter ({base_url}) · {}",
        capability_summary(harness, gateways, p)
    )
}

/// What the gateways actually delivered to this harness, as one phrase.
///
/// The startup line (#796, #782) and the machine-readable launch report are
/// both built from this one function, so the two cannot drift into disagreeing
/// about what the harness actually got.
pub(crate) fn capability_summary(
    harness: &crate::harness::catalog::Harness,
    gateways: &[crate::harness::catalog::McpServer],
    p: &Palette,
) -> String {
    let named = |name: &str| gateways.iter().any(|s| s.name == name);
    if harness.injects_mcp() {
        format!("tools {}", mark(named(crate::gateways::TOOLS_SERVER), p))
    } else {
        format!(
            "tools {} ({} has no MCP mechanism)",
            mark(false, p),
            harness_id(harness),
        )
    }
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
/// endpoint (`bitrouter_tools`, only when `mcp.aggregate` is enabled). See
/// [`crate::gateways`]. Installed local skills reach agents through the
/// shippable Agent Skill/plugin rails, not through an origin MCP server.
///
/// Injection reaches only the harnesses that have a mechanism for it — claude
/// (`--mcp-config`), codex (`-c mcp_servers.*`), opencode and hermes (their
/// synthesized config files). `pi`, `openclaw`, `grok`, and `antigravity` have
/// no injectable MCP surface and ignore these servers; that ceiling is the
/// harnesses', not BitRouter's.
fn launch_gateways(
    cfg: &crate::config::Config,
    base_url: &str,
    owns_local_daemon: bool,
) -> Vec<crate::harness::catalog::McpServer> {
    let auth = crate::harness::catalog::resolve_gateway_auth(
        std::env::var(crate::harness::catalog::BITROUTER_API_KEY_ENV).ok(),
        false,
    )
    .unwrap_or_else(|| crate::harness::catalog::PLACEHOLDER_API_KEY.to_string());
    // Local assembly mounts MCP only when upstreams are configured. External
    // targets own their upstream inventory; preserve their configured route.
    let aggregate_route = (cfg.mcp.aggregate.enabled
        && (!owns_local_daemon || !cfg.mcp_servers.is_empty()))
    .then(|| cfg.mcp.aggregate.route.clone());
    crate::gateways::gateway_servers(base_url, &auth, aggregate_route.as_deref())
}

/// Whether this harness's overlay needs the daemon's advertised model ids —
/// true exactly for the config-synthesis harnesses, whose synthesized provider
/// carries an explicit model list. Env/args and own-auth harnesses ignore it,
/// so they never pay for the `/v1/models` probe.
fn needs_model_catalog(h: &crate::harness::catalog::Harness) -> bool {
    matches!(h.routing, crate::harness::catalog::Routing::ConfigFile(_))
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
pub async fn check(cfg: &crate::config::Config, opts: &SpawnOptions) -> Result<SpawnCheckReport> {
    let harness = opts.agent;
    let agent_id = harness_id(harness);
    let binary = harness.interactive_binary.unwrap_or(harness.id);
    let is_codex = matches!(harness.routing, crate::harness::catalog::Routing::CodexArgs);
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
        status: if matches!(harness.routing, crate::harness::catalog::Routing::OwnAuth) {
            SpawnCheckStatus::Warn
        } else {
            SpawnCheckStatus::Pass
        },
        message: match harness.routing {
            crate::harness::catalog::Routing::OwnAuth => format!(
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
                // Resolve the same registry and stored-login activation as daemon
                // startup before applying Codex's native-name qualification.
                let mut resolved = cfg.clone();
                crate::assemble::merge_registry_into(&mut resolved).await;
                let resolved = crate::commands::resolve_static(resolved);
                let route = crate::codex_router::subscription_route(&resolved, model);
                codex_route_check(
                    model,
                    crate::commands::resolve_route(&resolved, route.as_deref().unwrap_or(model))
                        .await,
                )
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
/// *this* host that `bro spawn` may auto-start. A remote or LAN host is
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
    cfg: &crate::config::Config,
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

/// Locate a catalog harness's interactive binary, offering the official native
/// installer when BitRouter bundles one for it (claude, codex) and the caller
/// permits prompting. Harnesses with no bundled installer get an actionable
/// error pointing at their upstream project.
pub(crate) async fn ensure_harness_installed(
    harness: &'static crate::harness::catalog::Harness,
    no_install: bool,
) -> Result<PathBuf> {
    let binary = harness
        .interactive_binary
        .ok_or_else(|| anyhow::anyhow!("harness '{}' has no interactive binary", harness.id))?;
    ensure_binary_installed(binary, Some(harness.project_url), no_install).await
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
             start one with `{cli} start` (the agent will fail to reach it otherwise).",
            cyan = p.cyan,
            reset = p.reset,
            cli = invocation::name(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::cli_install::SpawnAgent;

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
    fn startup_line_states_routing_and_tool_availability() -> anyhow::Result<()> {
        let p = Palette::none();
        let cfg = config_with_mcp_upstream()?;
        let gateways = launch_gateways(&cfg, "http://127.0.0.1:4356", true);
        let line = |id: &str| {
            startup_line(
                crate::harness::catalog::by_id(id).expect("catalog"),
                "http://127.0.0.1:4356",
                &gateways,
                &p,
            )
        };

        // Fully-capable harness: routed, and the upstream-tools gateway lands.
        let claude = line("claude-acp");
        assert!(claude.contains("routed via bitrouter"), "{claude}");
        assert!(claude.contains("tools ✓"), "{claude}");

        // Routed, but the harness has nowhere to put MCP servers. The reason
        // is stated — a bare ✗ would read as a BitRouter failure.
        let pi = line("pi-acp");
        assert!(pi.contains("routed via bitrouter"), "{pi}");
        assert!(pi.contains("tools ✗"), "{pi}");
        assert!(pi.contains("no MCP mechanism"), "{pi}");

        // Own-auth: degrade honestly rather than showing blanks.
        Ok(())
    }

    #[test]
    fn startup_line_reports_a_disabled_aggregate_as_a_missing_tools_gateway() {
        // `mcp.aggregate` off means `bitrouter_tools` is never injected. The
        // line must say so rather than claiming tools the harness never got.
        let p = Palette::none();
        let line = startup_line(
            crate::harness::catalog::by_id("claude-acp").expect("catalog"),
            "http://127.0.0.1:4356",
            &[],
            &p,
        );
        assert!(line.contains("tools ✗"), "{line}");
    }

    fn config_with_mcp_upstream() -> anyhow::Result<crate::config::Config> {
        let mut cfg = crate::config::Config::default();
        cfg.mcp_servers.insert(
            "fixture".into(),
            serde_json::from_value(serde_json::json!({
                "name":"fixture", "transport":{"type":"http", "url":"http://127.0.0.1:4357/mcp"}
            }))?,
        );
        Ok(cfg)
    }

    #[test]
    fn launch_wires_the_tools_gateway_into_the_overlay() -> anyhow::Result<()> {
        let cfg = config_with_mcp_upstream()?;
        let servers = launch_gateways(&cfg, "http://127.0.0.1:4356", true);
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["bitrouter_tools"]);
        // The fleet bridge is gone: `launch` must never inject it.
        assert!(!names.contains(&"bitrouter_fleet"));

        // Every harness with an injectable MCP mechanism must actually carry
        // the servers through synthesis. The catalog's other arms (pi,
        // openclaw, grok, antigravity) have no mechanism and are excluded by
        // design.
        for id in ["claude-acp", "codex-acp", "opencode", "hermes-acp"] {
            let h = crate::harness::catalog::by_id(id).expect("catalog entry");
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
            assert!(
                rendered.contains("bitrouter_tools"),
                "{id}: gateway `bitrouter_tools` never reached the harness"
            );
            assert!(!rendered.contains("bitrouter_skills"));
        }
        Ok(())
    }

    #[test]
    fn launch_gateways_drops_tools_when_the_aggregate_is_disabled() {
        let mut cfg = crate::config::Config::default();
        cfg.mcp.aggregate.enabled = false;
        let servers = launch_gateways(&cfg, "http://127.0.0.1:4356", true);
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert!(names.is_empty());
    }

    #[test]
    fn launch_without_mcp_upstreams_does_not_advertise_an_unmounted_gateway() {
        let cfg = crate::config::Config::default();
        assert!(cfg.mcp_servers.is_empty());
        assert!(launch_gateways(&cfg, "http://127.0.0.1:4356", true).is_empty());
    }

    #[test]
    fn external_launch_keeps_mcp_gateway_without_local_upstreams() {
        let mut cfg = crate::config::Config::default();
        assert!(cfg.mcp_servers.is_empty());
        let servers = launch_gateways(&cfg, "https://router.example", false);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "bitrouter_tools");
        cfg.mcp.aggregate.enabled = false;
        assert!(launch_gateways(&cfg, "https://router.example", false).is_empty());
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
    fn only_a_minted_credential_is_treated_as_attribution() {
        // A user's own key must never be re-read as a launch tag: it is real
        // authentication, and mislabelling it would both break `skip_auth:
        // false` and attribute someone else's traffic to this launch.
        assert!(!is_launch_token("user-token"));
        assert!(!is_launch_token("brk_key"));
        assert!(!is_launch_token(
            crate::harness::catalog::PLACEHOLDER_API_KEY
        ));
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
    fn launch_agents_map_to_catalog_harnesses() -> Result<()> {
        // The interactive binary is the key into the shared routing catalog;
        // the overlay content itself is covered by `harness` tests.
        let claude =
            crate::harness::catalog::by_interactive_binary(SpawnAgent::Claude.spec().binary)
                .context("claude maps to a catalog harness")?;
        assert_eq!(claude.id, "claude-acp");
        assert!(matches!(
            claude.routing,
            crate::harness::catalog::Routing::Env {
                base_url_env: "ANTHROPIC_BASE_URL",
                auth_env: "ANTHROPIC_AUTH_TOKEN",
                ..
            }
        ));

        let codex = crate::harness::catalog::by_interactive_binary(SpawnAgent::Codex.spec().binary)
            .context("codex maps to a catalog harness")?;
        assert_eq!(codex.id, "codex-acp");
        assert!(matches!(
            codex.routing,
            crate::harness::catalog::Routing::CodexArgs
        ));
        Ok(())
    }

    #[test]
    fn resolve_launch_agent_accepts_every_supported_harness() {
        // `launch --agent` takes every *supported* harness, by binary name…
        for h in crate::harness::catalog::CATALOG
            .iter()
            .filter(|h| h.launch_supported())
        {
            let binary = h.interactive_binary.expect("supported implies a binary");
            let resolved = resolve_launch_agent(binary).expect("resolves by binary");
            assert_eq!(resolved.id, h.id, "{binary}");
            // …and by catalog id (`pi-acp` → `pi`).
            let by_id = resolve_launch_agent(h.id).expect("resolves by catalog id");
            assert_eq!(by_id.id, h.id);
        }
        // The full expected surface, spelled out so a catalog change is a
        // deliberate CLI change.
        assert_eq!(
            launchable(),
            vec!["claude", "codex", "hermes", "openclaw", "opencode", "pi"]
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
        // The message lists the fix — every harness `launch` actually
        // supports, which is now every catalog entry with an interactive
        // binary. It never advertises one that would then be refused, and
        // never omits one that would have worked.
        for id in ["claude", "codex", "opencode", "pi", "hermes", "openclaw"] {
            assert!(msg.contains(id), "{msg} should list {id}");
        }
        // A typo is the caller's mistake, not a BitRouter fault: the error
        // envelope must report `bad_request`, never `internal`.
        for bad in ["gemini-cli", "nope"] {
            let err = resolve_launch_agent(bad).expect_err("rejected");
            assert_eq!(
                crate::output::error::envelope_from_anyhow(&err).error.kind,
                crate::gateway::error::ErrorKind::BadRequest,
                "{bad}"
            );
        }
    }

    #[test]
    fn needs_model_catalog_only_for_config_synthesis_harnesses() {
        for id in ["opencode", "pi-acp", "hermes-acp", "openclaw"] {
            let h = crate::harness::catalog::by_id(id).expect("catalog harness");
            assert!(needs_model_catalog(h), "{id} synthesizes a model list");
        }
        for id in ["claude-acp", "codex-acp"] {
            let h = crate::harness::catalog::by_id(id).expect("catalog harness");
            assert!(!needs_model_catalog(h), "{id} needs no /v1/models probe");
        }
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

    #[tokio::test]
    async fn codex_preflight_resolves_native_subscription_model() -> anyhow::Result<()> {
        let cfg = crate::config::parse_with(
            r#"
inherit_defaults: false
providers:
  openai-codex:
    class: first-party-subscription
    models:
      - id: openai/gpt-native
        provider_model_id: gpt-native
        api_protocol: responses
"#,
            |_| None,
        )?;
        let report = check(
            &cfg,
            &SpawnOptions {
                agent: crate::harness::catalog::by_id("codex-acp")
                    .context("Codex harness missing")?,
                model: Some("gpt-native".into()),
                agent_args: Vec::new(),
                base_url: Some("http://127.0.0.1:0".into()),
                no_install: true,
                no_start: true,
                check: true,
            },
        )
        .await?;
        let route = report
            .checks
            .iter()
            .find(|row| row.name == "codex model route")
            .context("model route check missing")?;
        assert_eq!(route.status, SpawnCheckStatus::Pass, "{}", route.message);
        assert!(route.message.contains("openai-codex:gpt-native"));
        Ok(())
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
}
