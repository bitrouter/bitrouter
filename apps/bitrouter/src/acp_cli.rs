//! `bitrouter acp` subcommands — headless ACP session surface.
//!
//! Two entry points:
//!
//! - [`serve`] — launch a session and expose it as a vanilla ACP Agent over
//!   **stdio** until the manager disconnects. Used by GUIs and orchestrating
//!   agents that speak ACP.
//!
//! - [`prompt`] — launch a session, subscribe to updates, send one prompt, and
//!   stream each event as a self-describing **NDJSON** line to `out`. Exits
//!   after the prompt resolves (or immediately after submission when `no_wait`
//!   is true).
//!
//! ## NDJSON format
//!
//! Update lines carry the [`SessionUpdateKind`] directly — the `type` tag
//! value is the snake_case variant name (`message_chunk`, `thought_chunk`,
//! `tool_call`, `tool_call_update`). The terminal result line is:
//!
//! ```json
//! {"type":"result","stop_reason":"end_turn"}
//! ```
//!
//! The `stop_reason` value is the ACP wire form (snake_case, via serde) so a
//! downstream parser sees the same spelling the protocol uses.
//!
//! In `--no-wait` mode the only line emitted is:
//!
//! ```json
//! {"type":"submitted"}
//! ```
//!
//! Both functions load their `Config` via the standard resolution chain (see
//! `bitrouter::paths`) and build a [`ConfigAcpRoutingTable`] from
//! `config.agents` — the same table the GUI renderer uses.

use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter_sdk::acp::{AcpAgentConfig, AcpTransport, ConfigAcpRoutingTable};
use bitrouter_sdk::config::Config;
use futures::StreamExt;
use serde::Serialize;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use agent_client_protocol::schema::v1::{
    Cost, LlmProtocol, ProviderCurrentConfig, ProviderId, ProviderInfo, SessionUpdate,
    SetProviderRequest, UsageUpdate,
};
use bitrouter_sdk::acp::engine::LaunchOptions;
use bitrouter_sdk::acp::telemetry::RequestCompleted;
use bitrouter_sdk::acp::translate::SessionUpdateKind;

use crate::paths::ConfigSource;

// ── routing (spawn --via-daemon by default) ─────────────────────────────────────

/// Per-invocation routing decision for a spawned sub-agent. Routing is on by
/// default; `direct` opts out. See `docs/SPAWN_SPEC.md` §5.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct RoutingOptions {
    /// Do NOT route this session's LLM traffic through the daemon — let the
    /// harness use its own provider auth. Routing is attempted by default
    /// when the harness supports headless redirection.
    #[arg(long)]
    pub direct: bool,
    /// Override the gateway base URL (else derived from `server.listen`).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Pin the harness's model (via its model env var / `-c model=`).
    #[arg(long)]
    pub model: Option<String>,
    /// Never auto-start a local daemon when none is running — fail fast.
    #[arg(long)]
    pub no_start: bool,
}

/// The inputs shared by the two sub-agent launch paths ([`serve`] and
/// [`prompt`]): where the config came from, the loaded config, which agent,
/// the session options, and the routing decision. Bundled so each entry point
/// keeps a small, readable signature.
pub struct SpawnContext<'a> {
    /// Where the config was resolved from (daemon socket / auto-start).
    pub source: &'a ConfigSource,
    /// The loaded config (routing overlays its agent entry in place).
    pub config: Config,
    /// The agent id to launch (catalog id or configured entry).
    pub agent_id: &'a str,
    /// Session options (turn timeout).
    pub options: LaunchOptions,
    /// The routing decision (via-daemon by default, or `--direct`).
    pub routing: RoutingOptions,
}

/// A fail-fast routing failure, surfaced BEFORE any session side effect
/// (`docs/SPAWN_SPEC.md` §8). Rendered as a structured NDJSON `error` line in
/// `prompt` mode, or to stderr in `serve` mode.
#[derive(Debug)]
pub enum RoutingError {
    /// The daemon behind `via` did not answer `/health` after auto-start.
    DaemonUnreachable {
        /// The gateway base URL that was probed.
        via: String,
    },
    /// The daemon requires auth and no `BITROUTER_API_KEY` is available.
    AuthRequired {
        /// The gateway base URL that would have been used.
        via: String,
    },
}

impl std::fmt::Display for RoutingError {
    /// Message *and* hint, because this is what a caller renders when the
    /// failure leaves the process as an error value rather than as an
    /// already-printed line. The `ndjson` form keeps them as separate fields
    /// and is unaffected.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}\n  hint: {}", self.message(), self.hint())
    }
}

impl std::error::Error for RoutingError {}

impl RoutingError {
    /// Machine-readable `code` for the NDJSON `error` line.
    fn code(&self) -> &'static str {
        match self {
            RoutingError::DaemonUnreachable { .. } => "daemon_unreachable",
            RoutingError::AuthRequired { .. } => "auth_required",
        }
    }

    /// The gateway base URL this failure concerns.
    fn via(&self) -> &str {
        match self {
            RoutingError::DaemonUnreachable { via } | RoutingError::AuthRequired { via } => via,
        }
    }

    /// One-line remediation hint.
    fn hint(&self) -> &'static str {
        match self {
            RoutingError::DaemonUnreachable { .. } => "run `bitrouter start`, or pass --direct",
            RoutingError::AuthRequired { .. } => {
                "export BITROUTER_API_KEY (or create a key), or pass --direct"
            }
        }
    }

    /// Human message for stderr (`serve`) and the NDJSON `message` field.
    fn message(&self) -> String {
        match self {
            RoutingError::DaemonUnreachable { via } => {
                format!("BitRouter daemon unreachable at {via}")
            }
            RoutingError::AuthRequired { via } => {
                format!("daemon at {via} requires auth but no BITROUTER_API_KEY is set")
            }
        }
    }

    /// The structured NDJSON `error` line for this failure.
    fn ndjson(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "error",
            "code": self.code(),
            "via": self.via(),
            "hint": self.hint(),
            "message": self.message(),
        })
    }
}

/// What routing decided for one session.
#[derive(Debug, Clone, Default)]
pub struct Routed {
    /// The base URL the session's LLM traffic goes through, or `None` when it
    /// runs direct.
    pub via: Option<String>,
    /// The per-session attribution token, when this session's traffic can be
    /// told apart from every other caller's.
    ///
    /// `None` when the caller supplied their own credential: that is real
    /// authentication, and rewriting it to attach attribution would break
    /// `skip_auth: false` — the same rule `spawn.rs` follows. A session
    /// without one must report its spend as daemon-wide rather than implying
    /// a precision it does not have.
    pub launch_id: Option<String>,
}

/// Resolve routing and overlay it onto `config`'s entry for `agent_id`,
/// inserting the bundled-catalog invocation when the id is catalog-known but
/// unconfigured (so `bitrouter spawn claude-acp` works with no YAML edit).
///
/// Returns the "via" base URL when routing is active, or `None` when the
/// session runs direct (`--direct`, an unknown/custom agent, or an
/// unroutable harness — each warned to stderr). Fails fast — before the
/// caller spawns any agent process — on an unreachable daemon or a missing
/// required credential.
pub async fn apply_routing(
    source: &ConfigSource,
    config: &mut Config,
    agent_id: &str,
    opts: &RoutingOptions,
) -> std::result::Result<Routed, RoutingError> {
    // A catalog-known id needs no `agents:` entry — synthesize its invocation.
    if !config.agents.contains_key(agent_id)
        && let Some(h) = crate::harness::by_id(agent_id)
        // Interactive-only harnesses (grok, antigravity) have no ACP
        // adapter to synthesize — the id falls through to not-found.
        && let Some(command) = h.acp_command
    {
        config.agents.insert(
            agent_id.to_string(),
            AcpAgentConfig {
                name: agent_id.to_string(),
                transport: AcpTransport::Stdio {
                    command: command.to_string(),
                    args: h.acp_args.iter().map(|s| s.to_string()).collect(),
                    env: Default::default(),
                },
            },
        );
    }

    // `--model` only takes effect when the daemon route is applied; warn
    // rather than silently drop it on any path that launches direct.
    let warn_model_dropped = |why: &str| {
        if let Some(m) = &opts.model {
            eprintln!("note: --model '{m}' ignored — {why}");
        }
    };

    if opts.direct {
        warn_model_dropped("running --direct");
        return Ok(Routed::default());
    }

    // Match the (now-present-if-known) invocation back to a catalog harness.
    let harness = match config.agents.get(agent_id) {
        Some(entry) => {
            let AcpTransport::Stdio { command, args, .. } = &entry.transport;
            crate::harness::match_invocation(command, args)
        }
        // Unknown agent — let the caller's `Session::launch` surface the
        // configured-agents not-found error.
        None => return Ok(Routed::default()),
    };
    let Some(harness) = harness else {
        eprintln!(
            "note: routing unavailable for '{agent_id}' (not catalog-matched); \
             launching direct — set its `env` to route manually"
        );
        warn_model_dropped("the agent is not catalog-matched");
        return Ok(Routed::default());
    };
    if !harness.env_args_routable() {
        eprintln!(
            "note: '{}' routes via synthesized config, which headless spawn doesn't do yet \
             (`bitrouter launch` does); launching direct",
            harness.id
        );
        warn_model_dropped("the harness routes only in the interactive facet");
        return Ok(Routed::default());
    }

    // Base URL, auth mode, and whether the target is a remote we can't vouch for.
    let base_url = opts
        .base_url
        .clone()
        .unwrap_or_else(|| crate::spawn::derive_base_url(&config.server.listen));
    let target_authority = opts
        .base_url
        .as_deref()
        .and_then(crate::spawn::listen_from_base_url);
    let target_is_local = match &target_authority {
        Some(a) => crate::spawn::listen_is_local(a),
        None => crate::spawn::listen_is_local(&config.server.listen),
    };
    // A remote daemon's `skip_auth` is unknowable here, so require a key.
    let require_key = !target_is_local || !config.server.skip_auth;

    // A harness whose credential isn't Bearer (gemini's `x-goog-api-key`) is
    // rejected by the daemon's auth hook under `skip_auth: false` — warn
    // rather than let the session 401 mid-turn (SPAWN_SPEC §6.3).
    if require_key && !harness.auth_is_bearer() {
        eprintln!(
            "warning: '{}' sends its API key as a non-Bearer header the daemon rejects under \
             auth mode (`skip_auth: false`) — this session will likely 401. Use `skip_auth: \
             true`, a `--direct` session, or a different harness.",
            harness.id
        );
    }

    let explicit_key = crate::spawn::nonempty_env(crate::harness::BITROUTER_API_KEY_ENV);
    let stored_cloud_key = if explicit_key.is_none() && require_key && !target_is_local {
        crate::cloud::cloud_bearer_for_base_url(&base_url).await
    } else {
        None
    };
    // Mirrors `spawn.rs`: a user-supplied credential wins and is never
    // tagged, but a session that would otherwise send the placeholder gets a
    // freshly minted per-launch token instead. That is what makes this
    // session's spend separable from every other caller's — without it the
    // metering store has nothing to group by and cost can only be reported
    // daemon-wide.
    let supplied = explicit_key.or(stored_cloud_key);
    if supplied.is_none() && require_key {
        return Err(RoutingError::AuthRequired { via: base_url });
    }
    let auth = crate::spawn::resolve_launch_token(supplied, None);
    let launch_id = crate::spawn::is_launch_token(&auth).then(|| auth.clone());

    // Daemon liveness: auto-start a local daemon, then probe. Fail fast if the
    // daemon is still unreachable (a routed sub-agent without one is
    // guaranteed-dead) — before any session side effect.
    if opts.base_url.is_none() && target_is_local {
        crate::spawn::ensure_local_daemon(source, config, opts.no_start).await;
    }
    if !crate::spawn::base_url_reachable(&base_url).await {
        return Err(RoutingError::DaemonUnreachable { via: base_url });
    }

    // Compute + apply the overlay. Injection wins over inherited and
    // config-authored env; a config `env:` collision is warned, not silent.
    let overlay = harness.routing_overlay(&base_url, &auth, opts.model.as_deref());
    if let Some(entry) = config.agents.get_mut(agent_id) {
        let AcpTransport::Stdio { args, env, .. } = &mut entry.transport;
        for (k, v) in overlay.env {
            if let Some(existing) = env.get(&k)
                && existing != &v
            {
                eprintln!(
                    "note: routing overrides your `env.{k}` for '{agent_id}' \
                     (pass --direct to keep your value)"
                );
            }
            env.insert(k, v);
        }
        args.extend(overlay.args);
    }
    Ok(Routed {
        via: Some(base_url),
        launch_id,
    })
}

// ── NDJSON helpers ────────────────────────────────────────────────────────────

/// Terminal result line emitted after the prompt resolves.
///
/// Generic over the stop-reason type so the ACP `StopReason` (which derives
/// `serde::Serialize` with snake_case rename) renders its wire form directly —
/// `"end_turn"`, not the Rust `Debug` spelling `"EndTurn"`. Keeping it generic
/// also avoids naming `agent_client_protocol_schema` here (it isn't a direct
/// dependency of this crate).
#[derive(Serialize)]
struct ResultLine<S: Serialize> {
    #[serde(rename = "type")]
    kind: &'static str,
    stop_reason: S,
    /// Under `--result-schema`: the extracted, schema-valid result object —
    /// or JSON `null` when extraction/validation failed after the one repair
    /// re-prompt. Omitted entirely without the flag (byte-compatible).
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    /// Under `--result-schema`: whether `result` satisfied the schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_ok: Option<bool>,
    /// Under `--result-schema`, on failure only: the last reply's raw text so
    /// the orchestrator is never blocked.
    #[serde(skip_serializing_if = "Option::is_none")]
    raw: Option<String>,
}

/// Write one NDJSON line (JSON + `\n`) to `out`.
async fn write_ndjson_line<W, T>(out: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_string(value).context("serialising NDJSON line")?;
    line.push('\n');
    out.write_all(line.as_bytes())
        .await
        .context("writing NDJSON line")
}

/// `bitrouter spawn <agent> --check` — preflight the harness resolution, the
/// routing decision, and (when routing) daemon reachability, without launching
/// anything or auto-starting a daemon. Read-only.
pub async fn spawn_check(
    config: Config,
    agent_id: &str,
    routing: &RoutingOptions,
) -> Result<crate::spawn::SpawnCheckReport> {
    use crate::spawn::{SpawnCheckReport, SpawnCheckRow, SpawnCheckStatus};

    let row = |name: &str, status: SpawnCheckStatus, message: String| SpawnCheckRow {
        name: name.to_string(),
        status,
        message,
    };
    let mut checks = Vec::new();

    // 1. The agent resolves — either configured or a bundled-catalog id.
    let configured = config.agents.get(agent_id);
    let catalog = crate::harness::by_id(agent_id);
    let (command, args) = match (configured, catalog) {
        (Some(entry), _) => {
            let AcpTransport::Stdio { command, args, .. } = &entry.transport;
            checks.push(row(
                "agent",
                SpawnCheckStatus::Pass,
                format!("configured in agents: ({command})"),
            ));
            (command.clone(), args.clone())
        }
        (None, Some(h)) => match h.acp_command {
            Some(command) => {
                checks.push(row(
                    "agent",
                    SpawnCheckStatus::Pass,
                    format!("bundled catalog ({} {})", command, h.acp_args.join(" ")),
                ));
                (
                    command.to_string(),
                    h.acp_args.iter().map(|s| s.to_string()).collect(),
                )
            }
            None => {
                checks.push(row(
                    "agent",
                    SpawnCheckStatus::Fail,
                    format!(
                        "'{agent_id}' is interactive-only (no ACP adapter) — use `bitrouter launch --agent {}`",
                        h.interactive_binary.unwrap_or(agent_id)
                    ),
                ));
                (String::new(), Vec::new())
            }
        },
        (None, None) => {
            checks.push(row(
                "agent",
                SpawnCheckStatus::Fail,
                format!("'{agent_id}' is neither a configured agent nor a bundled-catalog id"),
            ));
            (String::new(), Vec::new())
        }
    };

    // 2. Routing decision.
    let base_url = routing
        .base_url
        .clone()
        .unwrap_or_else(|| crate::spawn::derive_base_url(&config.server.listen));
    let harness = crate::harness::match_invocation(&command, &args);
    let mut routable_harness: Option<&'static crate::harness::Harness> = None;
    if routing.direct {
        checks.push(row(
            "routing",
            SpawnCheckStatus::Warn,
            "--direct: the sub-agent uses its own provider auth".to_string(),
        ));
    } else {
        match harness {
            Some(h) if h.env_args_routable() => {
                routable_harness = Some(h);
                checks.push(row(
                    "routing",
                    SpawnCheckStatus::Pass,
                    format!("via daemon {base_url} [{}]", h.id),
                ));
            }
            Some(h) => {
                checks.push(row(
                    "routing",
                    SpawnCheckStatus::Warn,
                    format!(
                        "'{}' routes via synthesized config (interactive facet only); \
                         will run direct",
                        h.id
                    ),
                ));
            }
            None => checks.push(row(
                "routing",
                SpawnCheckStatus::Warn,
                "not catalog-matched; will run direct (set its `env` to route manually)"
                    .to_string(),
            )),
        }
    }

    // 3. Auth preflight — mirror `apply_routing`'s require_key so `--check`
    //    surfaces the same `auth_required` gate the launch would fail fast on.
    if let Some(h) = routable_harness {
        let target_is_local = routing
            .base_url
            .as_deref()
            .and_then(crate::spawn::listen_from_base_url)
            .map(|a| crate::spawn::listen_is_local(&a))
            .unwrap_or_else(|| crate::spawn::listen_is_local(&config.server.listen));
        let require_key = !target_is_local || !config.server.skip_auth;
        let has_key = crate::spawn::nonempty_env(crate::harness::BITROUTER_API_KEY_ENV).is_some();
        checks.push(if require_key && !has_key {
            row(
                "auth",
                SpawnCheckStatus::Fail,
                "daemon requires auth but BITROUTER_API_KEY is not set — export it or pass --direct"
                    .to_string(),
            )
        } else if require_key && !h.auth_is_bearer() {
            row(
                "auth",
                SpawnCheckStatus::Warn,
                format!(
                    "'{}' sends a non-Bearer header the daemon rejects under skip_auth:false — \
                     the session will likely 401",
                    h.id
                ),
            )
        } else if has_key {
            row(
                "auth",
                SpawnCheckStatus::Pass,
                "BITROUTER_API_KEY present".to_string(),
            )
        } else {
            row(
                "auth",
                SpawnCheckStatus::Pass,
                "skip_auth: credential-less requests admitted".to_string(),
            )
        });
    }

    // 4. Daemon reachability — only meaningful when routing is active. Read-only
    //    (no auto-start): `--check` observes, it does not mutate.
    if routable_harness.is_some() {
        checks.push(if crate::spawn::base_url_reachable(&base_url).await {
            row(
                "daemon",
                SpawnCheckStatus::Pass,
                format!("{base_url} is reachable"),
            )
        } else {
            row(
                "daemon",
                SpawnCheckStatus::Fail,
                format!("{base_url} is unreachable — run `bitrouter start` (or pass --direct)"),
            )
        });
    }

    Ok(SpawnCheckReport {
        agent: agent_id.to_string(),
        base_url,
        model: routing.model.clone(),
        checks,
    })
}

// ── serve ─────────────────────────────────────────────────────────────────────

/// Launch a session for `agent_id` and serve it as a vanilla ACP Agent over
/// **stdio** until the manager disconnects.
///
/// Config is taken by value (already loaded by the caller); `options` carries
/// the per-turn timeout resolved from the CLI flags (see [`launch_options`]).
pub async fn serve(ctx: SpawnContext<'_>) -> Result<()> {
    let SpawnContext {
        source,
        mut config,
        agent_id,
        options,
        routing,
    } = ctx;
    // Route the sub-agent's LLM traffic through the daemon (default) unless
    // opted out. Fail fast — before speaking any ACP — so a manager handles
    // "child failed to start" rather than a mid-session provider error.
    //
    // Returned, not `exit(1)`: a caller that never sees a value cannot render
    // one, and the shutdown path below is skipped either way because nothing
    // has been launched yet. `run_acp` renders it to stderr.
    let routed = apply_routing(source, &mut config, agent_id, &routing)
        .await
        .map_err(anyhow::Error::new)?;
    let catalog = catalog_from_config(&config)?;
    let cwd = std::env::current_dir().context("resolving current directory")?;
    // Deferred open: the upstream `session/new` runs when the manager sends
    // its own `session/new`, so the manager's cwd + mcpServers are relayed.
    let session = bitrouter_sdk::acp::engine::Session::launch_deferred(
        &catalog,
        agent_id,
        cwd.clone(),
        options,
    )
    .await
    .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    // The router-owned surfaces: measured cost (§7) and the routing catalog
    // (§6). Both are wired before serving so the first turn and the manager's
    // first `providers/list` already have somewhere to go.
    let (cost_tx, cost_rx) = tokio::sync::mpsc::unbounded_channel();
    let exporter = attach_observability(
        &config,
        agent_id,
        &session,
        Some(CostSink {
            source: source.clone(),
            session_start: chrono::Utc::now(),
            launch_id: routed.launch_id.clone(),
            updates: cost_tx,
        }),
    )
    .await;
    let providers = Arc::new(SessionProviders::new(
        &config,
        // Both halves are required, and their absence is the difference
        // between a picker that works and one that lies: a daemon to install
        // the override in, and a launch id to scope it by.
        routed
            .via
            .as_ref()
            .and(routed.launch_id.as_ref())
            .map(|id| RouteControl {
                socket: crate::daemon::resolve_socket_path(
                    source.home().join("bitrouter.yaml").as_path(),
                    &config.server.control_socket,
                ),
                launch_id: id.clone(),
            }),
    ));
    let extensions = bitrouter_sdk::acp::down::ServeExtensions {
        injected_updates: Some(cost_rx),
        providers: Some(
            Arc::clone(&providers) as Arc<dyn bitrouter_sdk::acp::down::ProviderSurface>
        ),
    };
    let session = Arc::new(session);

    let served = bitrouter_sdk::acp::down::serve_with(Arc::clone(&session), extensions).await;

    // The override belongs to this session; drop it before anything else so a
    // later launch cannot inherit a route it never asked for.
    providers.release().await;

    // No manager left: shut the session down deliberately so the agent child
    // is reaped (same semantics as `prompt`). Once serving ends, the forwarding
    // tasks have released their clones, so we are the sole owner.
    match Arc::try_unwrap(session) {
        Ok(session) => session
            .shutdown()
            .await
            .context("shutting down acp session")?,
        Err(_) => tracing::warn!("session still referenced after serve; skipping shutdown"),
    }
    if let Some(exporter) = exporter {
        // Flush the span batch before exit; spans are lost otherwise.
        exporter.shutdown();
    }
    served.map_err(|e| anyhow::anyhow!("acp serve: {e}"))
}

// ── chat ──────────────────────────────────────────────────────────────────────

/// This process's session log, once the subscriber has opened one.
///
/// A global because the path is decided during subscriber init — before any
/// command is dispatched, and the only moment it exists. A session that ends
/// badly needs it much later, and threading it through every launch path to
/// serve the failure case would cost more than it is worth.
static SESSION_LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Record where this process's session log lives. Called once at startup.
pub fn remember_session_log(path: std::path::PathBuf) {
    let _ = SESSION_LOG.set(path);
}

/// Show the end of the session log and name the file.
///
/// Called only when something went wrong: a permanent pane would cost rows on
/// every session to serve the rare one that fails. An unreadable log is
/// reported as unreadable rather than skipped — "there is no log" is the kind
/// of thing a user needs told, not hidden.
fn session_log_tail() -> Vec<ratatui::text::Line<'static>> {
    let Some(path) = SESSION_LOG.get() else {
        return Vec::new();
    };
    let log = match std::fs::read_to_string(path) {
        Ok(log) => log,
        Err(e) => format!("(could not read the session log: {e})"),
    };
    bitrouter_tui::log_tail::render(path, &log, bitrouter_tui::log_tail::TAIL_LINES)
}

/// Read the journal, whatever the last holder of the lock did.
///
/// A poisoned lock costs a frame at most: the journal is plain data, a panic
/// mid-`apply` leaves it merely stale, and refusing to draw at all would turn
/// one lost update into a dead session.
fn journal(
    shared: &std::sync::Mutex<bitrouter_tui::journal::Journal>,
) -> std::sync::MutexGuard<'_, bitrouter_tui::journal::Journal> {
    match shared.lock() {
        Ok(journal) => journal,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The live view of one chat session.
///
/// The document is the journal; this owns the two things the journal cannot
/// know about — the footer the app composes (§4.5) and the terminal the writer
/// paints into.
struct View {
    writer: bitrouter_tui::writer::Writer<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    cache: bitrouter_tui::writer::Cache,
    registry: bitrouter_tui::render::Registry,
    /// How this session is routed. Fixed for its life unless `/route` changes
    /// it, and absent when the session is direct.
    route: Option<String>,
    /// The most recent thing the client itself has to say — a stop reason, a
    /// route change, a failed turn. One row, replaced rather than accumulated:
    /// it is about the last thing that happened, and the session log is where
    /// the history lives.
    notice: Option<ratatui::text::Line<'static>>,
    /// A row owned by an open modal. Today only the provider picker.
    modal: Option<ratatui::text::Line<'static>>,
    /// The line being typed. Raw mode means nothing is echoed unless the
    /// footer echoes it.
    input: String,
}

impl View {
    /// Take the terminal.
    ///
    /// Called **before** the session's stdin owner exists: the writer reads
    /// the cursor once at construction, which on a real terminal is a DSR
    /// query, and a reader already sitting on stdin would take the answer.
    fn open(route: Option<String>) -> Result<Self> {
        let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
        Ok(Self {
            writer: bitrouter_tui::writer::Writer::new(backend).context("opening the writer")?,
            cache: bitrouter_tui::writer::Cache::default(),
            registry: bitrouter_tui::render::Registry::default(),
            route,
            notice: None,
            modal: None,
            input: String::new(),
        })
    }

    /// The rows below the document: what this session costs and where it is
    /// going, then whatever is currently being asked or typed.
    ///
    /// Re-emitted as the document's tail on every frame, so it is always the
    /// newest content and always in the live region — which is what bounds the
    /// stale-row cost of §4.3 to individual scrolled-off tool calls rather
    /// than to the session's own summary.
    fn footer(
        &self,
        journal: &bitrouter_tui::journal::Journal,
    ) -> Vec<ratatui::text::Line<'static>> {
        use ratatui::text::{Line, Span};

        let cost = journal
            .usage()
            .and_then(crate::chat::cost::Cost::from_usage)
            .map_or_else(crate::chat::cost::unreported, |cost| cost.render());
        let mut status: Vec<Span<'static>> = cost.spans;
        if let Some(route) = &self.route {
            status.push(Span::raw(format!(" · via {route}")));
        }
        if let Some(mode) = journal.mode() {
            status.push(Span::raw(format!(" · {mode}")));
        }
        if let Some(title) = journal.title() {
            status.push(Span::raw(format!(" · {title}")));
        }

        let mut rows = vec![Line::from(status)];
        if let Some(notice) = &self.notice {
            rows.push(notice.clone());
        }
        // A question waiting on a person outranks anything else down here.
        if let Some(prompt) = journal.pending_permission() {
            rows.push(prompt.render());
        }
        if let Some(modal) = &self.modal {
            rows.push(modal.clone());
        }
        rows.push(Line::from(format!("> {}", self.input)));
        rows
    }

    /// Paint one frame.
    ///
    /// Every caller of this has already decided the frame is worth painting —
    /// the writer still writes nothing when the document is unchanged, so a
    /// redundant call costs a render and no output.
    fn paint(&mut self, shared: &std::sync::Mutex<bitrouter_tui::journal::Journal>) -> Result<()> {
        let size = self.writer.size();
        let mut journal = journal(shared);
        let footer = self.footer(&journal);
        let document = self.cache.document(&journal, &self.registry, size, &footer);
        journal.painted();
        // The lock is not held across the write: a frame can take a
        // millisecond, and the pump has updates to apply.
        drop(journal);
        self.writer.frame(&document).context("painting a frame")
    }

    /// Leave the cursor below the document. Nothing rendered is removed.
    fn finish(&mut self) -> Result<()> {
        self.writer.finish().context("closing the view")
    }
}

/// Run one ACP session interactively in the caller's terminal.
///
/// The interactive counterpart to [`serve`]: same launch, same routing, same
/// per-session log — but the session is rendered for a person instead of
/// exposed to a manager over stdio.
///
/// This owns argument handling, routing, and session lifetime; the rendering
/// belongs to `bitrouter-tui`, which cannot depend on this crate and so is
/// handed only what ACP carries.
pub async fn chat(ctx: SpawnContext<'_>) -> Result<()> {
    use std::io::IsTerminal as _;

    let SpawnContext {
        source,
        mut config,
        agent_id,
        options,
        routing,
    } = ctx;
    // Fail fast, before any agent process exists — a person waiting at a
    // prompt should learn the route is dead now, not mid-turn.
    let routed = apply_routing(source, &mut config, agent_id, &routing)
        .await
        .map_err(anyhow::Error::new)?;

    let catalog = catalog_from_config(&config)?;
    let cwd = std::env::current_dir().context("resolving current directory")?;
    let session = bitrouter_sdk::acp::engine::Session::launch(&catalog, agent_id, cwd, options)
        .await
        .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let exporter = attach_observability(&config, agent_id, &session, None).await;

    match &routed.via {
        Some(via) => eprintln!("chat: '{agent_id}' routed via bitrouter ({via})"),
        None => eprintln!("chat: '{agent_id}' running direct (not routed, not metered)"),
    }

    // A pipe cannot be drawn on. Everything below this point — the live row,
    // the modals, raw mode — assumes a screen with a cursor on it, and writing
    // those escapes into a file or a `| grep` would corrupt the very output
    // the redirect exists to capture. So a redirected stdout gets the session
    // as plain text instead, which is the one thing a pipe *can* use.
    if !std::io::stdout().is_terminal() {
        let ended = chat_plain(session).await;
        if let Some(exporter) = exporter {
            exporter.shutdown();
        }
        return ended;
    }

    // The picker exists only when this session can actually be rerouted, which
    // needs both a daemon and an attributable launch (task 4.2). Probing that
    // here rather than assuming it is what keeps a dead control off the screen.
    let providers = SessionProviders::new(
        &config,
        routed
            .via
            .as_ref()
            .and(routed.launch_id.as_ref())
            .map(|id| RouteControl {
                socket: crate::daemon::resolve_socket_path(
                    source.home().join("bitrouter.yaml").as_path(),
                    &config.server.control_socket,
                ),
                launch_id: id.clone(),
            }),
    );
    let routable = providers.can_reroute();
    if routable {
        eprintln!("chat: type /route to change provider mid-session.");
    }
    eprintln!("chat: type a message and press enter; Ctrl-D to end the session.");

    // The view opens **before** the stdin owner: the writer reads the cursor
    // once, here, and on a real terminal that is a DSR query whose answer a
    // reader already sitting on stdin would take.
    let mut view = View::open(routed.via.clone())?;

    // Raw mode starts here, after the last plain `eprintln!` — a cooked
    // newline in a raw terminal does not return the carriage. From this point
    // the terminal echoes nothing and delivers no SIGINT: both are ours.
    let mut stdin = crate::chat::input::Stdin::open().context("taking the terminal for input")?;
    // The three exits, all landing at `lifecycle::restore()`: `Stdin`'s drop
    // covers the normal one and every `?` on the way out, the panic hook
    // covers the second, and `Shutdown` is the third — a signal runs no Rust
    // the loop controls, so it has to be awaited rather than caught.
    crate::tui::lifecycle::install_panic_restore();
    let mut shutdown = crate::tui::lifecycle::Shutdown::install();

    // Permission requests block the turn until a person answers. They are the
    // journal's, but they arrive on their own channel rather than as updates.
    let (permission_tx, mut permission_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut pending = session.permissions();
    let permissions = tokio::spawn(async move {
        while let Some(request) = pending.next().await {
            if permission_tx.send(request).is_err() {
                break;
            }
        }
    });

    // The **raw** ACP stream, not the translated one: the journal is a
    // protocol client, and translating first would lose exactly the fidelity
    // it exists to retain.
    let mut updates = session.raw_updates();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(
        bitrouter_tui::journal::Journal::default(),
    ));
    // A signal, not a payload: the pump has already applied the update by the
    // time this arrives, so the loop's only job is to decide when to paint.
    let (dirty_tx, mut dirty_rx) = tokio::sync::mpsc::unbounded_channel();
    let pump_journal = std::sync::Arc::clone(&shared);
    let pump = tokio::spawn(async move {
        while let Some(update) = updates.next().await {
            journal(&pump_journal).apply(update);
            if dirty_tx.send(()).is_err() {
                return;
            }
        }
    });

    // Every prompt gets its own id so two in a row cannot merge into one run —
    // which they otherwise would, if the agent answered the first with
    // nothing at all.
    let mut prompts = 0_usize;
    let mut abnormal = false;

    'session: loop {
        view.input.clear();
        view.paint(&shared)?;
        let read = tokio::select! {
            // Raw mode means nothing is echoed unless the footer echoes it.
            read = stdin.read_line(|typed| {
                view.input = typed.to_string();
                view.paint(&shared)
            }) => read?,
            // A signal at an idle prompt ends the session the same way Ctrl-D
            // does — through teardown, not around it.
            () = shutdown.recv() => break 'session,
        };
        let line = match read {
            crate::chat::input::Prompt::Line(line) => line,
            crate::chat::input::Prompt::End => break,
        };
        view.input.clear();
        // The last turn's word stands until this one starts, so a stop reason
        // is readable for as long as the reader is deciding what to say next
        // rather than for the one frame between them.
        view.notice = None;
        // A line of exactly `/route` opens the picker. Only offered when the
        // session can honour it — see `can_reroute`.
        if line.trim() == "/route" {
            if routable {
                pick_provider(&mut view, &shared, &mut stdin, &providers).await?;
            } else {
                view.notice = Some(ratatui::text::Line::from(
                    "this session cannot be rerouted (running direct, or its credential is \
                     its own and cannot be attributed)",
                ));
                view.paint(&shared)?;
            }
            continue;
        }
        // The prompt joins the document in the user's own voice, so it scrolls
        // with the answer it asked for.
        prompts = prompts.saturating_add(1);
        journal(&shared).apply(SessionUpdate::UserMessageChunk(prompt_chunk(
            &line, prompts,
        )));
        view.paint(&shared)?;

        let turn = session.prompt(&line);
        tokio::pin!(turn);
        // The tick is the streaming frame budget. It lives here rather than at
        // the prompt because streaming is the only thing that needs
        // coalescing: a keystroke and a permission paint immediately.
        let mut ticker = tokio::time::interval(bitrouter_tui::writer::Schedule::INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut schedule = bitrouter_tui::writer::Schedule::default();
        // Both are handled *after* the `select!` rather than inside a branch:
        // the branch bodies run while the arms' futures are still borrowed,
        // and one of those arms holds `stdin` — which is exactly what
        // answering a permission needs.
        let mut requested = None;
        let mut interrupted = false;
        let outcome = loop {
            let mut paint = false;
            tokio::select! {
                update = dirty_rx.recv() => match update {
                    // Already in the journal; the tick decides when it is seen.
                    Some(()) => {
                        schedule.wake(
                            bitrouter_tui::writer::Trigger::Update,
                            std::time::Instant::now(),
                        );
                    }
                    None => continue,
                },
                _ = ticker.tick() => {
                    paint = schedule.wake(
                        bitrouter_tui::writer::Trigger::Tick,
                        std::time::Instant::now(),
                    );
                },
                request = permission_rx.recv() => match request {
                    Some(request) => requested = Some(request),
                    None => continue,
                },
                event = stdin.next_event() => match event {
                    // Ctrl-C during a turn is a cancel, not an exit: the
                    // session survives it and the next prompt is drawn.
                    Some(event) => interrupted = crate::chat::input::is_interrupt(&event),
                    // The terminal went away. Nothing can answer the rest of
                    // this turn, so stop waiting on it.
                    None => interrupted = true,
                },
                result = &mut turn => break Some(result),
                // Mid-turn, a signal still leaves by the front door: the agent
                // is shut down and the terminal restored on the way out.
                () = shutdown.recv() => break 'session,
            }
            if paint {
                view.paint(&shared)?;
            }
            if let Some(request) = requested.take() {
                answer_permission(&mut view, &shared, &mut stdin, request).await?;
                schedule.wake(
                    bitrouter_tui::writer::Trigger::Permission,
                    std::time::Instant::now(),
                );
            }
            if interrupted {
                break None;
            }
        };
        // Whatever the pump applied between its last signal and the turn
        // resolving is already in the journal; draining the signals is what
        // makes the frame below include it.
        while dirty_rx.try_recv().is_ok() {}
        view.notice = Some(match outcome {
            // Dropping the turn future stops this side waiting; the agent is
            // told to stop by `session/cancel`, which is a later task. Saying
            // "abandoned" rather than "cancelled" is the honest difference.
            None => ratatui::text::Line::from("[turn abandoned]"),
            Some(Ok(response)) => {
                ratatui::text::Line::from(format!("[{:?}]", response.stop_reason))
            }
            Some(Err(e)) => {
                // A failed turn is the abnormal exit the log tail exists for;
                // it is written after teardown, when the terminal is the
                // shell's again.
                abnormal = true;
                ratatui::text::Line::from(format!("turn failed: {e}"))
            }
        });
        // A settled turn is immediate: it is the moment the reader is waiting
        // for, not streaming noise.
        schedule.wake(
            bitrouter_tui::writer::Trigger::TurnSettled,
            std::time::Instant::now(),
        );
        view.paint(&shared)?;
    }

    pump.abort();
    permissions.abort();
    // A session whose agent could not be shut down cleanly ended abnormally
    // too, and the log is where the reason is.
    let shutdown = session.shutdown().await;
    abnormal = abnormal || shutdown.is_err();
    // Leave the cursor below the document, then give the terminal back —
    // in that order, because the log tail below is written as ordinary
    // output and must land in a terminal that is the shell's again.
    view.finish()?;
    drop(stdin);
    if abnormal {
        write_plain(&mut std::io::stdout(), &session_log_tail())?;
    }
    shutdown.context("shutting down chat session")?;
    if let Some(exporter) = exporter {
        exporter.shutdown();
    }
    Ok(())
}

/// The user's own prompt, as the update the agent would have sent for it.
///
/// Keyed, because the journal's sticky rule continues an open run on an
/// unkeyed chunk: two prompts in a row with no answer between them would
/// otherwise become one paragraph.
fn prompt_chunk(line: &str, nth: usize) -> agent_client_protocol::schema::v1::ContentChunk {
    use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, MessageId, TextContent};

    let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(line.to_string())));
    chunk.message_id = Some(MessageId::from(format!("chat:prompt:{nth}")));
    chunk
}

/// A rendered line as plain text: the spans concatenated, every style
/// dropped.
///
/// The styles are the whole difference between the two paths. A `Line` is
/// only escape sequences once a backend writes it; written this way it never
/// becomes any.
fn plain_text(line: &ratatui::text::Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn write_plain(
    out: &mut impl std::io::Write,
    lines: &[ratatui::text::Line<'static>],
) -> Result<()> {
    for line in lines {
        // `writeln!` rather than `println!`: a closed pipe makes `println!`
        // panic, and `bitrouter chat … | head` closes the pipe as a matter of
        // course.
        writeln!(out, "{}", plain_text(line)).context("writing the session to stdout")?;
    }
    Ok(())
}

/// The same session, for a stdout that is not a terminal.
///
/// Deliberately not a second renderer: it is the same journal and the same
/// renderers, printed without a backend. What it drops is everything that
/// needs a screen — the footer, the picker, raw mode, painting in place — and
/// what it keeps is the transcript, which is the part a pipe can use.
///
/// The document is written **once per turn**, from the row after the last one
/// written. A pipe cannot take a row back, so nothing is emitted until the
/// turn that produces it has settled — which is also what makes in-place
/// patching arrive as one finished tool call rather than three.
///
/// A permission request is denied rather than asked, because there is nobody
/// to ask: the terminal that would carry the question is the one that isn't
/// there. Denying is the same answer this path gives an unanswerable prompt
/// anywhere else, and it is never mistaken for consent.
async fn chat_plain(session: bitrouter_sdk::acp::engine::Session) -> Result<()> {
    use std::io::Write as _;

    use agent_client_protocol::schema::v1::{RequestPermissionOutcome, SelectedPermissionOutcome};
    use futures::FutureExt as _;
    use tokio::io::AsyncBufReadExt as _;

    /// The terminal a pipe does not have. Eighty columns is the convention
    /// every tool that has had to guess has guessed.
    const PIPED: ratatui::layout::Size = ratatui::layout::Size {
        width: 80,
        height: 24,
    };

    let mut out = std::io::stdout();
    let mut journal = bitrouter_tui::journal::Journal::default();
    let mut cache = bitrouter_tui::writer::Cache::default();
    let registry = bitrouter_tui::render::Registry::default();
    // How many rows of the document have already been written.
    let mut written = 0_usize;
    let mut prompts = 0_usize;
    let mut updates = session.raw_updates();
    let mut permissions = session.permissions();
    // The only reader of stdin on this path, and it takes no raw mode — the
    // one that does (`chat::input::Stdin`) needs a terminal, which is exactly
    // what is missing here.
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    let ended = loop {
        let Some(line) = lines.next_line().await.context("reading stdin")? else {
            break Ok(());
        };
        if line.trim().is_empty() {
            continue;
        }
        if line.trim() == "/route" {
            writeln!(out, "/route needs a terminal").context("writing to stdout")?;
            continue;
        }
        prompts = prompts.saturating_add(1);
        journal.apply(SessionUpdate::UserMessageChunk(prompt_chunk(
            &line, prompts,
        )));

        let turn = session.prompt(&line);
        tokio::pin!(turn);
        let outcome = loop {
            tokio::select! {
                update = updates.next() => if let Some(update) = update {
                    journal.apply(update);
                },
                request = permissions.next() => if let Some(request) = request {
                    let prompt = bitrouter_tui::permission::Prompt::new(
                        request.tool_call.fields.title.clone(),
                        request.tool_call.tool_call_id.0.to_string(),
                        request.options.clone(),
                    );
                    request.resolve(match prompt.deny() {
                        Some(id) => RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(id),
                        ),
                        None => RequestPermissionOutcome::Cancelled,
                    });
                    writeln!(out, "  permission denied: no terminal to ask on")
                        .context("writing the permission decision")?;
                },
                result = &mut turn => break result,
            }
        };
        // Whatever the agent emitted between its last update and the turn
        // resolving. There is nothing to flush: the journal has no buffered
        // state, which is the whole reason it replaced `Transcript`.
        while let Some(Some(update)) = updates.next().now_or_never() {
            journal.apply(update);
        }
        let document = cache.document(&journal, &registry, PIPED, &[]);
        write_plain(&mut out, document.get(written..).unwrap_or_default())?;
        written = document.len();
        journal.painted();
        match outcome {
            Ok(response) => {
                writeln!(out, "[{:?}]", response.stop_reason).context("writing the stop reason")?
            }
            Err(e) => {
                writeln!(out, "turn failed: {e}").context("writing the failure")?;
                write_plain(&mut out, &session_log_tail())?;
            }
        }
    };

    let shutdown = session.shutdown().await;
    if shutdown.is_err() {
        write_plain(&mut out, &session_log_tail())?;
    }
    ended.and(shutdown.context("shutting down chat session"))
}

/// Draw the provider picker and, on a selection, actually change the route.
///
/// The order matters: `providers/set` is issued first and the list is re-read
/// afterwards, so what the user ends up seeing is the route the daemon is
/// serving rather than the one they asked for. A `set` that fails leaves the
/// old route marked, and says why.
async fn pick_provider(
    view: &mut View,
    shared: &std::sync::Mutex<bitrouter_tui::journal::Journal>,
    stdin: &mut crate::chat::input::Stdin,
    providers: &SessionProviders,
) -> Result<()> {
    use bitrouter_sdk::acp::down::ProviderSurface;
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    let listed = providers.list().await;
    let Some(picker) = crate::chat::picker::Picker::open(true, &listed) else {
        view.notice = Some(ratatui::text::Line::from(
            "no routable providers to choose between",
        ));
        view.paint(shared)?;
        return Ok(());
    };
    // The picker is a footer row for as long as it is open, repainted in
    // place like everything else down there.
    view.modal = Some(picker.render());
    view.paint(shared)?;

    // Keys come from the session's one stdin owner, which already holds raw
    // mode: a modal that took and dropped it would be a second owner, and the
    // window between them is where a keystroke gets lost.
    let chosen = loop {
        match stdin.next_event().await {
            Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                // A control chord is never a choice — Ctrl-C closes the
                // picker instead of selecting whatever `c` happens to be.
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    break None;
                }
                match key.code {
                    KeyCode::Char(c) => {
                        if let Some(id) = picker.choose(c) {
                            break Some(id);
                        }
                    }
                    KeyCode::Esc => break None,
                    _ => {}
                }
            }
            // The terminal went away mid-question; the route is unchanged.
            None => break None,
            Some(_) => {}
        }
    };

    view.modal = None;
    let Some(id) = chosen else {
        view.notice = Some(ratatui::text::Line::from("route unchanged"));
        view.paint(shared)?;
        return Ok(());
    };

    // Attempt it, then report what is actually in force — never what was asked
    // for. `providers/set` can legitimately refuse.
    let request = agent_client_protocol::schema::v1::SetProviderRequest::new(
        agent_client_protocol::schema::v1::ProviderId::new(id.clone()),
        LlmProtocol::Other(String::new()),
        String::new(),
    );
    match providers.set(request).await {
        Ok(()) => {
            let confirmed = providers.list().await;
            let in_force = confirmed
                .iter()
                .find(|p| p.current.is_some())
                .map(|p| p.provider_id.0.to_string())
                .unwrap_or_else(|| "unchanged".to_string());
            view.notice = Some(ratatui::text::Line::from(format!("route: {in_force}")));
            // The footer names the route for the rest of the session, not just
            // for this frame.
            view.route = Some(in_force);
        }
        Err(e) => {
            view.notice = Some(ratatui::text::Line::from(format!("route unchanged: {e}")));
        }
    }
    view.paint(shared)
}

/// Draw a permission request and block on the user's answer.
///
/// Reads keys directly rather than through the line-based stdin loop: a
/// permission answer is one keystroke, and requiring enter would make the
/// fast path — deny — slower than the dangerous one.
///
/// Every path that is not an explicit choice resolves to deny or cancel.
/// A prompt that resolved ambiguity as consent would be worse than no prompt.
async fn answer_permission(
    view: &mut View,
    shared: &std::sync::Mutex<bitrouter_tui::journal::Journal>,
    stdin: &mut crate::chat::input::Stdin,
    request: bitrouter_sdk::acp::up::PendingPermission,
) -> Result<()> {
    use agent_client_protocol::schema::v1::{RequestPermissionOutcome, SelectedPermissionOutcome};
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    let prompt = bitrouter_tui::permission::Prompt::new(
        request.tool_call.fields.title.clone(),
        request.tool_call.tool_call_id.0.to_string(),
        request.options.clone(),
    );
    // The journal holds the open question, so every frame drawn while it is
    // open shows it — including one painted by something else entirely.
    journal(shared).set_pending_permission(Some(prompt.clone()));
    view.paint(shared)?;

    let deny = || match prompt.deny() {
        Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
        // The agent offered no way to say no; cancelling is the honest answer
        // and is never mistaken for consent.
        None => RequestPermissionOutcome::Cancelled,
    };

    // Keys come from the session's one stdin owner. It already holds raw mode
    // for the whole session, so there is no mode to take here and none to
    // leave behind if this future is dropped.
    let outcome = loop {
        match stdin.next_event().await {
            Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                // Ctrl-C answers the question the only way an interrupt can be
                // read: no. Passing the chord's letter to `choose` could
                // select an option, which is the one outcome a cancel must
                // never produce.
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    break deny();
                }
                match key.code {
                    KeyCode::Char(c) => {
                        if let Some(id) = prompt.choose(c) {
                            break RequestPermissionOutcome::Selected(
                                SelectedPermissionOutcome::new(id),
                            );
                        }
                    }
                    KeyCode::Esc => break deny(),
                    _ => {}
                }
            }
            // The terminal went away mid-question. Deny — an unanswerable
            // prompt must not become an allow.
            None => break deny(),
            Some(_) => {}
        }
    };

    let chosen = matches!(outcome, RequestPermissionOutcome::Selected(_));
    request.resolve(outcome);
    // Answered: the question stops being asked, and what was decided is said
    // once rather than left on screen as though it were still open.
    journal(shared).set_pending_permission(None);
    view.notice = Some(ratatui::text::Line::from(if chosen {
        "permission answered"
    } else {
        "permission denied"
    }));
    view.paint(shared)
}

// ── prompt ────────────────────────────────────────────────────────────────────

/// Launch a session for `agent_id`, send one prompt, and stream each
/// [`SessionUpdateKind`] as a self-describing NDJSON line to `out`.
///
/// When `no_wait` is false (the default): subscribe to updates, send the
/// prompt, stream updates while the prompt is in flight, emit a terminal
/// `{"type":"result","stop_reason":"…"}` line, then shut down the session.
///
/// When `no_wait` is true: shut down the session immediately after emitting
/// `{"type":"submitted"}`. The agent child is terminated; callers needing a
/// persistent session should use `bitrouter acp serve` instead.
///
/// `contract` is the optional `--result-schema` contract: its
/// instruction rides the prompt, and the terminal `result` line gains
/// `result`/`schema_ok` (+ `raw` on failure) fields.
pub async fn prompt<W>(
    ctx: SpawnContext<'_>,
    text: &str,
    no_wait: bool,
    contract: Option<crate::result_contract::ResultContract>,
    out: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin + Send,
{
    let SpawnContext {
        source,
        mut config,
        agent_id,
        options,
        routing,
    } = ctx;
    // Route by default; fail fast with a single structured NDJSON `error`
    // line BEFORE any session side effect (no agent process spawned).
    //
    // The line goes on `out` because it is part of the NDJSON stream the
    // orchestrator is parsing — that is the contract, and it is why this
    // cannot simply propagate. What *is* returned is the same failure as an
    // error value, so the caller controls the exit rather than this function
    // ending the process from inside a library.
    let routed = match apply_routing(source, &mut config, agent_id, &routing).await {
        Ok(routed) => routed,
        Err(e) => {
            write_ndjson_line(out, &e.ndjson()).await?;
            out.flush().await.ok();
            return Err(anyhow::Error::new(e));
        }
    };

    let catalog = catalog_from_config(&config)?;
    let cwd = std::env::current_dir().context("resolving current directory")?;
    let session = bitrouter_sdk::acp::engine::Session::launch(&catalog, agent_id, cwd, options)
        .await
        .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    // No cost sink: `prompt` has no manager on a down-facing wire — its
    // output is the NDJSON stream, which carries its own terminal `result`.
    let exporter = attach_observability(&config, agent_id, &session, None).await;

    // First line: correlate this session with the cost/metering the
    // orchestrator later queries. `via` is null when running direct.
    write_ndjson_line(
        out,
        &serde_json::json!({
            "type": "session",
            "record_id": session.state().record_id,
            "agent": agent_id,
            "via": routed.via,
            // The token this session's requests carry, so an orchestrator can
            // group its spend. Null when the caller supplied their own
            // credential and the traffic is therefore not separable.
            "launch_id": routed.launch_id,
        }),
    )
    .await?;

    // Headless: there is no manager to broker permissions and none will ever
    // attach, so explicitly DENY each request (the reject option). Since the
    // session-scoped permission registry, merely *dropping* a pending item no
    // longer defaults to Deny — a registry clone keeps it alive for a
    // re-subscribing manager — so an unconsumed request would otherwise hang the
    // turn forever.
    let mut permissions = session.permissions();
    tokio::spawn(async move {
        while let Some(pending) = permissions.next().await {
            tracing::warn!(
                tool = pending
                    .tool_call
                    .fields
                    .title
                    .as_deref()
                    .unwrap_or("(unnamed)"),
                "headless prompt: denying permission request (no manager attached)"
            );
            pending.deny();
        }
    });

    if no_wait {
        // v1 no-wait: emit ack, then shut down immediately. The agent child is
        // killed on shutdown. Callers needing a persistent background session
        // should use `bitrouter acp serve` instead.
        write_ndjson_line(out, &serde_json::json!({ "type": "submitted" })).await?;
        session
            .shutdown()
            .await
            .context("shutting down acp session")?;
        if let Some(exporter) = exporter {
            exporter.shutdown();
        }
        return Ok(());
    }

    let outcome = prompt_wait(session, text, contract, out).await;
    if let Some(exporter) = exporter {
        // Flush the span batch before exit; spans are lost otherwise.
        exporter.shutdown();
    }
    outcome
}

/// Inner implementation for the wait (non-`--no-wait`) path. Separated so the
/// early-return in the `no_wait` branch above doesn't borrow `session` past its
/// drop point.
async fn prompt_wait<W>(
    session: bitrouter_sdk::acp::engine::Session,
    text: &str,
    contract: Option<crate::result_contract::ResultContract>,
    out: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    // Subscribe to updates BEFORE prompting so no streamed update is missed.
    let mut updates = session.updates();
    let task = match &contract {
        // The contract clause rides the subagent's task prompt.
        Some(c) => format!("{text}{}", c.instruction()),
        None => text.to_string(),
    };
    let (response, reply) =
        run_turn(&session, &mut updates, &task, contract.is_some(), out).await?;

    // Extract + validate the machine-consumable result. On failure: ONE
    // repair re-prompt, then `schema_ok:false` + raw text — the orchestrator
    // is never blocked on a malformed reply.
    let (response, result, schema_ok, raw) = match &contract {
        None => (response, None, None, None),
        Some(c) => match c.check(&reply) {
            Ok(value) => (response, Some(value), Some(true), None),
            Err(problem) => {
                let (response, reply) = run_turn(
                    &session,
                    &mut updates,
                    &c.repair_prompt(&problem),
                    true,
                    out,
                )
                .await?;
                match c.check(&reply) {
                    Ok(value) => (response, Some(value), Some(true), None),
                    Err(_) => (
                        response,
                        Some(serde_json::Value::Null),
                        Some(false),
                        Some(reply),
                    ),
                }
            }
        },
    };

    // Emit the terminal result line. `response.stop_reason` is an ACP
    // `StopReason` that serializes to its snake_case wire form (e.g.
    // `"end_turn"`).
    write_ndjson_line(
        out,
        &ResultLine {
            kind: "result",
            stop_reason: response.stop_reason,
            result,
            schema_ok,
            raw,
        },
    )
    .await?;

    session
        .shutdown()
        .await
        .context("shutting down acp session")?;
    Ok(())
}

/// Drive one prompt turn: stream its updates to `out` (accumulating message
/// text when `capture`), and return the typed response plus the reply text.
async fn run_turn<W>(
    session: &bitrouter_sdk::acp::engine::Session,
    updates: &mut (impl futures::Stream<Item = SessionUpdateKind> + Unpin),
    text: &str,
    capture: bool,
    out: &mut W,
) -> Result<(agent_client_protocol::schema::v1::PromptResponse, String)>
where
    W: AsyncWrite + Unpin,
{
    let mut reply = String::new();
    // Drive updates and the prompt concurrently. The loop returns the resolved
    // `PromptResponse` directly, so there is no `Option` to unwrap afterward.
    let response = {
        let prompt_future = session.prompt(text);
        tokio::pin!(prompt_future);

        loop {
            tokio::select! {
                biased;

                result = &mut prompt_future => {
                    let response = result.context("acp prompt failed")?;
                    // Non-blocking drain of any already-buffered updates.
                    loop {
                        let maybe = tokio::select! {
                            biased;
                            v = updates.next() => v,
                            _ = std::future::ready(()) => None,
                        };
                        match maybe {
                            Some(update) => emit_update(&update, capture, &mut reply, out).await?,
                            None => break,
                        }
                    }
                    break response;
                }

                maybe_update = updates.next() => {
                    if let Some(update) = maybe_update {
                        emit_update(&update, capture, &mut reply, out).await?;
                    }
                }
            }
        }
    };
    Ok((response, reply))
}

/// Emit one update as NDJSON, accumulating message text into `reply` when the
/// result contract needs it. The `SessionUpdateKind`'s own `type` tag (e.g.
/// `message_chunk`) makes the line self-describing.
async fn emit_update<W>(
    update: &SessionUpdateKind,
    capture: bool,
    reply: &mut String,
    out: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if capture && let SessionUpdateKind::MessageChunk { text, .. } = update {
        reply.push_str(text);
    }
    write_ndjson_line(out, update).await
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Attach observability to a session when the observe config opts telemetry
/// in: every turn is drained to stderr (always) and, with an exporter,
/// emitted as an OTel GenAI `invoke_agent` span; tool calls become
/// `execute_tool` spans from the translated update stream. Returns the
/// exporter so the caller can flush it (`shutdown`) before exit.
async fn attach_observability(
    config: &Config,
    agent_id: &str,
    session: &bitrouter_sdk::acp::engine::Session,
    cost: Option<CostSink>,
) -> Option<Arc<bitrouter_observe::otel::OtelExporter>> {
    let exporter = crate::assemble::build_otel_exporter_standalone(config).await;
    let recorder = exporter.as_ref().map(|exporter| {
        Arc::new(bitrouter_observe::acp::AcpSpanRecorder::new(
            exporter,
            agent_id,
            session.state().record_id.clone(),
        ))
    });

    // Telemetry drain: stderr log per turn (always), the invoke_agent span,
    // and the router-measured `UsageUpdate` (§7).
    //
    // One drain, three consumers, because `Session::telemetry()` hands out the
    // receiver exactly once — a second caller gets `None` and silently does
    // nothing. Everything that needs per-turn records has to hang off this
    // loop.
    if let Some(mut rx) = session.telemetry() {
        let recorder = recorder.clone();
        tokio::spawn(async move {
            // Opened lazily: the metering database may not exist until the
            // first routed request has settled.
            let mut store = None;
            while let Some(record) = rx.recv().await {
                if let Some(recorder) = &recorder {
                    recorder.turn_completed(&bitrouter_observe::acp::TurnRecord {
                        stop_reason: record.stop_reason.clone(),
                        latency: std::time::Duration::from_millis(record.latency_ms),
                        context_used: record.context.map(|c| c.used),
                        context_size: record.context.map(|c| c.size),
                    });
                }
                if let Some(cost) = &cost {
                    if store.is_none() {
                        store = crate::metering::reader::open_readonly(&cost.source).await;
                    }
                    if let Some(store) = &store
                        && let Some(update) = measured_usage_update(
                            store,
                            cost.session_start,
                            cost.launch_id.as_deref(),
                            &record,
                        )
                        .await
                    {
                        let _ = cost.updates.send(update);
                    }
                }
                drain_telemetry_record(record);
            }
        });
    }

    // Tool spans from the translated update stream (exporter-gated: without
    // one there is nothing to emit to).
    if let Some(recorder) = recorder {
        let mut updates = session.updates();
        tokio::spawn(async move {
            use bitrouter_sdk::acp::translate::ToolStatus;
            while let Some(update) = updates.next().await {
                match update {
                    SessionUpdateKind::ToolCall {
                        id, title, status, ..
                    } => match status {
                        ToolStatus::Pending | ToolStatus::Running => {
                            recorder.tool_started(id, title);
                        }
                        ToolStatus::Ok => recorder.tool_finished(&id, true, Some(&title)),
                        ToolStatus::Failed => recorder.tool_finished(&id, false, Some(&title)),
                    },
                    SessionUpdateKind::ToolCallUpdate {
                        id, status, title, ..
                    } => match status {
                        Some(ToolStatus::Ok) => {
                            recorder.tool_finished(&id, true, title.as_deref());
                        }
                        Some(ToolStatus::Failed) => {
                            recorder.tool_finished(&id, false, title.as_deref());
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        });
    }

    exporter
}

// ── providers/* — BitRouter's routing surface, in ACP's nouns (spec §6) ──────

/// Backs `providers/list` and `providers/set` for one live session.
///
/// **No credential ever crosses this wire.** `ProviderCurrentConfig` is
/// specified as *non-secret* routing config, and this type is constructed from
/// `api_base` / `api_protocol` only — `ProviderConfig::api_key` is never read
/// here. Credential management stays on `bitrouter providers login`.
///
/// # Scope of `providers/set` today
///
/// The selection is **session-scoped and held in this process**: it is what
/// `providers/list` reports as the effective route, and it is what a manager
/// renders. It does not yet rewrite the model on traffic already in flight
/// from the agent child to the daemon.
///
/// That last step is deliberately not faked here. The substrate is a separate
/// process from the daemon and the agent child talks to the daemon directly,
/// so the rewrite has to happen daemon-side, in the policy table (spec §6.1,
/// probe 4). The table is now hot-swappable — that was the point of rebuilding
/// it on reload — but reaching it from here needs a route-mutation command on
/// the daemon control socket, which does not exist yet. Reporting a route this
/// process cannot yet enforce is the honest half; claiming the traffic moved
/// would not be.
pub(crate) struct SessionProviders {
    /// Non-secret catalog snapshot, taken once at session launch.
    catalog: Vec<CatalogEntry>,
    /// The provider this session's route currently points at. `None` until a
    /// `providers/set` names one, at which point `providers/list` reports it.
    selected: std::sync::RwLock<Option<String>>,
    /// Where to install the override so it moves real traffic. `None` for a
    /// `--direct` session or one whose credential cannot be attributed — and
    /// in that case `set` **refuses** rather than reporting a switch it did
    /// not perform.
    control: Option<RouteControl>,
}

/// The daemon endpoint a route override is installed through, and the launch
/// id that scopes it to this session.
pub(crate) struct RouteControl {
    socket: std::path::PathBuf,
    launch_id: String,
}

/// One routable provider, reduced to what a UI may see.
struct CatalogEntry {
    id: String,
    api_base: String,
    protocol: String,
}

impl SessionProviders {
    /// Snapshot the active providers from `config`, dropping every secret.
    ///
    /// `control` is `Some` only when this session's traffic can actually be
    /// moved: that needs both a daemon to install the override in and a launch
    /// id to scope it by.
    pub(crate) fn new(config: &Config, control: Option<RouteControl>) -> Self {
        let mut catalog: Vec<CatalogEntry> = config
            .providers
            .iter()
            .filter(|(_, provider)| provider.active)
            .map(|(id, provider)| CatalogEntry {
                id: id.clone(),
                api_base: provider.api_base.clone(),
                protocol: provider
                    .api_protocol
                    .resolve("*")
                    .and_then(|list| list.preferred().map(|p| p.as_str().to_string()))
                    .unwrap_or_else(|| "openai".to_string()),
            })
            .collect();
        // Stable order so a rendered list does not reshuffle between calls.
        catalog.sort_by(|a, b| a.id.cmp(&b.id));
        Self {
            catalog,
            selected: std::sync::RwLock::new(None),
            control,
        }
    }

    /// Whether this session's traffic can actually be moved.
    ///
    /// Gates the picker: a chooser that cannot change anything is worse than
    /// no chooser, because absence is legible and a dead control is a lie.
    pub(crate) fn can_reroute(&self) -> bool {
        self.control.is_some()
    }

    /// Drop this session's override, so it cannot outlive the session that
    /// asked for it. Best-effort: the daemon may already be gone at teardown,
    /// and that is not a failure worth surfacing to the manager.
    pub(crate) async fn release(&self) {
        let Some(control) = &self.control else { return };
        if self.effective().is_none() {
            return;
        }
        if let Err(e) = crate::daemon::send_command(
            &control.socket,
            &crate::daemon::DaemonCommand::SetRoute {
                launch_id: control.launch_id.clone(),
                provider_id: None,
            },
        )
        .await
        {
            tracing::debug!(error = %e, "could not clear the session route override");
        }
    }

    /// The route in force, or `None` while the session is on its configured
    /// default. A poisoned lock reads as "no override" rather than panicking.
    fn effective(&self) -> Option<String> {
        match self.selected.read() {
            Ok(selected) => selected.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// Map a BitRouter protocol name onto ACP's `LlmProtocol`. Anything outside
/// ACP's closed set is reported as `Other` rather than forced onto a protocol
/// that means something different.
fn llm_protocol(name: &str) -> LlmProtocol {
    match name {
        "anthropic" | "messages" => LlmProtocol::Anthropic,
        "chat_completions" | "responses" | "openai" => LlmProtocol::OpenAi,
        other => LlmProtocol::Other(other.to_string()),
    }
}

#[async_trait::async_trait]
impl bitrouter_sdk::acp::down::ProviderSurface for SessionProviders {
    async fn list(&self) -> Vec<ProviderInfo> {
        let effective = self.effective();
        self.catalog
            .iter()
            .map(|entry| {
                let protocol = llm_protocol(&entry.protocol);
                // `current` marks the route in force. Before any
                // `providers/set` no provider is singled out: the effective
                // route is the config's cascade, and naming one would claim a
                // decision the router has not made.
                //
                // It carries `api_type` and `base_url` and nothing else — the
                // provider's `api_key` is never read on this path.
                let current = (effective.as_deref() == Some(entry.id.as_str()))
                    .then(|| ProviderCurrentConfig::new(protocol.clone(), entry.api_base.clone()));
                ProviderInfo::new(
                    ProviderId::new(entry.id.clone()),
                    vec![protocol],
                    // Nothing in a BitRouter catalog is undisablable — the
                    // point of the router is that any provider can be routed
                    // around.
                    false,
                    current,
                )
            })
            .collect()
    }

    async fn set(&self, request: SetProviderRequest) -> std::result::Result<(), String> {
        let requested = request.provider_id.0.to_string();
        if !self.catalog.iter().any(|entry| entry.id == requested) {
            return Err(format!(
                "unknown provider '{requested}' — `providers/list` reports what is routable"
            ));
        }
        // Move the traffic FIRST, and record the selection only if it moved.
        // A `providers/list` reporting a route the daemon is not serving would
        // be a lie the manager renders as a fact.
        let Some(control) = &self.control else {
            return Err(
                "this session's traffic cannot be rerouted: it runs direct, or its credential \
                 is the caller's own and so cannot be attributed to a launch"
                    .to_string(),
            );
        };
        let response = crate::daemon::send_command(
            &control.socket,
            &crate::daemon::DaemonCommand::SetRoute {
                launch_id: control.launch_id.clone(),
                provider_id: Some(requested.clone()),
            },
        )
        .await
        .map_err(|e| format!("reaching the daemon to install the route: {e}"))?;
        if let crate::daemon::DaemonResponse::Error { message } = response {
            return Err(format!("daemon refused the route change: {message}"));
        }
        match self.selected.write() {
            Ok(mut selected) => {
                *selected = Some(requested);
                Ok(())
            }
            Err(_) => Err("provider selection lock was poisoned".to_string()),
        }
    }
}

/// Everything the telemetry drain needs to put measured cost on the wire.
struct CostSink {
    source: ConfigSource,
    session_start: chrono::DateTime<chrono::Utc>,
    /// This session's attribution token, when its traffic is separable from
    /// every other caller's. `None` downgrades the reported scope rather than
    /// silently counting someone else's spend.
    launch_id: Option<String>,
    updates: tokio::sync::mpsc::UnboundedSender<SessionUpdate>,
}

/// Router-measured session spend, synthesized as an ACP `UsageUpdate` on every
/// settled turn (spec §7, decision 4).
///
/// BitRouter sits in the agent seat, so it can report what the upstream harness
/// cannot: what the turn actually **cost**. Upstream `UsageUpdate.cost` is
/// optional and most harnesses never send it, which is how the old status bar
/// ended up presenting daemon-wide spend as the session's. This measures it
/// instead, from the metering store, scoped to this session's window.
///
/// `used`/`size` come from the upstream's own context reporting and are `0`
/// when it reports none — the context window is genuinely the harness's fact,
/// and inventing a number for it would be the exact dishonesty this replaces.
///
/// Returns `None` when there is nothing to say: no metering database, no
/// requests settled yet, or a read error. A silent gap beats a fabricated zero.
async fn measured_usage_update(
    store: &crate::metering::store::MeteringStore,
    session_start: chrono::DateTime<chrono::Utc>,
    launch_id: Option<&str>,
    record: &RequestCompleted,
) -> Option<SessionUpdate> {
    use crate::metering::store::TimeWindow;
    use crate::tui::snapshot::Scope;

    let window = TimeWindow::Custom {
        start: session_start,
        end: chrono::Utc::now(),
    };
    // Attribution decides the scope, and the scope decides the query. With a
    // launch token the store can isolate this session's rows; without one the
    // best available answer is every caller in the window — which is a
    // different fact, so it travels labelled as one.
    let (summary, scope) = match launch_id {
        Some(id) => (
            store.spend_summary_for_launch(id, window).await.ok()?,
            Scope::Launch,
        ),
        None => (store.spend_summary(window).await.ok()?, Scope::DaemonWide),
    };
    if summary.requests == 0 {
        return None;
    }
    let cost = Cost::new(summary.spend_micro_usd as f64 / 1_000_000.0, "USD");
    let context = record.context;
    // The scope rides in `_meta`: a client that renders a number without
    // knowing whose it is cannot be honest about it, and ACP has no field for
    // this.
    let mut meta = serde_json::Map::new();
    meta.insert(
        crate::chat::cost::COST_SCOPE_META_KEY.to_string(),
        serde_json::Value::String(scope.as_wire().to_string()),
    );
    Some(SessionUpdate::UsageUpdate(
        UsageUpdate::new(
            context.map(|c| c.used).unwrap_or(0),
            context.map(|c| c.size).unwrap_or(0),
        )
        .cost(cost)
        .meta(meta),
    ))
}

/// Emit one telemetry record to stderr via tracing. Stdout must stay clean
/// (ACP JSON-RPC for `serve`, NDJSON for `prompt`), so telemetry goes to
/// `tracing::info!` which the acp CLI routes to stderr.
fn drain_telemetry_record(r: RequestCompleted) {
    tracing::info!(
        agent = %r.agent,
        stop_reason = %r.stop_reason,
        latency_ms = r.latency_ms,
        context_used = r.context.map(|c| c.used),
        context_size = r.context.map(|c| c.size),
        "acp turn completed"
    );
}

/// Build [`LaunchOptions`] from the CLI flags shared by `serve` and `prompt`:
/// `--turn-timeout <secs>`.
pub fn launch_options(turn_timeout_secs: Option<u64>) -> LaunchOptions {
    LaunchOptions {
        turn_timeout: turn_timeout_secs.map(std::time::Duration::from_secs),
        ..Default::default()
    }
}

/// Build a [`ConfigAcpRoutingTable`] from the `agents` section of `config`.
pub(crate) fn catalog_from_config(config: &Config) -> Result<ConfigAcpRoutingTable> {
    ConfigAcpRoutingTable::from_configs(config.agents.iter().map(|(k, v)| (k.clone(), v.clone())))
        .context("building acp routing table from config")
}

#[cfg(test)]
mod cost_tests {
    use super::*;
    use crate::metering::db::{ReconciliationStatus, RequestMetric};
    use crate::metering::pricing::{
        ChargeEvidence, ChargeStatus, EffectivePricingRates, PricingSource,
    };
    use crate::metering::store::MeteringStore;

    /// A settled turn must reach the manager carrying a **non-null** cost that
    /// equals what the router actually measured — the whole point of decision
    /// 4. Upstream `UsageUpdate.cost` is optional and usually absent; this is
    /// the router reporting what only it can know.
    /// Build a settled request row charged `charge_micro_usd`, attributed to
    /// `launch_id` when one is given.
    fn settled(request_id: &str, launch_id: Option<&str>, charge_micro_usd: i64) -> RequestMetric {
        RequestMetric {
            request_id: request_id.to_string(),
            user_id: "u1".into(),
            api_key_id: "k1".into(),
            launch_id: launch_id.map(ToString::to_string),
            model_id: "claude-sonnet-4-5".into(),
            provider_id: "anthropic".into(),
            prompt_tokens: 1_000,
            completion_tokens: 200,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            uncached_input_tokens: 1_000,
            output_tokens: 200,
            usage_origin: bitrouter_sdk::language_model::UsageOrigin::ProviderReported,
            raw_usage: None,
            charge_status: ChargeStatus::Computed,
            charge_evidence: ChargeEvidence {
                status: ChargeStatus::Computed,
                charge_micro_usd: Some(charge_micro_usd),
                normalized_usage: Default::default(),
                effective_rates: EffectivePricingRates::default(),
                pricing_source: PricingSource::Configured,
                pricing_version: "sha256:test".to_string(),
                unknown_reason: None,
            },
            reconciliation_status: ReconciliationStatus::NotApplicable,
            estimated_charge_micro_usd: charge_micro_usd,
            latency_ms: 1_200,
            generation_time_ms: 900,
            streamed: false,
            error: None,
        }
    }

    fn turn() -> RequestCompleted {
        RequestCompleted {
            agent: "claude-acp".into(),
            stop_reason: "EndTurn".into(),
            latency_ms: 1_200,
            context: Some(bitrouter_sdk::acp::telemetry::ContextUsage {
                used: 1_500,
                size: 200_000,
            }),
        }
    }

    /// Read the cost and the `_meta` scope label off a synthesized update.
    fn cost_and_scope(update: SessionUpdate) -> anyhow::Result<(f64, String)> {
        let SessionUpdate::UsageUpdate(usage) = update else {
            anyhow::bail!("expected a UsageUpdate");
        };
        let cost = usage
            .cost
            .ok_or_else(|| anyhow::anyhow!("UsageUpdate.cost must not be null"))?;
        let scope = usage
            .meta
            .as_ref()
            .and_then(|m| m.get(crate::chat::cost::COST_SCOPE_META_KEY))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("a cost must carry its scope"))?
            .to_string();
        Ok((cost.amount, scope))
    }

    /// Two callers settle spend in the same window. A session that can attribute
    /// its traffic must report **only its own** — the wire previously reported
    /// every caller's and called it the session's.
    #[tokio::test]
    async fn reported_cost_counts_only_this_session() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = MeteringStore::new(db);
        let session_start = chrono::Utc::now() - chrono::Duration::seconds(5);

        // This session's spend, and a concurrent caller's — same window.
        store
            .record_request(settled("mine", Some("brl_mine"), 420_000))
            .await?;
        store
            .record_request(settled("theirs", Some("brl_theirs"), 900_000))
            .await?;
        // Plus an untagged caller, which no launch id can claim.
        store
            .record_request(settled("anon", None, 1_500_000))
            .await?;

        let update = measured_usage_update(&store, session_start, Some("brl_mine"), &turn())
            .await
            .ok_or_else(|| anyhow::anyhow!("an attributed session must report its spend"))?;
        let (amount, scope) = cost_and_scope(update)?;
        assert_eq!(
            amount, 0.42,
            "only this session's $0.42 — not the other caller's $0.90 or the \
             untagged $1.50"
        );
        assert_eq!(scope, "session", "attributed spend is labelled as such");
        Ok(())
    }

    /// A session that cannot attribute its traffic — the caller supplied their
    /// own credential, so nothing may be rewritten to tag it — must say the
    /// number is daemon-wide rather than imply a precision it does not have.
    #[tokio::test]
    async fn unattributable_cost_is_labelled_daemon_wide() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = MeteringStore::new(db);
        let session_start = chrono::Utc::now() - chrono::Duration::seconds(5);

        store
            .record_request(settled("mine", Some("brl_mine"), 420_000))
            .await?;
        store
            .record_request(settled("theirs", Some("brl_theirs"), 900_000))
            .await?;

        let update = measured_usage_update(&store, session_start, None, &turn())
            .await
            .ok_or_else(|| anyhow::anyhow!("an unattributed session still reports a window"))?;
        let (amount, scope) = cost_and_scope(update)?;
        assert_eq!(amount, 1.32, "every caller in the window: $0.42 + $0.90");
        assert_eq!(
            scope, "daemon_wide",
            "an unattributable figure must never be presented as the session's"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_settled_turn_reports_router_measured_cost() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = MeteringStore::new(db);
        let session_start = chrono::Utc::now() - chrono::Duration::seconds(5);

        // No settled request yet: nothing to report, and reporting a zero
        // would be a fabricated number.
        let record = RequestCompleted {
            agent: "claude-acp".into(),
            stop_reason: "EndTurn".into(),
            latency_ms: 1_200,
            context: Some(bitrouter_sdk::acp::telemetry::ContextUsage {
                used: 1_500,
                size: 200_000,
            }),
        };
        assert!(
            measured_usage_update(&store, session_start, None, &record)
                .await
                .is_none(),
            "no settled request means no usage update"
        );

        // Settle one request worth $0.42.
        store
            .record_request(RequestMetric {
                request_id: "r1".into(),
                user_id: "u1".into(),
                api_key_id: "k1".into(),
                launch_id: None,
                model_id: "claude-sonnet-4-5".into(),
                provider_id: "anthropic".into(),
                prompt_tokens: 1_000,
                completion_tokens: 200,
                reasoning_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                uncached_input_tokens: 1_000,
                output_tokens: 200,
                usage_origin: bitrouter_sdk::language_model::UsageOrigin::ProviderReported,
                raw_usage: None,
                charge_status: ChargeStatus::Computed,
                charge_evidence: ChargeEvidence {
                    status: ChargeStatus::Computed,
                    charge_micro_usd: Some(420_000),
                    normalized_usage: Default::default(),
                    effective_rates: EffectivePricingRates::default(),
                    pricing_source: PricingSource::Configured,
                    pricing_version: "sha256:test".to_string(),
                    unknown_reason: None,
                },
                reconciliation_status: ReconciliationStatus::NotApplicable,
                estimated_charge_micro_usd: 420_000,
                latency_ms: 1_200,
                generation_time_ms: 900,
                streamed: false,
                error: None,
            })
            .await?;

        let update = measured_usage_update(&store, session_start, None, &record)
            .await
            .ok_or_else(|| anyhow::anyhow!("a settled request must produce a usage update"))?;

        let SessionUpdate::UsageUpdate(usage) = update else {
            anyhow::bail!("expected a UsageUpdate");
        };
        let cost = usage
            .cost
            .ok_or_else(|| anyhow::anyhow!("UsageUpdate.cost must not be null"))?;
        assert_eq!(cost.amount, 0.42, "the measured charge, in USD");
        assert_eq!(cost.currency, "USD");
        // Context occupancy stays the upstream's fact, relayed unchanged.
        assert_eq!(usage.used, 1_500);
        assert_eq!(usage.size, 200_000);

        // And the window is the session's, not the daemon's lifetime: a
        // request settled before this session started must not be counted.
        let later_start = chrono::Utc::now() + chrono::Duration::seconds(5);
        assert!(
            measured_usage_update(&store, later_start, None, &record)
                .await
                .is_none(),
            "spend from before the session must not be attributed to it"
        );
        Ok(())
    }
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    use bitrouter_sdk::acp::down::ProviderSurface;

    /// A config whose providers carry unmistakable secrets, so a leak in any
    /// serialized field is impossible to miss.
    fn config_with_secrets() -> anyhow::Result<Config> {
        Ok(bitrouter_sdk::config::parse(
            r#"inherit_defaults: false
providers:
  alpha:
    api_base: https://alpha.example.com/v1
    api_key: sk-SECRET-ALPHA-DO-NOT-LEAK
    api_protocol:
      - "*": chat_completions
    models:
      - { id: m1 }
  beta:
    api_base: https://beta.example.com/v1
    api_key: sk-SECRET-BETA-DO-NOT-LEAK
    api_protocol:
      - "*": anthropic
    models:
      - { id: m1 }
  dormant:
    active: false
    api_base: https://dormant.example.com/v1
    api_key: sk-SECRET-DORMANT-DO-NOT-LEAK
    api_protocol:
      - "*": chat_completions
    models:
      - { id: m1 }
"#,
        )?)
    }

    #[tokio::test]
    async fn providers_list_reports_the_routable_catalog() -> anyhow::Result<()> {
        let providers = SessionProviders::new(&config_with_secrets()?, None);
        let listed = providers.list().await;

        let ids: Vec<String> = listed.iter().map(|p| p.provider_id.0.to_string()).collect();
        // Active providers only, in a stable order — an inactive provider is
        // not routable, so offering it would be offering a dead route.
        assert_eq!(ids, vec!["alpha".to_string(), "beta".to_string()]);

        // Protocols are mapped into ACP's vocabulary, not passed through raw.
        assert_eq!(listed[0].supported, vec![LlmProtocol::OpenAi]);
        assert_eq!(listed[1].supported, vec![LlmProtocol::Anthropic]);
        // Nothing in a router's catalog is undisablable.
        assert!(listed.iter().all(|p| !p.required));
        // No `providers/set` yet: no provider claims to be the effective route.
        assert!(listed.iter().all(|p| p.current.is_none()));
        Ok(())
    }

    /// A session with no way to move its traffic must **refuse** rather than
    /// report a switch it cannot perform. The reroute itself is a daemon-side
    /// fact, proven by `set_route_reroutes_only_the_named_launch`.
    #[tokio::test]
    async fn providers_set_refuses_when_it_cannot_reroute() -> anyhow::Result<()> {
        let providers = SessionProviders::new(&config_with_secrets()?, None);
        let refused = providers
            .set(SetProviderRequest::new(
                ProviderId::new("beta"),
                LlmProtocol::Anthropic,
                "https://beta.example.com/v1",
            ))
            .await;
        assert!(
            refused.is_err(),
            "a session that cannot reroute must not claim it did"
        );
        // And it must not then report a route it never installed.
        assert!(
            providers.list().await.iter().all(|p| p.current.is_none()),
            "a refused set leaves no route in force"
        );

        // An unroutable target is refused before the daemon is ever asked —
        // reporting success for a route that does not exist would be worse
        // than the error.
        let rejected = providers
            .set(SetProviderRequest::new(
                ProviderId::new("nonexistent"),
                LlmProtocol::OpenAi,
                "https://nope.example.com/v1",
            ))
            .await;
        assert!(rejected.is_err(), "unknown provider must be refused");
        Ok(())
    }

    /// The hard rule from spec §6: `ProviderCurrentConfig` is non-secret
    /// routing config. This asserts against the **serialized JSON** — the
    /// bytes that actually reach the manager — rather than against fields,
    /// so a secret smuggled through `_meta` would fail too.
    #[tokio::test]
    async fn no_credential_appears_in_any_providers_response() -> anyhow::Result<()> {
        let providers = SessionProviders::new(&config_with_secrets()?, None);
        let wire = serde_json::to_string(&providers.list().await)?;
        for secret in [
            "sk-SECRET-ALPHA-DO-NOT-LEAK",
            "sk-SECRET-BETA-DO-NOT-LEAK",
            "sk-SECRET-DORMANT-DO-NOT-LEAK",
        ] {
            assert!(
                !wire.contains(secret),
                "a credential reached the providers wire: {wire}"
            );
        }
        // Not just the exact strings — no `api_key`-shaped field at all.
        assert!(!wire.contains("api_key"), "{wire}");
        assert!(!wire.contains("sk-"), "{wire}");
        // The provider ids a UI needs are still there.
        assert!(wire.contains("alpha"), "{wire}");
        assert!(wire.contains("beta"), "{wire}");
        Ok(())
    }
}
