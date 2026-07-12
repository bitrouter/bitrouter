//! `bitrouter spawn` — launch a coding-agent harness (Claude Code, Codex, …) as a
//! child process with its API base URL pointed at the local BitRouter daemon.
//!
//! The agent's traffic then routes through BitRouter without ever touching the
//! agent's own config files: instead of mutating `~/.claude/config.json` or
//! `~/.codex/config.toml` (the
//! "config takeover" model used by some switcher tools — invasive, needs
//! backup/restore and crash recovery), we set per-process environment variables
//! or one-shot CLI config overrides. Nothing on disk changes, and if BitRouter
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
use clap::ValueEnum;
use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;
use crate::style::Palette;

/// The coding-agent harnesses `bitrouter spawn` can launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SpawnAgent {
    /// Anthropic's Claude Code CLI (`claude`).
    Claude,
    /// OpenAI's Codex CLI (`codex`).
    Codex,
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
            SpawnAgent::Codex => AgentSpec {
                agent: self,
                id: "codex",
                binary: "codex",
                // Codex custom providers are configured through `-c` one-shot
                // overrides rather than fixed endpoint env vars.
                base_url_env: "",
                auth_token_env: "",
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

/// What `bitrouter spawn` injects into the child process.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildLaunch {
    /// Environment overrides for the child process.
    env: Vec<(&'static str, String)>,
    /// Arguments inserted before the user's forwarded args.
    args_prefix: Vec<String>,
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
            "spawn check for {} via {}",
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

/// Run `bitrouter spawn`. Resolves the base URL from `cfg`, locates the agent
/// binary (offering to install it if missing and permitted), ensures the local
/// daemon is up (auto-starting it when down), then execs the agent with the
/// routing environment injected. On success this **does not return** — it exits
/// the process with the agent's exit code, the way a launcher like
/// `git <subcommand>` propagates its child's status.
pub async fn run(
    source: &crate::paths::ConfigSource,
    cfg: &bitrouter_sdk::config::Config,
    opts: SpawnOptions,
) -> Result<()> {
    let spec = opts.agent.spec();

    let base_url = match &opts.base_url {
        Some(explicit) => explicit.clone(),
        None => derive_base_url(&cfg.server.listen),
    };

    if opts.agent == SpawnAgent::Codex {
        let conflicts = codex_forwarded_config_args(&opts.agent_args);
        if !conflicts.is_empty() {
            anyhow::bail!(
                "codex forwarded config flags ({}) can override BitRouter's one-shot provider \
                 injection. Remove those -c/--config flags and run `bitrouter spawn --agent \
                 codex --check` to inspect the route before launching.",
                conflicts.join(", ")
            );
        }
    }

    // Locate the binary; prompt-to-install when it's missing.
    let binary = ensure_agent_installed(opts.agent, opts.no_install).await?;

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

    // Auth-token precedence (highest first): an auth token the user already
    // exported for this agent → the BitRouter API key → a local placeholder.
    // Codex has no fixed endpoint/auth env vars; it gets one-shot config args
    // from `build_child_launch` below.
    let parent_auth = match opts.agent {
        SpawnAgent::Claude => nonempty_env(spec.auth_token_env),
        SpawnAgent::Codex => None,
    };
    let bitrouter_key = nonempty_env(BITROUTER_API_KEY_ENV);
    let launch = build_child_launch(&spec, &base_url, parent_auth, bitrouter_key);

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
    cmd.args(&launch.args_prefix);
    cmd.args(&opts.agent_args);
    for (k, v) in &launch.env {
        cmd.env(k, v);
    }
    // Inherit the parent's stdio so the agent owns the terminal directly
    // (Claude Code is an interactive TUI). Inheritance is the default for
    // `Command`, but we state it for clarity.
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    // Timestamp the wrapped session so the exit summary can attribute
    // spend to exactly this run of the agent.
    let session_start = chrono::Utc::now();

    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawning agent '{}' ({})", spec.id, binary.display()))?;

    print_exit_summary(source, session_start, &p).await;

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
async fn print_exit_summary(
    source: &crate::paths::ConfigSource,
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
    let (Ok(session), Ok(today)) = (
        store.spend_summary(window).await,
        store.spend_summary(TimeWindow::Today).await,
    ) else {
        return;
    };
    if session.requests == 0 {
        return;
    }
    eprintln!(
        "{cyan}{bold}spawn:{reset} session spend {bold}{}{reset} ({} requests) · today {}",
        crate::metering::fmt_usd(session.spend_micro_usd),
        session.requests,
        crate::metering::fmt_usd(today.spend_micro_usd),
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );
}

/// Check a spawn invocation without launching the child process.
pub async fn check(
    cfg: &bitrouter_sdk::config::Config,
    opts: &SpawnOptions,
) -> Result<SpawnCheckReport> {
    let spec = opts.agent.spec();
    let base_url = opts
        .base_url
        .clone()
        .unwrap_or_else(|| derive_base_url(&cfg.server.listen));
    let model = (opts.agent == SpawnAgent::Codex)
        .then(|| codex_requested_model(&opts.agent_args))
        .flatten();
    let mut checks = Vec::new();

    checks.push(match resolve_binary(spec.binary) {
        Some(path) => SpawnCheckRow {
            name: "agent binary".to_string(),
            status: SpawnCheckStatus::Pass,
            message: format!("found {}", path.display()),
        },
        None => SpawnCheckRow {
            name: "agent binary".to_string(),
            status: SpawnCheckStatus::Fail,
            message: format!("{} is not on PATH", spec.binary),
        },
    });

    checks.push(check_base_url(&base_url).await);

    if opts.agent == SpawnAgent::Codex {
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
                message: "no --model/-m forwarded; Codex will choose its default model".to_string(),
            },
        });
    }

    Ok(SpawnCheckReport {
        agent: spec.id.to_string(),
        base_url,
        model,
        checks,
    })
}

async fn check_base_url(base_url: &str) -> SpawnCheckRow {
    let health = format!("{}/health", bitrouter_root_url(base_url));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return SpawnCheckRow {
                name: "bitrouter base url".to_string(),
                status: SpawnCheckStatus::Warn,
                message: format!("could not build HTTP client: {e}"),
            };
        }
    };
    match client.get(&health).send().await {
        Ok(resp) if resp.status().is_success() => SpawnCheckRow {
            name: "bitrouter base url".to_string(),
            status: SpawnCheckStatus::Pass,
            message: format!("{health} responded {}", resp.status()),
        },
        Ok(resp) => SpawnCheckRow {
            name: "bitrouter base url".to_string(),
            status: SpawnCheckStatus::Fail,
            message: format!("{health} responded {}", resp.status()),
        },
        Err(e) => SpawnCheckRow {
            name: "bitrouter base url".to_string(),
            status: SpawnCheckStatus::Fail,
            message: format!("could not reach {health}: {e}"),
        },
    }
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

/// Build all per-agent child-process overrides. Claude Code uses environment
/// variables; Codex uses transient `-c` config overrides because custom model
/// providers are a Codex config concept.
fn build_child_launch(
    spec: &AgentSpec,
    base_url: &str,
    parent_auth: Option<String>,
    bitrouter_key: Option<String>,
) -> ChildLaunch {
    match spec.agent {
        SpawnAgent::Claude => ChildLaunch {
            env: build_child_env(spec, base_url, parent_auth, bitrouter_key),
            args_prefix: Vec::new(),
        },
        SpawnAgent::Codex => build_codex_child_launch(base_url, bitrouter_key),
    }
}

fn build_codex_child_launch(base_url: &str, bitrouter_key: Option<String>) -> ChildLaunch {
    let mut env = Vec::new();
    let mut args_prefix = vec![
        "-c".to_string(),
        codex_config_string("model_provider", "bitrouter"),
        "-c".to_string(),
        codex_config_string("model_providers.bitrouter.name", "BitRouter"),
        "-c".to_string(),
        codex_config_string(
            "model_providers.bitrouter.base_url",
            &codex_api_base_url(base_url),
        ),
        "-c".to_string(),
        codex_config_string("model_providers.bitrouter.wire_api", "responses"),
    ];

    match bitrouter_key {
        Some(key) => {
            env.push((BITROUTER_API_KEY_ENV, key));
            args_prefix.push("-c".to_string());
            args_prefix.push(codex_config_string(
                "model_providers.bitrouter.env_key",
                BITROUTER_API_KEY_ENV,
            ));
        }
        None => {
            args_prefix.push("-c".to_string());
            args_prefix.push(codex_config_string(
                "model_providers.bitrouter.experimental_bearer_token",
                PLACEHOLDER_API_KEY,
            ));
        }
    }

    ChildLaunch { env, args_prefix }
}

fn codex_api_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

fn codex_config_string(key: &str, value: &str) -> String {
    format!("{key}={}", toml_string(value))
}

fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
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

/// True when `listen` binds a loopback / wildcard address — i.e. a daemon on
/// *this* host that `bitrouter spawn` may auto-start. A remote or LAN host is
/// someone else's daemon, which we can only warn about. Exact-match only:
/// `127.0.0.0/8` aliases (e.g. `127.0.0.2`) and IPv4-mapped IPv6 fall through to
/// the warn path — the fail-safe direction (never a wrong auto-start).
fn listen_is_local(listen: &str) -> bool {
    let (host, _port) = split_listen(listen);
    matches!(
        host,
        "127.0.0.1" | "0.0.0.0" | "" | "::1" | "[::1]" | "::" | "[::]" | "localhost"
    )
}

/// Extract the `host[:port]` authority from a base URL for a best-effort
/// reachability note. Returns `None` when there is no authority to probe.
fn listen_from_base_url(url: &str) -> Option<String> {
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
async fn ensure_local_daemon(
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
    /// Sources:
    /// - Claude Code quickstart, "Native Install":
    ///   <https://code.claude.com/docs/en/quickstart>
    /// - macOS / Linux: `curl -fsSL https://claude.ai/install.sh | bash`
    /// - Windows:       `irm https://claude.ai/install.ps1 | iex`
    /// - Codex quickstart:
    ///   <https://developers.openai.com/codex/quickstart>
    /// - macOS / Linux: `curl -fsSL https://chatgpt.com/codex/install.sh | sh`
    /// - Windows:       `irm https://chatgpt.com/codex/install.ps1 | iex`
    pub fn for_agent(agent: SpawnAgent) -> Self {
        match agent {
            SpawnAgent::Claude => Self::claude(),
            SpawnAgent::Codex => Self::codex(),
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
    fn codex_spec_uses_codex_binary() {
        let spec = SpawnAgent::Codex.spec();
        assert_eq!(spec.binary, "codex");
        assert_eq!(spec.id, "codex");
    }

    #[test]
    fn codex_launch_args_route_responses_to_bitrouter_v1() {
        let spec = SpawnAgent::Codex.spec();
        let launch = build_child_launch(&spec, "http://127.0.0.1:4356", None, None);
        assert!(launch.args_prefix.contains(&"-c".to_string()));
        assert!(
            launch
                .args_prefix
                .contains(&"model_provider=\"bitrouter\"".to_string())
        );
        assert!(launch.args_prefix.contains(
            &"model_providers.bitrouter.base_url=\"http://127.0.0.1:4356/v1\"".to_string()
        ));
        assert!(
            launch
                .args_prefix
                .contains(&"model_providers.bitrouter.wire_api=\"responses\"".to_string())
        );
        assert!(launch.args_prefix.contains(
            &"model_providers.bitrouter.experimental_bearer_token=\"bitrouter-local\"".to_string()
        ));
    }

    #[test]
    fn codex_launch_uses_env_key_when_bitrouter_key_exists() {
        let spec = SpawnAgent::Codex.spec();
        let launch = build_child_launch(
            &spec,
            "http://127.0.0.1:4356",
            None,
            Some("brk_test".into()),
        );
        assert!(
            launch
                .env
                .iter()
                .any(|(k, v)| *k == BITROUTER_API_KEY_ENV && v == "brk_test")
        );
        assert!(
            launch
                .args_prefix
                .contains(&"model_providers.bitrouter.env_key=\"BITROUTER_API_KEY\"".to_string())
        );
        assert!(
            launch
                .args_prefix
                .iter()
                .all(|arg| !arg.contains("experimental_bearer_token"))
        );
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

        let codex = InstallCommand::for_agent(SpawnAgent::Codex);
        assert!(codex.display().contains("chatgpt.com/codex/install"));
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
