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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bitrouter_sdk::acp::{AcpAgentConfig, AcpTransport, ConfigAcpRoutingTable};
use bitrouter_sdk::config::Config;
use futures::StreamExt;
use serde::Serialize;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use agent_client_protocol::schema::v1::{
    Cost, LlmProtocol, ProviderCurrentConfig, ProviderId, ProviderInfo, SetProviderRequest,
};
use bitrouter_sdk::acp::client::{AcpClient, ClientOptions};
use bitrouter_sdk::acp::controller::{
    RouteControl as AcpRouteControl, RouteControlError, RouteControlState,
    SessionCost as AcpSessionCost,
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
    /// without one must report its spend as daemon-wide rather than implying
    /// a precision it does not have.
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
    let mut route_control: Option<Arc<dyn AcpRouteControl>> = None;
    let mut session_cost: Option<Arc<dyn AcpSessionCost>> = None;
    if let (Some(endpoint), Some(controller_instance_id)) = (
        routed.endpoint_plan.clone(),
        routed.controller_instance_id.clone(),
    ) {
        if routing.base_url.is_none() {
            let socket_path = crate::daemon::socket_path_for(source, &config);
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
            let endpoint = endpoint.controller_credential(credential.as_str());
            let overlay = match endpoint.fallback_overlay() {
                Ok(overlay) => overlay,
                Err(error) => {
                    let _ = crate::daemon::send_command(
                        &socket_path,
                        &crate::daemon::DaemonCommand::AcpControllerRevoke {
                            controller_instance_id: controller_instance_id.clone(),
                        },
                    )
                    .await;
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
            // Both bridges share the trusted local binding: the same gate
            // that makes route leases safe makes the spend query
            // attributable, and neither exists without it.
            route_control = Some(Arc::new(DaemonRouteControl::new(
                socket_path.clone(),
                controller_instance_id.clone(),
            )));
            session_cost = Some(Arc::new(DaemonSessionCost {
                socket_path,
                controller_instance_id,
            }));
        } else {
            eprintln!(
                "note: _bitrouter/route/* is unavailable with an explicit --base-url; \
                 the remote model endpoint remains usable without trusted session leases"
            );
        }
    }
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
    if let Some(route_control) = route_control {
        controller = controller.route_control(route_control);
    }
    if let Some(session_cost) = session_cost {
        controller = controller.session_cost(session_cost);
    }
    controller
        .run(agent_client_protocol::Stdio::new())
        .await
        .map_err(|error| anyhow::anyhow!("acp serve: {error}"))
}

// ── chat ──────────────────────────────────────────────────────────────────────

/// Launch a session for `agent_id` and hand it to the interactive renderer.
///
/// This half is the composition root: it resolves routing, launches the agent,
/// attaches observability, and builds the routing surface — all of which need
/// `Config`, the config source, and the daemon's control socket. What it hands
/// over needs none of them, which is why the loop itself lives in
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
    let routed = apply_routing_with_cloud_credentials(
        source,
        &mut config,
        agent_id,
        &routing,
        &cloud_credentials,
        RoutingCredentialMode::UserOrLaunch,
    )
    .await
    .map_err(anyhow::Error::new)?;

    let catalog = catalog_from_config(&config)?;
    let cwd = std::env::current_dir().context("resolving current directory")?;
    let session = bitrouter_sdk::acp::engine::Session::launch(&catalog, agent_id, cwd, options)
        .await
        .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let exporter = attach_observability(&config, agent_id, &session, &cloud_credentials).await;

    match &routed.via {
        Some(via) => eprintln!("chat: '{agent_id}' routed via bitrouter ({via})"),
        None => eprintln!("chat: '{agent_id}' running direct (not routed, not metered)"),
    }

    // A pipe cannot be drawn on. Everything below this point — the live row,
    // the modals, raw mode — assumes a screen with a cursor on it, and writing
    // those escapes into a file or a `| grep` would corrupt the very output
    // the redirect exists to capture. So a redirected stdout gets the session
    // as plain text instead, which is the one thing a pipe *can* use.
    let ended = if std::io::stdout().is_terminal() {
        // The picker exists only when this session can actually be rerouted,
        // which needs both a daemon and an attributable launch (task 4.2).
        // Probing that here rather than assuming it is what keeps a dead
        // control off the screen — and it is probed here rather than in the
        // renderer because the daemon socket is this half's to know.
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
        crate::chat::session::run(session, providers, routable, routed.via.clone()).await
    } else {
        crate::chat::session::chat_plain(session).await
    };

    if let Some(exporter) = exporter {
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
    let mut session = launch_controlled(&config, agent_id, &routed, options)
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
    // orchestrator later queries. The identifiers are the harness-native
    // session id and this controller's instance id — the two columns the
    // daemon meters ACP traffic by, so the line actually joins to spend.
    // `via` is null when running direct.
    write_ndjson_line(
        out,
        &serde_json::json!({
            "type": "session",
            "session_id": ids.acp_session_id,
            "agent_session_id": ids.agent_session_id,
            "controller_instance_id": routed.controller_instance_id,
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
        session.shutdown().await;
        if let Some(exporter) = observability.exporter {
            exporter.shutdown();
        }
        return Ok(());
    }

    let turn = Turn {
        client: &session.client,
        session_id: &ids.acp_session_id,
        agent_id,
        recorder: observability.recorder.clone(),
    };
    let outcome = prompt_wait(&turn, text, contract, out).await;
    session.shutdown().await;
    if let Some(exporter) = observability.exporter {
        // Flush the span batch before exit; spans are lost otherwise.
        exporter.shutdown();
    }
    outcome
}

/// One harness behind an in-process connection-level controller, with the
/// shared ACP client connected to that controller as its manager.
///
/// This is exactly the stack `acp serve` exposes over stdio; the only
/// difference is that the manager on the other end of the duplex is this
/// process. Session identity, lifecycle, and history therefore stay
/// harness-native, and there is no second scheduler: the controller, the
/// harness child's I/O, and the client all run on this runtime.
struct ControlledSession {
    client: AcpClient,
    /// The controller's own `run`. It ends when the manager side of the duplex
    /// closes, and awaiting it is what proves the harness child was reaped.
    controller: tokio::task::JoinHandle<std::result::Result<(), anyhow::Error>>,
}

/// How long teardown waits for the controller to finish reaping its harness
/// before giving up and reporting it.
const CONTROLLER_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

impl ControlledSession {
    /// Tear the client down (which closes the duplex and so ends the
    /// controller), then wait for the controller to confirm. Failures are
    /// reported rather than propagated: the caller has already asked to stop
    /// and there is nothing left to retry.
    async fn shutdown(&mut self) {
        if let Err(error) = self.client.shutdown().await {
            tracing::warn!(%error, "acp teardown unconfirmed; the harness may not have terminated");
        }
        match tokio::time::timeout(CONTROLLER_EXIT_TIMEOUT, &mut self.controller).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => tracing::debug!(%error, "acp controller ended with an error"),
            Ok(Err(error)) => tracing::warn!(%error, "acp controller task failed"),
            Err(_) => {
                tracing::warn!("acp controller did not exit within {CONTROLLER_EXIT_TIMEOUT:?}");
                self.controller.abort();
            }
        }
    }
}

/// Launch `agent_id` behind an in-process controller and connect the shared
/// client to it. The controller carries the same identity and endpoint plan
/// `acp serve` gives it, so a routed `prompt` configures the harness's
/// provider through `providers/set` exactly as a served one does.
async fn launch_controlled(
    config: &Config,
    agent_id: &str,
    routed: &Routed,
    options: LaunchOptions,
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
    let process =
        bitrouter_sdk::acp::up::AgentProcess::new(command.clone(), args.clone(), env.clone())
            .strip_inherited_env(options.strip_inherited_env);
    let controller = bitrouter_sdk::acp::controller::Controller::new(process, controller_config);

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
            controller.abort();
            return Err(error);
        }
    };
    Ok(ControlledSession { client, controller })
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
    report_turn(turn, &response, started.elapsed());
    Ok((response, reply))
}

/// Re-derive this turn's completion record from the prompt round-trip and hand
/// it to both telemetry consumers.
///
/// The engine's pipeline hook used to be the only thing that produced these.
/// Latency and stop reason are visible right here on the round-trip, and
/// context occupancy on the client's usage slot, so the record is built from
/// what the turn itself observed.
fn report_turn(
    turn: &Turn<'_>,
    response: &agent_client_protocol::schema::v1::PromptResponse,
    latency: Duration,
) {
    let record = RequestCompleted {
        agent: turn.agent_id.to_string(),
        stop_reason: format!("{:?}", response.stop_reason),
        latency_ms: u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
        context: turn
            .client
            .context_usage()
            .lock()
            .ok()
            .and_then(|slot| *slot),
    };
    if let Some(recorder) = &turn.recorder {
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

/// Attach observability to an engine session when the observe config opts
/// telemetry in: every turn is drained to stderr (always) and, with an
/// exporter, emitted as an OTel GenAI `invoke_agent` span; tool calls become
/// `execute_tool` spans from the translated update stream. Returns the
/// exporter so the caller can flush it (`shutdown`) before exit.
///
/// The `prompt` path builds the same pieces directly — see
/// [`build_observability`] and [`report_turn`] — because it has no pipeline
/// hook to drain and reads latency and stop reason off the prompt round-trip
/// instead.
async fn attach_observability(
    config: &Config,
    agent_id: &str,
    session: &bitrouter_sdk::acp::engine::Session,
    cloud_credentials: &crate::cloud::StandaloneCloudCredentials,
) -> Option<Arc<bitrouter_observe::otel::OtelExporter>> {
    let observability = build_observability(
        config,
        agent_id,
        &session.state().record_id,
        cloud_credentials,
    )
    .await;

    // Telemetry drain: stderr log per turn (always) and the invoke_agent span.
    //
    // One drain, two consumers, because `Session::telemetry()` hands out the
    // receiver exactly once — a second caller gets `None` and silently does
    // nothing. Everything that needs per-turn records has to hang off this
    // loop.
    if let Some(mut rx) = session.telemetry() {
        let recorder = observability.recorder.clone();
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

    if let Some(recorder) = observability.recorder {
        spawn_tool_spans(recorder, session.updates());
    }
    observability.exporter
}

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
mod controller_tests {
    use agent_client_protocol::schema::v1::Cost;

    use super::{RoutingCredentialMode, attributed_cost, controller_identity, user_key_required};

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
