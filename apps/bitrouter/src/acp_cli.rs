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
//! `bitrouter::paths`) and launch the agent named under `config.agents`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bitrouter_sdk::acp::transport::{AcpAgentConfig, AcpTransport};
use bitrouter_sdk::config::Config;
use futures::StreamExt;
use serde::Serialize;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use agent_client_protocol::schema::v1::{Cost, LlmProtocol};
use bitrouter_sdk::acp::client::{AcpClient, ClientOptions};
use bitrouter_sdk::acp::controller::{
    RouteControl as AcpRouteControl, RouteControlError, RouteControlState,
    SessionCost as AcpSessionCost,
};
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
    /// The pinned adapter fallback could not be rendered before launch.
    EndpointConfiguration {
        /// The gateway base URL that would have been used.
        via: String,
        /// Sanitized configuration failure. Credentials are never included.
        message: String,
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
            RoutingError::EndpointConfiguration { .. } => "endpoint_configuration",
        }
    }

    /// The gateway base URL this failure concerns.
    fn via(&self) -> &str {
        match self {
            RoutingError::DaemonUnreachable { via }
            | RoutingError::AuthRequired { via }
            | RoutingError::EndpointConfiguration { via, .. } => via,
        }
    }

    /// One-line remediation hint.
    fn hint(&self) -> &'static str {
        match self {
            RoutingError::DaemonUnreachable { .. } => "run `bitrouter start`, or pass --direct",
            RoutingError::AuthRequired { .. } => {
                "export BITROUTER_API_KEY (or create a key), or pass --direct"
            }
            RoutingError::EndpointConfiguration { .. } => {
                "check the pinned ACP adapter configuration, or pass --direct"
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
            RoutingError::EndpointConfiguration { message, .. } => message.clone(),
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
    /// without one has no per-launch attribution; on the controlled paths a
    /// controller credential attributes by controller instance instead.
    pub launch_id: Option<String>,
    /// One controller process / harness connection correlation id. This is
    /// never an ACP session id and is absent for direct or legacy harnesses.
    pub controller_instance_id: Option<String>,
    /// The one process-scoped endpoint plan used for both launch fallback and
    /// post-initialize ACP provider configuration.
    pub endpoint_plan: Option<crate::harness::HarnessEndpointPlan>,
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
    let cloud_credentials = crate::cloud::StandaloneCloudCredentials::new();
    apply_routing_with_cloud_credentials(
        source,
        config,
        agent_id,
        opts,
        &cloud_credentials,
        RoutingCredentialMode::UserOrLaunch,
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoutingCredentialMode {
    UserOrLaunch,
    ControllerIssuedLocal,
}

fn user_key_required(
    target_is_local: bool,
    daemon_requires_key: bool,
    uses_maintained_adapter: bool,
    implicit_local_target: bool,
    mode: RoutingCredentialMode,
) -> bool {
    daemon_requires_key
        && !(target_is_local
            && uses_maintained_adapter
            && implicit_local_target
            && mode == RoutingCredentialMode::ControllerIssuedLocal)
}

async fn apply_routing_with_cloud_credentials(
    source: &ConfigSource,
    config: &mut Config,
    agent_id: &str,
    opts: &RoutingOptions,
    cloud_credentials: &crate::cloud::StandaloneCloudCredentials,
    credential_mode: RoutingCredentialMode,
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
    let uses_maintained_adapter =
        config
            .agents
            .get(agent_id)
            .is_some_and(|entry| match &entry.transport {
                AcpTransport::Stdio { command, args, .. } => {
                    harness.uses_maintained_adapter(command, args)
                }
            });
    if harness.id == "codex-acp" && !uses_maintained_adapter {
        eprintln!(
            "note: routing unavailable for '{agent_id}': Codex ACP endpoint configuration \
             requires @agentclientprotocol/codex-acp@1.7.0; launching direct"
        );
        warn_model_dropped("the configured Codex ACP adapter is not the maintained pin");
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
    let require_user_key = user_key_required(
        target_is_local,
        require_key,
        uses_maintained_adapter,
        opts.base_url.is_none(),
        credential_mode,
    );

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
        cloud_credentials.routing_fallback(&base_url).await
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
    if supplied.is_none() && require_user_key {
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

    // Compute one endpoint plan for maintained ACP adapters and render the
    // launch fallback from that same plan. The controller later applies the
    // returned plan through providers/set; no second route description is
    // constructed. Legacy harnesses retain their existing env/args overlay.
    let controller_instance_id = format!("brc_{}", uuid::Uuid::new_v4().simple());
    let endpoint_plan = if uses_maintained_adapter {
        harness.endpoint_plan(
            &base_url,
            &auth,
            opts.model.as_deref(),
            &controller_instance_id,
        )
    } else {
        None
    };
    let overlay = match &endpoint_plan {
        Some(plan) => {
            plan.fallback_overlay()
                .map_err(|error| RoutingError::EndpointConfiguration {
                    via: base_url.clone(),
                    message: format!("could not configure pinned '{}': {error}", harness.id),
                })?
        }
        None => harness.routing_overlay(&base_url, &auth, opts.model.as_deref()),
    };
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
        controller_instance_id: endpoint_plan.as_ref().map(|_| controller_instance_id),
        endpoint_plan,
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

fn controller_identity(
    agent_id: &str,
    command: &str,
    args: &[String],
    endpoint: Option<&crate::harness::HarnessEndpointPlan>,
) -> bitrouter_sdk::acp::controller::ControllerIdentity {
    if let Some(endpoint) = endpoint {
        return bitrouter_sdk::acp::controller::ControllerIdentity::new(
            endpoint.harness_id,
            endpoint.adapter_package,
            endpoint.adapter_version,
        );
    }
    if let Some(harness) = crate::harness::match_invocation(command, args)
        && let Some((package, version)) = harness.maintained_adapter_identity()
        && harness.uses_maintained_adapter(command, args)
    {
        return bitrouter_sdk::acp::controller::ControllerIdentity::new(
            harness.id, package, version,
        );
    }
    bitrouter_sdk::acp::controller::ControllerIdentity::new(
        agent_id,
        "configured-acp-adapter",
        "configured",
    )
}

fn controller_endpoint(
    endpoint: &crate::harness::HarnessEndpointPlan,
) -> bitrouter_sdk::acp::controller::ProviderEndpointPlan {
    let protocol = match endpoint.protocol {
        crate::harness::HarnessProtocol::Anthropic => LlmProtocol::Anthropic,
        crate::harness::HarnessProtocol::OpenAi => LlmProtocol::OpenAi,
    };
    bitrouter_sdk::acp::controller::ProviderEndpointPlan {
        provider_id: endpoint.provider_id.to_string(),
        protocol,
        base_url: endpoint.base_url.clone(),
        headers: endpoint.headers.clone(),
    }
}

struct DaemonRouteControl {
    socket_path: PathBuf,
    controller_instance_id: String,
}

impl DaemonRouteControl {
    fn new(socket_path: PathBuf, controller_instance_id: impl Into<String>) -> Self {
        Self {
            socket_path,
            controller_instance_id: controller_instance_id.into(),
        }
    }

    async fn route_state(
        &self,
        command: crate::daemon::DaemonCommand,
    ) -> std::result::Result<RouteControlState, RouteControlError> {
        match crate::daemon::send_command(&self.socket_path, &command).await {
            Ok(crate::daemon::DaemonResponse::AcpRouteState {
                available, current, ..
            }) => Ok(RouteControlState { available, current }),
            Ok(crate::daemon::DaemonResponse::Error { message }) => {
                if message.starts_with("route is not available") {
                    Err(RouteControlError::invalid_route(message))
                } else {
                    Err(RouteControlError::unavailable(message))
                }
            }
            Ok(other) => Err(RouteControlError::unavailable(format!(
                "daemon returned an unexpected route-control response: {other:?}"
            ))),
            Err(error) => Err(RouteControlError::unavailable(format!(
                "daemon route control is unavailable: {error}"
            ))),
        }
    }
}

#[async_trait::async_trait]
impl AcpRouteControl for DaemonRouteControl {
    async fn list(
        &self,
        session_id: &str,
    ) -> std::result::Result<RouteControlState, RouteControlError> {
        self.route_state(crate::daemon::DaemonCommand::AcpRouteList {
            controller_instance_id: self.controller_instance_id.clone(),
            session_id: session_id.to_string(),
        })
        .await
    }

    async fn set(
        &self,
        session_id: &str,
        route: &str,
    ) -> std::result::Result<RouteControlState, RouteControlError> {
        self.route_state(crate::daemon::DaemonCommand::AcpRouteSet {
            controller_instance_id: self.controller_instance_id.clone(),
            session_id: session_id.to_string(),
            route: route.to_string(),
        })
        .await
    }

    async fn reset(
        &self,
        session_id: &str,
    ) -> std::result::Result<RouteControlState, RouteControlError> {
        self.route_state(crate::daemon::DaemonCommand::AcpRouteReset {
            controller_instance_id: self.controller_instance_id.clone(),
            session_id: session_id.to_string(),
        })
        .await
    }

    async fn session_closed(&self, session_id: &str) -> std::result::Result<(), RouteControlError> {
        self.reset(session_id).await.map(|_| ())
    }

    async fn disconnected(&self) -> std::result::Result<(), RouteControlError> {
        match crate::daemon::send_command(
            &self.socket_path,
            &crate::daemon::DaemonCommand::AcpControllerRevoke {
                controller_instance_id: self.controller_instance_id.clone(),
            },
        )
        .await
        {
            Ok(crate::daemon::DaemonResponse::Ok) => Ok(()),
            Ok(crate::daemon::DaemonResponse::Error { message }) => {
                Err(RouteControlError::unavailable(message))
            }
            Ok(other) => Err(RouteControlError::unavailable(format!(
                "daemon returned an unexpected revoke response: {other:?}"
            ))),
            Err(error) => Err(RouteControlError::unavailable(format!(
                "daemon controller revoke is unavailable: {error}"
            ))),
        }
    }
}

/// Session-attributed spend read back from the daemon's metering store for
/// this controller's own credential-bound traffic.
struct DaemonSessionCost {
    socket_path: PathBuf,
    controller_instance_id: String,
}

/// How long one usage update may wait on the daemon before it is forwarded
/// without a figure. The lookup sits on the controller's forward path, so a
/// stalled daemon must cost the manager a cost line, never its update stream.
const SESSION_COST_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

#[async_trait::async_trait]
impl AcpSessionCost for DaemonSessionCost {
    async fn attributed_cost(&self, session_id: &str) -> Option<Cost> {
        let command = crate::daemon::DaemonCommand::AcpSessionSpend {
            controller_instance_id: self.controller_instance_id.clone(),
            session_id: session_id.to_string(),
        };
        let response = tokio::time::timeout(
            SESSION_COST_LOOKUP_TIMEOUT,
            crate::daemon::send_command(&self.socket_path, &command),
        )
        .await;
        match response {
            Ok(Ok(crate::daemon::DaemonResponse::AcpSessionSpend {
                spend_micro_usd,
                requests,
                unpriced,
            })) => attributed_cost(spend_micro_usd, requests, unpriced),
            Ok(Ok(crate::daemon::DaemonResponse::Error { message })) => {
                tracing::debug!(session_id, %message, "session cost is unavailable");
                None
            }
            Ok(Ok(other)) => {
                tracing::debug!(
                    session_id,
                    ?other,
                    "daemon returned an unexpected session-spend response"
                );
                None
            }
            Ok(Err(error)) => {
                tracing::debug!(session_id, %error, "session cost lookup failed");
                None
            }
            Err(_) => {
                tracing::debug!(session_id, "session cost lookup timed out");
                None
            }
        }
    }
}

/// The figure a session's spend summary puts on the wire, or nothing.
///
/// Absent when no request was attributed to the session, and when none of
/// the attributed requests carries charge evidence: a session BitRouter routed
/// but could not price is not a free one, so it is never `$0.00`. When some
/// rows are priced the figure is their sum — the same floor
/// `bitrouter status --requests` reports alongside its unpriced count.
fn attributed_cost(spend_micro_usd: u64, requests: u64, unpriced: u64) -> Option<Cost> {
    (requests > 0 && unpriced < requests)
        .then(|| Cost::new(spend_micro_usd as f64 / 1_000_000.0, "USD"))
}

/// Session-attributed spend, read **off** the controller's forward path.
///
/// The controller consults its cost bridge inside its dispatch handler, which
/// the SDK runs one message at a time — so a bridge that awaited the daemon
/// there would stop every `session/update` behind it for as long as the
/// lookup took. Under `acp prompt` nobody notices; under interactive `chat` a
/// person watches the transcript stop. This bridge therefore answers from a
/// cache at once and refreshes it in the background, single-flight per
/// session.
///
/// The honesty rule the cache keeps: a figure is stored only when the daemon
/// measured one, so a session that was never metered stays absent rather
/// than becoming `$0.00`; and a stale cumulative figure — the spend as of the
/// previous refresh — is still a figure BitRouter measured, which understates
/// rather than invents. A refresh that yields nothing never erases a figure
/// already held.
struct CachedSessionCost {
    inner: Arc<dyn AcpSessionCost>,
    /// Shared with each in-flight refresh task, which outlives the `&self`
    /// borrow it was spawned from.
    cache: Arc<CostCache>,
}

/// What the cache holds: the last figure the daemon confirmed per session,
/// and which sessions have a refresh in flight — so a burst of usage updates
/// costs one daemon round trip rather than one each.
#[derive(Default)]
struct CostCache {
    figures: std::sync::Mutex<std::collections::HashMap<String, Cost>>,
    refreshing: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl CostCache {
    /// The figure held for `session_id`, if the daemon has ever confirmed one.
    fn held(&self, session_id: &str) -> Option<Cost> {
        match self.figures.lock() {
            Ok(figures) => figures.get(session_id).cloned(),
            Err(poisoned) => poisoned.into_inner().get(session_id).cloned(),
        }
    }

    /// Claim the refresh for `session_id`; `false` when one is already running.
    fn claim_refresh(&self, session_id: &str) -> bool {
        match self.refreshing.lock() {
            Ok(mut refreshing) => refreshing.insert(session_id.to_string()),
            Err(poisoned) => poisoned.into_inner().insert(session_id.to_string()),
        }
    }

    /// Record the daemon's answer and release the refresh claim. `None` keeps
    /// whatever was held: a lookup that yielded nothing is not evidence that
    /// an earlier figure was wrong.
    fn settle_refresh(&self, session_id: &str, figure: Option<Cost>) {
        if let Some(figure) = figure {
            match self.figures.lock() {
                Ok(mut figures) => {
                    figures.insert(session_id.to_string(), figure);
                }
                Err(poisoned) => {
                    poisoned.into_inner().insert(session_id.to_string(), figure);
                }
            }
        }
        match self.refreshing.lock() {
            Ok(mut refreshing) => {
                refreshing.remove(session_id);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove(session_id);
            }
        }
    }
}

impl CachedSessionCost {
    fn new(inner: Arc<dyn AcpSessionCost>) -> Self {
        Self {
            inner,
            cache: Arc::new(CostCache::default()),
        }
    }
}

#[async_trait::async_trait]
impl AcpSessionCost for CachedSessionCost {
    async fn attributed_cost(&self, session_id: &str) -> Option<Cost> {
        // Runs on the forward path: nothing here awaits the daemon. The
        // refresh this update triggers lands in time for the *next* one.
        if self.cache.claim_refresh(session_id) {
            let inner = Arc::clone(&self.inner);
            let cache = Arc::clone(&self.cache);
            let session_id = session_id.to_string();
            tokio::spawn(async move {
                let figure = inner.attributed_cost(&session_id).await;
                cache.settle_refresh(&session_id, figure);
            });
        }
        self.cache.held(session_id)
    }
}

/// A controller credential minted over the daemon's owner-only control
/// socket, and the identity it binds.
///
/// This is what makes the two controller bridges trustworthy: route leases
/// are keyed by the authenticated controller instance, and so is the spend
/// query behind attributed cost. Neither exists without it, which is why a
/// `--direct` session, an unroutable harness, or an explicit `--base-url`
/// (no reviewed trusted binding) yields no binding at all.
///
/// The credential is revoked on every exit through [`Self::revoke`]. It
/// also carries a daemon-side TTL, which is what bounds a panic exit — the
/// one path that runs no teardown.
struct LocalControllerBinding {
    socket_path: PathBuf,
    controller_instance_id: String,
}

impl LocalControllerBinding {
    /// Issue a controller credential for `routed`'s controller instance and
    /// re-render the harness overlay so the child carries that credential
    /// instead of the launch token.
    ///
    /// `None` when the session has no controller instance to bind (direct,
    /// or a harness without a maintained adapter) or when `explicit_base_url`
    /// names a daemon this process cannot vouch for. A credential issued and
    /// then not installable is revoked before the error is returned.
    async fn issue(
        source: &ConfigSource,
        config: &mut Config,
        agent_id: &str,
        routed: &mut Routed,
        explicit_base_url: bool,
    ) -> Result<Option<Self>> {
        let (Some(endpoint), Some(controller_instance_id)) = (
            routed.endpoint_plan.clone(),
            routed.controller_instance_id.clone(),
        ) else {
            return Ok(None);
        };
        if explicit_base_url {
            eprintln!(
                "note: _bitrouter/route/* is unavailable with an explicit --base-url; \
                 the remote model endpoint remains usable without trusted session leases"
            );
            return Ok(None);
        }
        let socket_path = crate::daemon::socket_path_for(source, config);
        let issued = crate::daemon::send_command(
            &socket_path,
            &crate::daemon::DaemonCommand::AcpControllerIssue {
                controller_instance_id: controller_instance_id.clone(),
            },
        )
        .await
        .context("requesting a local ACP controller credential")?;
        let credential = match issued {
            crate::daemon::DaemonResponse::AcpControllerCredential {
                controller_instance_id: confirmed,
                credential,
                ..
            } if confirmed == controller_instance_id => credential,
            crate::daemon::DaemonResponse::Error { message } => {
                return Err(anyhow::anyhow!("ACP controller binding failed: {message}"));
            }
            other => {
                return Err(anyhow::anyhow!(
                    "daemon returned an unexpected ACP credential response: {other:?}"
                ));
            }
        };
        let binding = Self {
            socket_path,
            controller_instance_id,
        };
        let endpoint = endpoint.controller_credential(credential.as_str());
        let overlay = match endpoint.fallback_overlay() {
            Ok(overlay) => overlay,
            Err(error) => {
                binding.revoke().await;
                return Err(error.context("rendering controller-authenticated fallback"));
            }
        };
        if let Some(entry) = config.agents.get_mut(agent_id) {
            let AcpTransport::Stdio { args, env, .. } = &mut entry.transport;
            for (name, value) in overlay.env {
                env.insert(name, value);
            }
            args.extend(overlay.args);
        }
        routed.endpoint_plan = Some(endpoint);
        Ok(Some(binding))
    }

    /// The route bridge this binding makes trustworthy.
    fn route_control(&self) -> Arc<dyn AcpRouteControl> {
        Arc::new(DaemonRouteControl::new(
            self.socket_path.clone(),
            self.controller_instance_id.clone(),
        ))
    }

    /// The cost bridge this binding makes attributable — cached, so the
    /// controller's forward path never waits on the daemon.
    fn session_cost(&self) -> Arc<dyn AcpSessionCost> {
        Arc::new(CachedSessionCost::new(Arc::new(DaemonSessionCost {
            socket_path: self.socket_path.clone(),
            controller_instance_id: self.controller_instance_id.clone(),
        })))
    }

    /// Revoke the credential and every lease it owns. Idempotent on the
    /// daemon side, so calling it after the controller's own disconnect
    /// revoke costs one round trip and nothing else; a failure is logged,
    /// because the caller is already on its way out.
    async fn revoke(&self) {
        let revoked = crate::daemon::send_command(
            &self.socket_path,
            &crate::daemon::DaemonCommand::AcpControllerRevoke {
                controller_instance_id: self.controller_instance_id.clone(),
            },
        )
        .await;
        match revoked {
            Ok(crate::daemon::DaemonResponse::Ok) => {}
            Ok(other) => {
                tracing::debug!(?other, "controller credential revoke was not acknowledged")
            }
            Err(error) => {
                tracing::debug!(%error, "controller credential revoke did not reach the daemon")
            }
        }
    }
}

/// Launch one harness process and expose its connection-level ACP controller
/// over **stdio** until the manager disconnects.
///
/// Session creation, identifiers, lifecycle, history, and persistence remain
/// harness-native. `options` contributes only process-isolation settings here;
/// prompt deadlines belong to the manager on this transparent serve path.
pub async fn serve(ctx: SpawnContext<'_>) -> Result<()> {
    let SpawnContext {
        source,
        mut config,
        agent_id,
        options,
        routing,
    } = ctx;
    let cloud_credentials = crate::cloud::StandaloneCloudCredentials::new();
    // Route the sub-agent's LLM traffic through the daemon (default) unless
    // opted out. Fail fast — before speaking any ACP — so a manager handles
    // "child failed to start" rather than a mid-session provider error.
    //
    // Returned, not `exit(1)`: a caller that never sees a value cannot render
    // one, and the shutdown path below is skipped either way because nothing
    // has been launched yet. `run_acp` renders it to stderr.
    let mut routed = apply_routing_with_cloud_credentials(
        source,
        &mut config,
        agent_id,
        &routing,
        &cloud_credentials,
        RoutingCredentialMode::ControllerIssuedLocal,
    )
    .await
    .map_err(anyhow::Error::new)?;
    config
        .agents
        .get(agent_id)
        .with_context(|| format!("ACP agent '{agent_id}' is not configured"))?
        .validate()
        .with_context(|| format!("invalid ACP agent '{agent_id}'"))?;
    // The trusted local binding both bridges share: the same gate that makes
    // route leases safe makes the spend query attributable, and neither
    // exists without it.
    let binding = LocalControllerBinding::issue(
        source,
        &mut config,
        agent_id,
        &mut routed,
        routing.base_url.is_some(),
    )
    .await?;
    let agent = config
        .agents
        .get(agent_id)
        .with_context(|| format!("ACP agent '{agent_id}' is not configured"))?;
    let AcpTransport::Stdio { command, args, env } = &agent.transport;
    let identity = controller_identity(agent_id, command, args, routed.endpoint_plan.as_ref());
    let mut controller_config = bitrouter_sdk::acp::controller::ControllerConfig::new(identity);
    if let Some(endpoint) = routed.endpoint_plan.as_ref() {
        controller_config = controller_config.endpoint(controller_endpoint(endpoint));
    }
    if options.turn_timeout.is_some() {
        eprintln!(
            "note: --turn-timeout is not enforced by transparent acp serve; \
             the manager controls prompt deadlines"
        );
    }
    let process =
        bitrouter_sdk::acp::up::AgentProcess::new(command.clone(), args.clone(), env.clone())
            .strip_inherited_env(options.strip_inherited_env);
    let mut controller =
        bitrouter_sdk::acp::controller::Controller::new(process, controller_config);
    if let Some(binding) = &binding {
        controller = controller
            .route_control(binding.route_control())
            .session_cost(binding.session_cost());
    }
    controller
        .run(agent_client_protocol::Stdio::new())
        .await
        .map_err(|error| anyhow::anyhow!("acp serve: {error}"))
}

// ── chat ──────────────────────────────────────────────────────────────────────

/// Launch a session for `agent_id` and hand it to the interactive renderer.
///
/// This half is the composition root: it resolves routing, binds the
/// controller credential, launches the harness behind an in-process
/// controller, and attaches observability — all of which need `Config`, the
/// config source, and the daemon's control socket. What it hands over needs
/// none of them, which is why the loop itself lives in
/// [`crate::chat::session`].
pub async fn chat(ctx: SpawnContext<'_>) -> Result<()> {
    use std::io::IsTerminal as _;

    let SpawnContext {
        source,
        mut config,
        agent_id,
        options,
        routing,
    } = ctx;
    let cloud_credentials = crate::cloud::StandaloneCloudCredentials::new();
    // Fail fast, before any agent process exists — a person waiting at a
    // prompt should learn the route is dead now, not mid-turn.
    //
    // Controller-issued: the session's traffic meters under the controller
    // instance rather than a launch token, which is what makes both a route
    // lease and an attributable cost figure possible.
    let mut routed = apply_routing_with_cloud_credentials(
        source,
        &mut config,
        agent_id,
        &routing,
        &cloud_credentials,
        RoutingCredentialMode::ControllerIssuedLocal,
    )
    .await
    .map_err(anyhow::Error::new)?;

    match &routed.via {
        Some(via) => eprintln!("chat: '{agent_id}' routed via bitrouter ({via})"),
        None => eprintln!("chat: '{agent_id}' running direct (not routed, not metered)"),
    }
    let binding = LocalControllerBinding::issue(
        source,
        &mut config,
        agent_id,
        &mut routed,
        routing.base_url.is_some(),
    )
    .await?;

    // A pipe cannot be drawn on. Everything the terminal branch does — the
    // live row, the modals, raw mode — assumes a screen with a cursor on it,
    // and writing those escapes into a file or a `| grep` would corrupt the
    // very output the redirect exists to capture. So a redirected stdout gets
    // the session as plain text instead, which is the one thing a pipe *can*
    // use.
    if !std::io::stdout().is_terminal() {
        return chat_piped(
            &config,
            agent_id,
            &routed,
            options,
            &cloud_credentials,
            binding,
        )
        .await;
    }

    let cwd = std::env::current_dir().context("resolving current directory")?;
    let mcp_servers = options.mcp_servers.clone();
    let mut session = launch_controlled(&config, agent_id, &routed, options, binding)
        .await
        .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let ids = match session.client.new_session(cwd, mcp_servers).await {
        Ok(ids) => ids,
        Err(error) => {
            session.shutdown().await;
            return Err(error.context("opening the harness session"));
        }
    };
    let observability =
        build_observability(&config, agent_id, &ids.acp_session_id, &cloud_credentials).await;
    if let Some(recorder) = observability.recorder.clone() {
        spawn_tool_spans(recorder, session.client.subscribe_updates());
    }

    // The picker exists only when the controller advertised route control —
    // which it does only under a trusted local binding. Said here, before raw
    // mode, because a cooked newline in a raw terminal does not return the
    // carriage.
    if crate::chat::session::can_reroute(&session.client) {
        eprintln!("chat: type /route to change the route mid-session.");
    }
    eprintln!("chat: type a message and press enter; Ctrl-D to end the session.");
    let ended = crate::chat::session::run(
        &mut session,
        &ids.acp_session_id,
        agent_id,
        observability.recorder,
        routed.via.clone(),
    )
    .await;

    if let Some(exporter) = observability.exporter {
        exporter.shutdown();
    }
    ended
}

/// `chat` for a stdout that is not a terminal.
///
/// The same stack `acp serve` exposes and `acp prompt` drives: an in-process
/// controller, the shared client, and harness-native session identity. What
/// differs from `prompt` is only the consumer — a journal rendered per turn
/// rather than an NDJSON stream — which is why the loop itself lives beside
/// the renderers in [`crate::chat::session`].
async fn chat_piped(
    config: &Config,
    agent_id: &str,
    routed: &Routed,
    options: LaunchOptions,
    cloud_credentials: &crate::cloud::StandaloneCloudCredentials,
    binding: Option<LocalControllerBinding>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("resolving current directory")?;
    let mcp_servers = options.mcp_servers.clone();
    let mut session = launch_controlled(config, agent_id, routed, options, binding)
        .await
        .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let ids = match session.client.new_session(cwd, mcp_servers).await {
        Ok(ids) => ids,
        Err(error) => {
            session.shutdown().await;
            return Err(error.context("opening the harness session"));
        }
    };
    let observability =
        build_observability(config, agent_id, &ids.acp_session_id, cloud_credentials).await;
    if let Some(recorder) = observability.recorder.clone() {
        spawn_tool_spans(recorder, session.client.subscribe_updates());
    }

    let ended = crate::chat::session::chat_plain(
        &session.client,
        &ids.acp_session_id,
        agent_id,
        observability.recorder,
    )
    .await;

    // Teardown is here rather than in the loop: the loop borrows the client,
    // and the controller that owns the harness child is this function's.
    session.shutdown().await;
    if let Some(exporter) = observability.exporter {
        exporter.shutdown();
    }
    ended
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
    let cloud_credentials = crate::cloud::StandaloneCloudCredentials::new();
    // Route by default; fail fast with a single structured NDJSON `error`
    // line BEFORE any session side effect (no agent process spawned).
    //
    // The line goes on `out` because it is part of the NDJSON stream the
    // orchestrator is parsing — that is the contract, and it is why this
    // cannot simply propagate. What *is* returned is the same failure as an
    // error value, so the caller controls the exit rather than this function
    // ending the process from inside a library.
    let routed = match apply_routing_with_cloud_credentials(
        source,
        &mut config,
        agent_id,
        &routing,
        &cloud_credentials,
        RoutingCredentialMode::UserOrLaunch,
    )
    .await
    {
        Ok(routed) => routed,
        Err(e) => {
            write_ndjson_line(out, &e.ndjson()).await?;
            out.flush().await.ok();
            return Err(anyhow::Error::new(e));
        }
    };

    let cwd = std::env::current_dir().context("resolving current directory")?;
    let mcp_servers = options.mcp_servers.clone();
    let mut session = launch_controlled(&config, agent_id, &routed, options, None)
        .await
        .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let ids = match session.client.new_session(cwd, mcp_servers).await {
        Ok(ids) => ids,
        Err(error) => {
            session.shutdown().await;
            return Err(error.context("opening the harness session"));
        }
    };
    let observability =
        build_observability(&config, agent_id, &ids.acp_session_id, &cloud_credentials).await;
    if let Some(recorder) = observability.recorder.clone() {
        spawn_tool_spans(recorder, session.client.subscribe_updates());
    }

    // First line: correlate this session with the cost/metering the
    // orchestrator later queries.
    //
    // `session_id` is the harness-native ACP id — the one every subsequent
    // line and every ACP method uses. It is **not** the spend key on this
    // path: metering attributes ACP traffic by the *authenticated* controller
    // instance, and `prompt` routes with a launch token rather than a
    // controller credential, so `requests.controller_instance_id` is null for
    // every row it produces. `launch_id` is what joins here, as it always has.
    //
    // The controller id is deliberately absent rather than reported: the value
    // this process mints reaches the daemon only as a *claimed* header, which
    // §5.5 of the controller spec treats as correlation evidence and not as
    // authorization. Emitting it would name a column that is null. When this
    // path issues a controller credential — as `acp serve` and `chat` do — it
    // can come back and be true.
    //
    // `via` is null when running direct.
    write_ndjson_line(
        out,
        &serde_json::json!({
            "type": "session",
            "session_id": ids.acp_session_id,
            "agent_session_id": ids.agent_session_id,
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
    // attach, so explicitly DENY each request (the reject option). The client
    // also denies whatever is still outstanding when a turn is abandoned or
    // the connection tears down; this loop is the one that answers promptly,
    // so a turn that only needs consent it will never get ends now.
    let mut permissions = session.client.subscribe_permissions();
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
        let clean = session.shutdown().await;
        if let Some(exporter) = observability.exporter {
            exporter.shutdown();
        }
        return teardown_result(clean);
    }

    let turn = Turn {
        client: &session.client,
        session_id: &ids.acp_session_id,
        agent_id,
        recorder: observability.recorder.clone(),
    };
    let outcome = prompt_wait(&turn, text, contract, out).await;
    let clean = session.shutdown().await;
    if let Some(exporter) = observability.exporter {
        // Flush the span batch before exit; spans are lost otherwise.
        exporter.shutdown();
    }
    // The turn's own failure is the more specific thing that went wrong, so it
    // outranks a teardown that did not confirm.
    outcome?;
    teardown_result(clean)
}

/// What a `prompt` exits with when the turn itself was fine.
///
/// A teardown that did not confirm **fails the command**. Before `prompt`
/// moved onto the shared client it did this by `?`-ing on `shutdown()`; the
/// controlled session logs and returns a flag instead, and the flag went
/// unread — so an orchestrator scripting `bitrouter acp prompt` saw exit 0
/// for a run whose harness child may still be alive. That was an undeclared
/// change, and this is it declared: the NDJSON on stdout is already complete
/// and correct, and the status code is what says the process did not end
/// cleanly.
fn teardown_result(clean: bool) -> Result<()> {
    if clean {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "shutting down acp session: teardown did not confirm; see the session log"
        ))
    }
}

/// One harness behind an in-process connection-level controller, with the
/// shared ACP client connected to that controller as its manager.
///
/// This is exactly the stack `acp serve` exposes over stdio; the only
/// difference is that the manager on the other end of the duplex is this
/// process. Session identity, lifecycle, and history therefore stay
/// harness-native, and there is no second scheduler: the controller, the
/// harness child's I/O, and the client all run on this runtime.
pub(crate) struct ControlledSession {
    pub(crate) client: AcpClient,
    /// The controller credential this session's traffic meters under, when
    /// it has one. Revoked by [`ControlledSession::shutdown`] on every exit.
    binding: Option<LocalControllerBinding>,
    /// Resolves once the harness child and its process group are reaped.
    /// Awaited in [`ControlledSession::shutdown`], because tearing the
    /// connection down is not the same as the child being gone — the SDK drops
    /// the transport task rather than letting it confirm the kill.
    reaped: futures::channel::oneshot::Receiver<()>,
    /// The controller's own `run`. It ends when the manager side of the duplex
    /// closes, and awaiting it is what proves the harness child was reaped.
    controller: tokio::task::JoinHandle<std::result::Result<(), anyhow::Error>>,
}

/// How long teardown waits for the controller to finish reaping its harness
/// before giving up and reporting it.
const CONTROLLER_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

impl ControlledSession {
    /// Tear the client down (which closes the duplex and so ends the
    /// controller), wait for the controller to confirm, then revoke the
    /// controller credential. Returns whether every step confirmed; failures
    /// are logged rather than propagated, because the caller has already
    /// asked to stop and there is nothing left to retry — but an interactive
    /// caller wants to know the session ended abnormally, so it can say where
    /// the reason is.
    pub(crate) async fn shutdown(&mut self) -> bool {
        let mut clean = true;
        if let Err(error) = self.client.shutdown().await {
            tracing::warn!(%error, "acp teardown unconfirmed; the harness may not have terminated");
            clean = false;
        }
        // The connection is down; the child may not be. `kill_on_drop` reaches
        // the wrapper (`npx`) and not the `node` it spawned, so the group kill
        // has to be confirmed rather than assumed.
        if tokio::time::timeout(bitrouter_sdk::acp::up::REAP_CONFIRM, &mut self.reaped)
            .await
            .is_err()
        {
            tracing::warn!("harness child not confirmed reaped; a grandchild may have survived");
            clean = false;
        }
        match tokio::time::timeout(CONTROLLER_EXIT_TIMEOUT, &mut self.controller).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => tracing::debug!(%error, "acp controller ended with an error"),
            Ok(Err(error)) => {
                tracing::warn!(%error, "acp controller task failed");
                clean = false;
            }
            Err(_) => {
                tracing::warn!("acp controller did not exit within {CONTROLLER_EXIT_TIMEOUT:?}");
                self.controller.abort();
                clean = false;
            }
        }
        // The controller revokes on its own disconnect; this is the backstop
        // for the paths where it never got that far.
        if let Some(binding) = &self.binding {
            binding.revoke().await;
        }
        clean
    }
}

/// Launch `agent_id` behind an in-process controller and connect the shared
/// client to it. The controller carries the same identity and endpoint plan
/// `acp serve` gives it, so a routed session configures the harness's
/// provider through `providers/set` exactly as a served one does — and, with
/// a `binding`, the same route and cost bridges.
async fn launch_controlled(
    config: &Config,
    agent_id: &str,
    routed: &Routed,
    options: LaunchOptions,
    binding: Option<LocalControllerBinding>,
) -> Result<ControlledSession> {
    let agent = config
        .agents
        .get(agent_id)
        .with_context(|| format!("no acp agent configured for '{agent_id}'"))?;
    agent
        .validate()
        .with_context(|| format!("invalid ACP agent '{agent_id}'"))?;
    let AcpTransport::Stdio { command, args, env } = &agent.transport;

    let identity = controller_identity(agent_id, command, args, routed.endpoint_plan.as_ref());
    let mut controller_config = bitrouter_sdk::acp::controller::ControllerConfig::new(identity);
    if let Some(endpoint) = routed.endpoint_plan.as_ref() {
        controller_config = controller_config.endpoint(controller_endpoint(endpoint));
    }
    let mut process =
        bitrouter_sdk::acp::up::AgentProcess::new(command.clone(), args.clone(), env.clone())
            .strip_inherited_env(options.strip_inherited_env);
    let reaped = process.reaped();
    let mut controller =
        bitrouter_sdk::acp::controller::Controller::new(process, controller_config);
    if let Some(binding) = &binding {
        controller = controller
            .route_control(binding.route_control())
            .session_cost(binding.session_cost());
    }

    // In-process duplex: no bytes, no pipe, no second process between the
    // manager and the controller it drives.
    let (manager_side, controller_side) = agent_client_protocol::Channel::duplex();
    let controller = tokio::spawn(async move {
        controller
            .run(controller_side)
            .await
            .map_err(|error| anyhow::anyhow!("acp controller: {error}"))
    });
    let client = match AcpClient::connect(
        manager_side,
        ClientOptions {
            turn_timeout: options.turn_timeout,
        },
    )
    .await
    {
        Ok(client) => client,
        Err(error) => {
            // Aborted, not awaited: the controller's own disconnect revoke may
            // never run, so the credential is revoked here.
            controller.abort();
            if let Some(binding) = &binding {
                binding.revoke().await;
            }
            return Err(error);
        }
    };
    Ok(ControlledSession {
        client,
        binding,
        reaped,
        controller,
    })
}

/// Everything one prompt turn needs beyond its text: the client to drive, the
/// harness-native session it belongs to, and where its telemetry goes.
struct Turn<'a> {
    client: &'a AcpClient,
    session_id: &'a str,
    agent_id: &'a str,
    recorder: Option<Arc<bitrouter_observe::acp::AcpSpanRecorder>>,
}

/// Inner implementation for the wait (non-`--no-wait`) path.
async fn prompt_wait<W>(
    turn: &Turn<'_>,
    text: &str,
    contract: Option<crate::result_contract::ResultContract>,
    out: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    // Subscribe to updates BEFORE prompting so no streamed update is missed.
    let mut updates = turn.client.subscribe_updates();
    let task = match &contract {
        // The contract clause rides the subagent's task prompt.
        Some(c) => format!("{text}{}", c.instruction()),
        None => text.to_string(),
    };
    let (response, reply) = run_turn(turn, &mut updates, &task, contract.is_some(), out).await?;

    // Extract + validate the machine-consumable result. On failure: ONE
    // repair re-prompt, then `schema_ok:false` + raw text — the orchestrator
    // is never blocked on a malformed reply.
    let (response, result, schema_ok, raw) = match &contract {
        None => (response, None, None, None),
        Some(c) => match c.check(&reply) {
            Ok(value) => (response, Some(value), Some(true), None),
            Err(problem) => {
                let (response, reply) =
                    run_turn(turn, &mut updates, &c.repair_prompt(&problem), true, out).await?;
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
    Ok(())
}

/// Drive one prompt turn: stream its updates to `out` (accumulating message
/// text when `capture`), report the turn's telemetry, and return the typed
/// response plus the reply text.
async fn run_turn<W>(
    turn: &Turn<'_>,
    updates: &mut (impl futures::Stream<Item = SessionUpdateKind> + Unpin),
    text: &str,
    capture: bool,
    out: &mut W,
) -> Result<(agent_client_protocol::schema::v1::PromptResponse, String)>
where
    W: AsyncWrite + Unpin,
{
    let mut reply = String::new();
    let started = std::time::Instant::now();
    // Drive updates and the prompt concurrently. The loop returns the resolved
    // `PromptResponse` directly, so there is no `Option` to unwrap afterward.
    let response = {
        let prompt_future = turn.client.prompt(turn.session_id, text);
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
    report_turn(
        turn.client,
        turn.agent_id,
        turn.recorder.as_ref(),
        &response,
        started.elapsed(),
    );
    Ok((response, reply))
}

/// Re-derive this turn's completion record from the prompt round-trip and hand
/// it to both telemetry consumers.
///
/// The engine's pipeline hook used to be the only thing that produced these.
/// Latency and stop reason are visible right here on the round-trip, and
/// context occupancy on the client's usage slot, so the record is built from
/// what the turn itself observed.
///
/// Takes its inputs loose rather than a `Turn`, because the piped chat path
/// ([`crate::chat::session::chat_plain`]) reports the same record and has no
/// prompt contract, no NDJSON sink, and therefore no `Turn`.
pub(crate) fn report_turn(
    client: &AcpClient,
    agent_id: &str,
    recorder: Option<&Arc<bitrouter_observe::acp::AcpSpanRecorder>>,
    response: &agent_client_protocol::schema::v1::PromptResponse,
    latency: Duration,
) {
    let record = RequestCompleted {
        agent: agent_id.to_string(),
        stop_reason: format!("{:?}", response.stop_reason),
        latency_ms: u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
        context: client.context_usage().lock().ok().and_then(|slot| *slot),
    };
    if let Some(recorder) = recorder {
        recorder.turn_completed(&bitrouter_observe::acp::TurnRecord {
            stop_reason: record.stop_reason.clone(),
            latency,
            context_used: record.context.map(|c| c.used),
            context_size: record.context.map(|c| c.size),
        });
    }
    drain_telemetry_record(record);
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

/// The OTel exporter for this process and, when one exists, the span recorder
/// bound to one session.
struct Observability {
    exporter: Option<Arc<bitrouter_observe::otel::OtelExporter>>,
    recorder: Option<Arc<bitrouter_observe::acp::AcpSpanRecorder>>,
}

/// Build the exporter and the session's span recorder. `conversation_id` is
/// the span attribute every turn and tool span correlates on — the
/// harness-native ACP session id on the controller path.
async fn build_observability(
    config: &Config,
    agent_id: &str,
    conversation_id: &str,
    cloud_credentials: &crate::cloud::StandaloneCloudCredentials,
) -> Observability {
    let exporter =
        crate::assemble::build_otel_exporter_standalone_with_credentials(config, cloud_credentials)
            .await;
    let recorder = exporter.as_ref().map(|exporter| {
        Arc::new(bitrouter_observe::acp::AcpSpanRecorder::new(
            exporter,
            agent_id,
            conversation_id,
        ))
    });
    Observability { exporter, recorder }
}

/// Emit an `execute_tool` span per completed tool call, read off the
/// translated update stream. Exporter-gated by its `recorder` argument:
/// without one there is nothing to emit to.
fn spawn_tool_spans(
    recorder: Arc<bitrouter_observe::acp::AcpSpanRecorder>,
    mut updates: std::pin::Pin<Box<dyn futures::Stream<Item = SessionUpdateKind> + Send>>,
) {
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

#[cfg(test)]
mod standalone_cloud_credentials_tests {
    use std::sync::Arc;

    use bitrouter_providers::hosted::account::credentials::{Credentials, StoredCredential};
    use bitrouter_providers::hosted::account::manager::CredentialManager;
    use chrono::{Duration, Utc};
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cloud::StandaloneCloudCredentials;

    #[tokio::test]
    async fn standalone_wiring_single_flights_routing_and_account_telemetry() -> anyhow::Result<()>
    {
        let server = MockServer::start().await;
        let origin = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": origin,
                "device_authorization_endpoint": format!("{origin}/oauth/device_authorization"),
                "token_endpoint": format!("{origin}/oauth/token"),
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "rotated-access",
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": "rotated-refresh",
                "scope": "inference:invoke",
            })))
            .mount(&server)
            .await;

        let directory = tempfile::tempdir()?;
        let manager = Arc::new(CredentialManager::with_client(
            directory.path().join("account-credentials.json"),
            reqwest::Client::new(),
        ));
        manager
            .save(StoredCredential::from(Credentials {
                access_token: "stale-access".to_owned(),
                refresh_token: Some("original-refresh".to_owned()),
                expires_at: Utc::now() + Duration::seconds(10),
                refresh_token_expires_at: None,
                token_type: "Bearer".to_owned(),
                scope: "inference:invoke".to_owned(),
                client_id: "bitrouter-cli".to_owned(),
                authorization_server: origin.clone(),
                namespace_id: Some("ns-test".to_owned()),
                subject: None,
            }))
            .await?;

        let credentials = StandaloneCloudCredentials::with_manager(manager);
        let telemetry = credentials
            .telemetry_bearer(&origin)
            .await
            .ok_or_else(|| anyhow::anyhow!("account telemetry source was not built"))?;
        let (routing, bearer) =
            tokio::join!(credentials.routing_fallback(&origin), telemetry.bearer(),);
        assert_eq!(routing.as_deref(), Some("rotated-access"));
        assert_eq!(bearer.as_deref(), Some("rotated-access"));

        let requests = server
            .received_requests()
            .await
            .ok_or_else(|| anyhow::anyhow!("wiremock did not record requests"))?;
        let metadata_requests = requests
            .iter()
            .filter(|request| request.url.path() == "/.well-known/oauth-authorization-server")
            .count();
        let refresh_requests = requests
            .iter()
            .filter(|request| request.url.path() == "/oauth/token")
            .count();
        assert_eq!(metadata_requests, 1);
        assert_eq!(refresh_requests, 1);
        Ok(())
    }
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

/// How to launch one harness process and drive it.
///
/// Lived in the SDK while `engine::Session` did the launching. It is the
/// app's now: every field is consumed by a different part of the shared
/// stack — `strip_inherited_env` by `AgentProcess`, `turn_timeout` by
/// `ClientOptions`, `mcp_servers` by `AcpClient::new_session` — so there is no
/// one SDK type it belongs to.
#[derive(Clone, Debug, Default)]
pub struct LaunchOptions {
    /// Inherited environment names to remove before applying the explicit
    /// transport and launch overlays. This lets isolated callers prevent
    /// ambient credentials from crossing into an agent process while still
    /// permitting a deliberately configured credential to win.
    pub strip_inherited_env: Vec<String>,
    /// Per-turn deadline. On elapse the agent is asked to cancel cooperatively
    /// (`session/cancel`); if it does not comply within the client's grace the
    /// turn errors.
    pub turn_timeout: Option<Duration>,
    /// MCP servers passed to the agent in `session/new` (`mcpServers`) — the
    /// caller's tool surface for the session.
    pub mcp_servers: Vec<agent_client_protocol::schema::v1::McpServer>,
}

/// A completed ACP turn, as this process records it.
///
/// Also came from the SDK, where an `ExecutionHook` on the ACP pipeline was
/// the only thing that produced one. That pipeline routed to a single pinned
/// target its executor ignored, and nothing outside the SDK ever registered a
/// hook on it, so it went with the engine. The record survives because it is
/// what the span recorder and the per-turn log consume — rebuilt now from the
/// prompt round-trip itself, which is where the latency and stop reason were
/// visible all along.
#[derive(Debug, Clone)]
pub struct RequestCompleted {
    /// The agent name that handled the turn.
    pub agent: String,
    /// The stop reason rendered as a string (e.g. `"EndTurn"`, `"MaxTokens"`).
    pub stop_reason: String,
    /// Wall-clock latency for the turn in milliseconds.
    pub latency_ms: u64,
    /// Context-window occupancy as of the latest `UsageUpdate`, when the agent
    /// has reported one.
    pub context: Option<bitrouter_sdk::acp::telemetry::ContextUsage>,
}

/// Build [`LaunchOptions`] from the CLI flags shared by `serve` and `prompt`:
/// `--turn-timeout <secs>`.
pub fn launch_options(turn_timeout_secs: Option<u64>) -> LaunchOptions {
    LaunchOptions {
        turn_timeout: turn_timeout_secs.map(std::time::Duration::from_secs),
        ..Default::default()
    }
}

#[cfg(test)]
mod controller_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use agent_client_protocol::schema::v1::Cost;
    use bitrouter_sdk::acp::controller::SessionCost;

    use super::{
        CachedSessionCost, RoutingCredentialMode, attributed_cost, controller_identity,
        user_key_required,
    };

    /// A daemon bridge that does not answer until released, and counts how
    /// often it was asked.
    struct StallingCost {
        release: tokio::sync::Notify,
        asked: AtomicUsize,
        figure: Option<Cost>,
    }

    #[async_trait::async_trait]
    impl SessionCost for StallingCost {
        async fn attributed_cost(&self, _session_id: &str) -> Option<Cost> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
            self.figure.clone()
        }
    }

    async fn settled(cache: &CachedSessionCost, session_id: &str) -> Option<Cost> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let held = cache.cache.held(session_id);
            if held.is_some() || std::time::Instant::now() >= deadline {
                return held;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The forward path never waits on the daemon: the first call returns at
    /// once with no figure, the refresh it started lands in the background,
    /// and the next call reads it. A burst of updates costs one lookup.
    #[tokio::test]
    async fn the_cached_cost_answers_without_awaiting_the_daemon() {
        let daemon = Arc::new(StallingCost {
            release: tokio::sync::Notify::new(),
            asked: AtomicUsize::new(0),
            figure: Some(Cost::new(0.42, "USD")),
        });
        let cache = CachedSessionCost::new(daemon.clone());

        let first = tokio::time::timeout(Duration::from_millis(200), cache.attributed_cost("s1"))
            .await
            .expect("the forward path must not block on the daemon");
        assert_eq!(first, None, "nothing measured yet is nothing, not zero");
        let second = tokio::time::timeout(Duration::from_millis(200), cache.attributed_cost("s1"))
            .await
            .expect("still not blocked while the refresh is in flight");
        assert_eq!(second, None);
        tokio::task::yield_now().await;
        assert_eq!(
            daemon.asked.load(Ordering::SeqCst),
            1,
            "one refresh in flight per session, however many updates arrive"
        );

        daemon.release.notify_one();
        assert_eq!(settled(&cache, "s1").await, Some(Cost::new(0.42, "USD")));
        assert_eq!(
            cache.attributed_cost("s1").await,
            Some(Cost::new(0.42, "USD")),
            "the figure the daemon confirmed is what the next update carries"
        );
    }

    /// A session the daemon never measured stays absent, and a refresh that
    /// yields nothing never erases a figure already held.
    #[tokio::test]
    async fn an_unmeasured_session_stays_absent_and_a_held_figure_survives() {
        let daemon = Arc::new(StallingCost {
            release: tokio::sync::Notify::new(),
            asked: AtomicUsize::new(0),
            figure: None,
        });
        let cache = CachedSessionCost::new(daemon.clone());
        assert_eq!(cache.attributed_cost("never").await, None);
        daemon.release.notify_one();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            cache.attributed_cost("never").await,
            None,
            "no priced evidence is no figure — not $0.00"
        );

        cache
            .cache
            .settle_refresh("held", Some(Cost::new(0.25, "USD")));
        cache.cache.settle_refresh("held", None);
        assert_eq!(
            cache.cache.held("held"),
            Some(Cost::new(0.25, "USD")),
            "a stale measured figure understates; it is never dropped for nothing"
        );
    }

    /// The provenance marker is one spelling in two crates: what the
    /// controller writes is what the renderer reads.
    #[test]
    fn the_cost_marker_is_spelled_the_same_on_both_sides() {
        assert_eq!(
            bitrouter_tui::cost::COST_PROVENANCE_META_KEY,
            bitrouter_sdk::acp::controller::COST_PROVENANCE_META_KEY
        );
        assert_eq!(
            bitrouter_tui::cost::COST_PROVENANCE_ROUTER,
            bitrouter_sdk::acp::controller::COST_PROVENANCE_ROUTER
        );
    }

    /// C5: a session with no attributed traffic, or none that could be
    /// priced, renders nothing — never `$0.00`.
    #[test]
    fn unattributed_or_unpriced_sessions_carry_no_figure() {
        assert_eq!(attributed_cost(0, 0, 0), None);
        assert_eq!(attributed_cost(0, 3, 3), None);
    }

    /// Priced evidence becomes a USD figure; a partially priced session
    /// reports its priced floor rather than hiding it.
    #[test]
    fn priced_evidence_becomes_a_usd_figure() {
        assert_eq!(attributed_cost(420_000, 2, 0), Some(Cost::new(0.42, "USD")));
        assert_eq!(attributed_cost(250_000, 3, 2), Some(Cost::new(0.25, "USD")));
    }

    #[test]
    fn local_maintained_controller_bootstraps_without_a_user_key() {
        assert!(!user_key_required(
            true,
            true,
            true,
            true,
            RoutingCredentialMode::ControllerIssuedLocal,
        ));
        assert!(user_key_required(
            false,
            true,
            true,
            true,
            RoutingCredentialMode::ControllerIssuedLocal,
        ));
        assert!(user_key_required(
            true,
            true,
            false,
            true,
            RoutingCredentialMode::ControllerIssuedLocal,
        ));
        assert!(user_key_required(
            true,
            true,
            true,
            false,
            RoutingCredentialMode::ControllerIssuedLocal,
        ));
        assert!(user_key_required(
            true,
            true,
            true,
            true,
            RoutingCredentialMode::UserOrLaunch,
        ));
        assert!(!user_key_required(
            true,
            false,
            true,
            true,
            RoutingCredentialMode::UserOrLaunch,
        ));
    }

    #[test]
    fn direct_maintained_adapter_keeps_exact_identity() {
        let identity = controller_identity(
            "codex-acp",
            "npx",
            &[
                "-y".to_string(),
                "@agentclientprotocol/codex-acp@1.7.0".to_string(),
            ],
            None,
        );
        assert_eq!(identity.harness_id, "codex-acp");
        assert_eq!(identity.adapter_package, "@agentclientprotocol/codex-acp");
        assert_eq!(identity.adapter_version, "1.7.0");
    }

    #[test]
    fn custom_adapter_identity_exposes_no_launch_command() {
        let identity = controller_identity(
            "private-agent",
            "/private/bin/agent-with-token-in-path",
            &["--secret-argument".to_string()],
            None,
        );
        assert_eq!(identity.harness_id, "private-agent");
        assert_eq!(identity.adapter_package, "configured-acp-adapter");
        assert_eq!(identity.adapter_version, "configured");
        let rendered = format!("{identity:?}");
        assert!(!rendered.contains("token"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn unpinned_catalog_invocation_is_not_reported_as_the_pin() {
        let identity = controller_identity(
            "codex-acp",
            "npx",
            &["@agentclientprotocol/codex-acp@1.6.0".to_string()],
            None,
        );
        assert_eq!(identity.harness_id, "codex-acp");
        assert_eq!(identity.adapter_package, "configured-acp-adapter");
        assert_eq!(identity.adapter_version, "configured");
    }
}
