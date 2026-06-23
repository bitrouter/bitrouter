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
//! ## Model overrides
//!
//! By default the launched agent uses its own default models (for Claude Code,
//! the bare `claude-*` ids that the daemon routes to the subscription). A user
//! can instead point the agent at arbitrary BitRouter models — expressed as
//! generic capability tiers ([`model_plan::ModelTier`]) and supplied via the
//! config file, environment variables, or `--preset` / `--model` flags. The
//! selection (the [`model_plan`] IR) is resolved independently of how it reaches
//! a given harness ([`agent::AgentSpec::tier_env`]), so the feature is generic
//! across agents.
//!
//! ## Claude Code integration
//!
//! - `ANTHROPIC_BASE_URL` redirects the Anthropic SDK Claude Code uses to an
//!   alternate endpoint. See the Claude Code settings reference:
//!   <https://code.claude.com/docs/en/settings#environment-variables>.
//! - Install commands are the official native installers documented in the
//!   Claude Code quickstart: <https://code.claude.com/docs/en/quickstart>.

pub mod agent;
pub mod model_plan;

use anyhow::{Context, Result};

use crate::spawn::agent::{AgentSpec, SpawnAgent, ensure_agent_installed};
use crate::spawn::model_plan::ModelPlan;
use crate::style::Palette;

/// BitRouter's own API-key env var (`brk_…`). When set, we forward it to the
/// agent as the gateway bearer token so the agent authenticates to BitRouter
/// with the user's real credential instead of the placeholder.
const BITROUTER_API_KEY_ENV: &str = "BITROUTER_API_KEY";

/// Environment variable naming a `spawn.presets` entry to apply (overrides the
/// config default plan, overridden by the `--preset` flag).
const SPAWN_PRESET_ENV: &str = "BITROUTER_SPAWN_PRESET";

/// Environment variable holding a single model id applied to every tier
/// (overrides a preset, overridden by `--model`).
const SPAWN_MODEL_ENV: &str = "BITROUTER_SPAWN_MODEL";

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
    /// `--preset <name>`: a `spawn.presets` entry to apply on top of the config
    /// default plan / environment overrides. `None` when not given.
    pub preset: Option<String>,
    /// `--model <SPEC>` flags (repeatable): a bare `<id>` sets every tier; a
    /// `<tier>=<id>` sets one tier. Applied last (highest priority).
    pub models: Vec<String>,
}

/// Run `bitrouter spawn`. Resolves the base URL and the model plan, locates the
/// agent binary (offering to install it if missing and permitted), ensures the
/// local daemon is up (auto-starting it when down), then execs the agent with
/// the routing environment injected. On success this **does not return** — it
/// exits the process with the agent's exit code, the way a launcher like
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

    // Resolve the model plan before anything with side effects (install /
    // daemon start), so a typo in a preset name or `--model` flag fails fast.
    let plan = model_plan::resolve(
        opts.agent,
        &cfg.spawn,
        nonempty_env(SPAWN_PRESET_ENV),
        nonempty_env(SPAWN_MODEL_ENV),
        opts.preset.as_deref(),
        &opts.models,
    )?;

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
    let parent_auth = nonempty_env(spec.auth_token_env);
    let bitrouter_key = nonempty_env(BITROUTER_API_KEY_ENV);
    let env = build_child_env(&spec, &base_url, parent_auth, bitrouter_key, &plan);

    let p = Palette::for_stderr();
    eprintln!(
        "{cyan}{bold}spawn:{reset} launching {bold}{}{reset} via BitRouter ({})",
        spec.id,
        base_url,
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );
    if !plan.is_empty() {
        let overrides: Vec<String> = plan.iter().map(|(t, m)| format!("{t}={m}")).collect();
        eprintln!(
            "{cyan}spawn:{reset} model overrides — {}",
            overrides.join(", "),
            cyan = p.cyan,
            reset = p.reset,
        );
    }

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
/// environment: always force the routing base URL, resolve the gateway bearer
/// token by precedence, and append any model-plan tier overrides.
///
/// Returned as an explicit list (rather than mutating the global env) so the
/// logic is unit-testable. `parent_auth` is any auth token the user already
/// exported for the agent; `bitrouter_key` is the value of `BITROUTER_API_KEY`.
/// Auth precedence: `parent_auth` → `bitrouter_key` → placeholder. We always set
/// the auth token (never just inherit) so that an inherited `ANTHROPIC_API_KEY`
/// (which in this repo is the *upstream* Anthropic provider key, not a valid
/// BitRouter inbound credential) cannot accidentally become the auth Claude Code
/// sends to the router. An empty `plan` adds no model vars — the harness then
/// uses its own default models.
fn build_child_env(
    spec: &AgentSpec,
    base_url: &str,
    parent_auth: Option<String>,
    bitrouter_key: Option<String>,
    plan: &ModelPlan,
) -> Vec<(&'static str, String)> {
    let token = parent_auth
        .or(bitrouter_key)
        .unwrap_or_else(|| PLACEHOLDER_API_KEY.to_string());
    let mut env = vec![
        (spec.base_url_env, base_url.to_string()),
        (spec.auth_token_env, token),
    ];
    // Layer the resolved model plan on top: each selected tier becomes the
    // agent's corresponding model env var. A tier the agent does not model
    // (tier_env → None) is skipped.
    for (tier, model) in plan.iter() {
        if let Some(var) = spec.tier_env(tier) {
            env.push((var, model.to_string()));
        }
    }
    env
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

    /// A default (empty) model plan — the common case where no override is set.
    fn empty_plan() -> ModelPlan {
        ModelPlan::default()
    }

    #[test]
    fn child_env_always_sets_base_url() {
        let spec = SpawnAgent::Claude.spec();
        let env = build_child_env(&spec, "http://127.0.0.1:4356", None, None, &empty_plan());
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
            &empty_plan(),
        );
        assert_eq!(auth_token(&explicit).as_deref(), Some("user-token"));

        // No explicit token → fall back to the BitRouter API key.
        let from_key = build_child_env(
            &spec,
            "http://x:1",
            None,
            Some("brk_key".into()),
            &empty_plan(),
        );
        assert_eq!(auth_token(&from_key).as_deref(), Some("brk_key"));

        // Neither set → the local placeholder so the harness still starts.
        let placeholder = build_child_env(&spec, "http://x:1", None, None, &empty_plan());
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
        let env = build_child_env(&spec, "http://x:1", None, None, &empty_plan());
        assert!(env.iter().any(|(k, _)| *k == "ANTHROPIC_AUTH_TOKEN"));
        assert!(env.iter().all(|(k, _)| *k != "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn empty_plan_adds_only_base_url_and_auth() {
        let spec = SpawnAgent::Claude.spec();
        let env = build_child_env(&spec, "http://x:1", None, None, &empty_plan());
        assert_eq!(env.len(), 2, "no model vars for an empty plan");
    }

    #[test]
    fn child_env_appends_resolved_model_plan() {
        let spec = SpawnAgent::Claude.spec();
        let plan = model_plan::resolve(
            SpawnAgent::Claude,
            &bitrouter_sdk::config::SpawnConfig::default(),
            None,
            None,
            None,
            &["low=prov/air".to_string(), "high=prov/big".to_string()],
        )
        .unwrap();
        let env = build_child_env(&spec, "http://x:1", None, None, &plan);
        assert!(
            env.iter()
                .any(|(k, v)| *k == "ANTHROPIC_DEFAULT_HAIKU_MODEL" && v == "prov/air")
        );
        assert!(
            env.iter()
                .any(|(k, v)| *k == "ANTHROPIC_DEFAULT_OPUS_MODEL" && v == "prov/big")
        );
        // The unset `mid` tier must not produce a sonnet var.
        assert!(
            env.iter()
                .all(|(k, _)| *k != "ANTHROPIC_DEFAULT_SONNET_MODEL")
        );
    }
}
