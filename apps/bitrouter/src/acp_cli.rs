//! `bitrouter acp` subcommands — headless ACP session surface.
//!
//! Three entry points:
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
//! - [`sessions`] — list the durable session records under the current repo's
//!   `.bitrouter/sessions/`, newest first.
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

use bitrouter_substrate::engine::LaunchOptions;
use bitrouter_substrate::telemetry::RequestCompleted;
use bitrouter_substrate::translate::SessionUpdateKind;
use bitrouter_substrate::worktree::WorktreeSpec;

use crate::paths::ConfigSource;

// ── routing (spawn --via-daemon by default) ─────────────────────────────────────

/// Per-invocation routing decision for a spawned sub-agent. Routing is on by
/// default; `direct` opts out. See `internal/SPAWN_SPEC.md` §5.
#[derive(Debug, Clone, Default)]
pub struct RoutingOptions {
    /// Skip daemon routing entirely — the harness talks to its own provider.
    pub direct: bool,
    /// Explicit gateway base URL. When `None` it is derived from the daemon's
    /// `server.listen`.
    pub base_url: Option<String>,
    /// Pin the harness's model (via its model env var / `-c model=`).
    pub model: Option<String>,
    /// Never auto-start a local daemon when none is running — fail fast.
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
    /// Session options (worktree, turn timeout).
    pub options: LaunchOptions,
    /// The routing decision (via-daemon by default, or `--direct`).
    pub routing: RoutingOptions,
}

/// A fail-fast routing failure, surfaced BEFORE any session side effect
/// (`internal/SPAWN_SPEC.md` §8). Rendered as a structured NDJSON `error` line in
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

/// Resolve routing and overlay it onto `config`'s entry for `agent_id`,
/// inserting the bundled-catalog invocation when the id is catalog-known but
/// unconfigured (so `bitrouter spawn claude-acp` works with no YAML edit).
///
/// Returns the "via" base URL when routing is active, or `None` when the
/// session runs direct (`--direct`, an unknown/custom agent, or an
/// unroutable harness — each warned to stderr). Fails fast — before the
/// caller creates any worktree or record — on an unreachable
/// daemon or a missing required credential.
pub async fn apply_routing(
    source: &ConfigSource,
    config: &mut Config,
    agent_id: &str,
    opts: &RoutingOptions,
) -> std::result::Result<Option<String>, RoutingError> {
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
        return Ok(None);
    }

    // Match the (now-present-if-known) invocation back to a catalog harness.
    let harness = match config.agents.get(agent_id) {
        Some(entry) => {
            let AcpTransport::Stdio { command, args, .. } = &entry.transport;
            crate::harness::match_invocation(command, args)
        }
        // Unknown agent — let the caller's `Session::launch` surface the
        // configured-agents not-found error.
        None => return Ok(None),
    };
    let Some(harness) = harness else {
        eprintln!(
            "note: routing unavailable for '{agent_id}' (not catalog-matched); \
             launching direct — set its `env` to route manually"
        );
        warn_model_dropped("the agent is not catalog-matched");
        return Ok(None);
    };
    if !harness.env_args_routable() {
        eprintln!(
            "note: '{}' routes via synthesized config, which headless spawn doesn't do yet \
             (the `bitrouter tui` orchestrator facet does); launching direct",
            harness.id
        );
        warn_model_dropped("the harness routes only in the interactive facet");
        return Ok(None);
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

    let auth = match crate::harness::resolve_gateway_auth(
        crate::spawn::nonempty_env(crate::harness::BITROUTER_API_KEY_ENV),
        require_key,
    ) {
        Some(a) => a,
        None => return Err(RoutingError::AuthRequired { via: base_url }),
    };

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
    Ok(Some(base_url))
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
                        "'{agent_id}' is interactive-only (no ACP adapter) — use `bitrouter tui --agent {}`",
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
/// the worktree spec and per-turn timeout resolved from the CLI flags (see
/// [`launch_options`]).
pub async fn serve(ctx: SpawnContext<'_>) -> Result<()> {
    let SpawnContext {
        source,
        mut config,
        agent_id,
        options,
        routing,
    } = ctx;
    // Route the sub-agent's LLM traffic through the daemon (default) unless
    // opted out. Fail fast to stderr — before speaking any ACP — so a manager
    // handles "child failed to start" rather than a mid-session provider error.
    if let Err(e) = apply_routing(source, &mut config, agent_id, &routing).await {
        eprintln!("spawn: {}\n  hint: {}", e.message(), e.hint());
        std::process::exit(1);
    }
    let catalog = catalog_from_config(&config)?;
    let base_repo = std::env::current_dir().context("resolving current directory")?;
    // Deferred open: the upstream `session/new` runs when the manager sends
    // its own `session/new`, so the manager's cwd + mcpServers are relayed.
    let session = bitrouter_substrate::engine::Session::launch_deferred(
        &catalog,
        agent_id,
        base_repo.clone(),
        options,
    )
    .await
    .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let exporter = attach_observability(&config, agent_id, &session).await;
    let session = Arc::new(session);

    let served = bitrouter_substrate::down::serve(Arc::clone(&session)).await;

    // No manager left: shut the session down deliberately so the worktree
    // policy is honored (same semantics as `prompt`). Once serving ends, the
    // forwarding tasks have released their clones, so we are the sole owner.
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
/// `contract` is the optional `--result-schema` contract (TUI_SPEC §4): its
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
    // line BEFORE any session side effect (no worktree/record).
    let via = match apply_routing(source, &mut config, agent_id, &routing).await {
        Ok(via) => via,
        Err(e) => {
            write_ndjson_line(out, &e.ndjson()).await?;
            out.flush().await.ok();
            std::process::exit(1);
        }
    };

    let catalog = catalog_from_config(&config)?;
    let base_repo = std::env::current_dir().context("resolving current directory")?;
    let session =
        bitrouter_substrate::engine::Session::launch(&catalog, agent_id, base_repo, options)
            .await
            .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let exporter = attach_observability(&config, agent_id, &session).await;

    // First line: correlate this session's record with the cost/metering the
    // orchestrator later queries. `via` is null when running direct.
    write_ndjson_line(
        out,
        &serde_json::json!({
            "type": "session",
            "record_id": session.state().record_id,
            "agent": agent_id,
            "via": via,
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
    session: bitrouter_substrate::engine::Session,
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
    session: &bitrouter_substrate::engine::Session,
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

// ── sessions ──────────────────────────────────────────────────────────────────

/// List the session records under the current repo's `.bitrouter/sessions/`,
/// newest first: short record id, agent, status, age, and worktree.
///
/// A record left `running` by a substrate process that died without shutting
/// down is shown as `dead` (its pid no longer exists) rather than trusted.
pub async fn sessions<W>(out: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    use bitrouter_substrate::record::{RecordStatus, RecordStore, now_unix};

    let base = std::env::current_dir().context("resolving current directory")?;
    let store = RecordStore::new(&base);
    let mut records = store.list().await?;
    if records.is_empty() {
        out.write_all(b"no sessions recorded under .bitrouter/sessions\n")
            .await
            .context("writing output")?;
        return Ok(());
    }
    records.sort_by_key(|r| std::cmp::Reverse(r.started_at));

    let now = now_unix();
    let mut buf = String::from("RECORD    AGENT             STATUS   AGE      WORKTREE\n");
    for r in records {
        let status = match r.status {
            RecordStatus::Exited => "exited",
            RecordStatus::Running if pid_alive(r.pid) => "running",
            RecordStatus::Running => "dead",
        };
        let short_id: String = r.record_id.chars().take(8).collect();
        let worktree = r
            .worktree
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".to_string());
        buf.push_str(&format!(
            "{short_id:<9} {agent:<17} {status:<8} {age:<8} {worktree}\n",
            agent = r.agent_id,
            age = format_age(now.saturating_sub(r.started_at)),
        ));
    }
    out.write_all(buf.as_bytes())
        .await
        .context("writing output")
}

/// Whether `pid` is a live process. Used to demote a stale `running` record
/// (left behind by a killed substrate) to `dead` in the listing.
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `kill -0` probes existence without signalling. EPERM (owned by
        // another user) exits non-zero, which conservatively reads as dead —
        // acceptable, since substrate sessions run as the invoking user.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Render an age in seconds as a compact human unit (`42s`, `7m`, `3h`, `2d`).
fn format_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
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
    session: &bitrouter_substrate::engine::Session,
) -> Option<Arc<bitrouter_observe::otel::OtelExporter>> {
    let exporter = crate::assemble::build_otel_exporter_standalone(config).await;
    let recorder = exporter.as_ref().map(|exporter| {
        Arc::new(bitrouter_observe::acp::AcpSpanRecorder::new(
            exporter,
            agent_id,
            session.state().record_id.clone(),
        ))
    });

    // Telemetry drain: stderr log per turn (always) + invoke_agent span.
    if let Some(mut rx) = session.telemetry() {
        let recorder = recorder.clone();
        tokio::spawn(async move {
            while let Some(record) = rx.recv().await {
                if let Some(recorder) = &recorder {
                    recorder.turn_completed(&bitrouter_observe::acp::TurnRecord {
                        stop_reason: record.stop_reason.clone(),
                        latency: std::time::Duration::from_millis(record.latency_ms),
                        context_used: record.context.map(|c| c.used),
                        context_size: record.context.map(|c| c.size),
                    });
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
            use bitrouter_substrate::translate::ToolStatus;
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
/// `--worktree`/`--rm-worktree` (retention is the default — removal destroys
/// the agent's uncommitted work, so it is strictly opt-in) and
/// `--turn-timeout <secs>`.
pub fn launch_options(
    worktree: Option<&str>,
    rm_worktree: bool,
    turn_timeout_secs: Option<u64>,
) -> LaunchOptions {
    LaunchOptions {
        worktree: worktree.map(|name| WorktreeSpec {
            name: name.to_string(),
            branch: None,
            remove_on_shutdown: rm_worktree,
        }),
        turn_timeout: turn_timeout_secs.map(std::time::Duration::from_secs),
        ..Default::default()
    }
}

/// Build a [`ConfigAcpRoutingTable`] from the `agents` section of `config`.
fn catalog_from_config(config: &Config) -> Result<ConfigAcpRoutingTable> {
    ConfigAcpRoutingTable::from_configs(config.agents.iter().map(|(k, v)| (k.clone(), v.clone())))
        .context("building acp routing table from config")
}
