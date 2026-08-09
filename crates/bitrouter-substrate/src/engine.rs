//! Engine — the integration core that wires one live session end-to-end.
//!
//! [`Session`] owns the four substrate pieces for a single agent session and
//! makes them run as one unit:
//!
//! - the [`UpstreamConnection`] (the agent child process + ACP client),
//! - the SDK [`Pipeline`] (`PreRequest → Route → Execute`) whose executor is
//!   a [`SessionExecutor`] bound to this connection and whose `ExecutionHook` is
//!   a [`TelemetryHook`],
//! - the [`TurnController`] that serialises prompts into ordered turns.
//!
//! # Identity (D8)
//!
//! The agent never changes for the life of a `Session`. The pipeline's routing
//! table is pinned to this session's single [`AcpTarget`]; the
//! [`SessionExecutor`] ignores the target anyway (it holds the connection
//! directly), so the pinned table exists only to satisfy the pipeline contract.
//!
//! # Shutdown / child-kill
//!
//! [`Session::shutdown`] tears down **deterministically**: it drops the turn
//! controller (no new turns get queued), then sends an explicit `Shutdown`
//! command into the upstream connection's command loop and awaits its
//! confirmation — the loop exits, the connection (and its transport) drops,
//! and the ACP SDK's child guard kills the agent process. Outstanding
//! `Arc<UpstreamConnection>` clones (the pipeline's executor, a mid-turn
//! worker) only keep the struct alive, not the connection: their subsequent
//! calls fail fast on the closed command channel. If the connection ended on
//! its own earlier (agent crash), shutdown is a no-op for the connection and
//! still settles the session's durable record.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent_client_protocol_schema::v1::{
    ContentBlock, McpServer, PromptRequest, PromptResponse, SessionId, SessionUpdate, StopReason,
    TextContent,
};
use async_trait::async_trait;
use bitrouter_sdk::acp::{
    AcpRequest, AcpRequestPayload, AcpTarget, AcpTransport, ConfigAcpRoutingTable, Pipeline,
    PipelineBuilder, RoutingTable,
};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::error::{BitrouterError, Result as SdkResult};
use futures::Stream;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::executor::SessionExecutor;
use crate::permissions::PermissionRegistry;
use crate::record::{RecordStatus, RecordStore, SessionRecord, now_unix};
use crate::session::{SessionState, SessionStatus};
use crate::telemetry::{RequestCompleted, TelemetryHook};
use crate::translate::SessionUpdateKind;
use crate::turn::TurnController;
use crate::up::{PendingPermission, UpstreamConnection, UpstreamSessionIds};

/// Bound on the per-session turn queue: how many prompts may be enqueued at once
/// before [`TurnController::try_submit`] reports backpressure. `prompt` uses the
/// non-panicking `submit`, so an over-full queue surfaces as a turn error rather
/// than a panic.
const TURN_QUEUE_BOUND: usize = 64;

/// How long a timed-out turn waits for the upstream to honor the
/// `session/cancel` it was sent before the turn is failed outright.
const TURN_CANCEL_GRACE: Duration = Duration::from_secs(3);

/// Options for [`Session::launch`].
#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    /// Extra environment for the agent child, overlaid on the transport's
    /// `env`.
    pub env: Vec<(String, String)>,
    /// Inherited environment names to remove before applying the explicit
    /// transport and launch overlays. This lets isolated callers prevent
    /// ambient credentials from crossing into an agent process while still
    /// permitting a deliberately configured credential to win.
    pub strip_inherited_env: Vec<String>,
    /// Per-turn deadline. On elapse the upstream is asked to cancel
    /// cooperatively (`session/cancel`); if it does not comply within
    /// `TURN_CANCEL_GRACE` (3s) the turn errors.
    pub turn_timeout: Option<Duration>,
    /// MCP servers passed to the agent in `session/new` (`mcpServers`) — the
    /// caller's tool surface for the session, e.g. the TUI's gateway servers.
    /// Only the immediate-open launch path consumes this; a deferred launch
    /// (`launch_deferred`) relays the **manager's** descriptors via
    /// [`Session::open`] instead.
    pub mcp_servers: Vec<McpServer>,
}

/// Everything [`Session::build`] needs beyond the agent id; bundled so the
/// launch paths stay readable.
struct BuildArgs {
    transport: AcpTransport,
    /// The repository the session runs in: the upstream `session/new` cwd
    /// (unless a manager relays its own) and the root of the record store.
    base_repo: PathBuf,
    /// The session's stable manager-facing record id, minted in `launch_inner`.
    record_id: String,
    /// Extra environment overlaid on the transport's `env` for the child.
    env: Vec<(String, String)>,
    /// Environment names stripped from the child before the explicit overlay.
    strip_inherited_env: Vec<String>,
    turn_timeout: Option<Duration>,
    /// `mcpServers` for the immediate `session/new` (unused when deferring —
    /// [`Session::open`] then carries the manager's descriptors).
    mcp_servers: Vec<McpServer>,
    /// Run the upstream `session/new` right away (headless/prompt path) rather
    /// than deferring to [`Session::open`] (serve path).
    open_now: bool,
}

/// A routing table pinned to one session's single [`AcpTarget`].
///
/// The agent is fixed for the life of the session (D8), so this resolves *any*
/// agent name to the same target. The [`SessionExecutor`] ignores the target —
/// it drives the connection it already holds — so the target only needs to
/// exist to satisfy the pipeline's `routing_table → executor` contract.
struct PinnedTable {
    target: AcpTarget,
}

#[async_trait]
impl RoutingTable for PinnedTable {
    async fn resolve(&self, _agent: &str, _caller: &CallerContext) -> SdkResult<AcpTarget> {
        Ok(self.target.clone())
    }
}

/// One live session: upstream connection + SDK pipeline + turn queue.
pub struct Session {
    /// Manager-facing identity + status.
    pub state: SessionState,
    /// The upstream ACP connection (agent child). Shared with the pipeline's
    /// executor; see the module-level shutdown note.
    conn: Arc<UpstreamConnection>,
    /// The SDK routing/execution pipeline for this session.
    pipeline: Arc<Pipeline>,
    /// Serialises prompts into ordered turns, each carrying the prompt's
    /// content blocks verbatim and yielding a [`PromptResponse`].
    turn: TurnController<Vec<ContentBlock>, PromptResponse>,
    /// The repository the session runs in — the `session/new` cwd fallback
    /// when no manager relays one of its own.
    base_repo: PathBuf,
    /// Wire identity, set exactly once — at launch (immediate open) or when
    /// the manager's `session/new` arrives ([`Session::open`]).
    wire: Arc<OnceLock<UpstreamSessionIds>>,
    /// The session's durable on-disk record; wire ids added at open, settled
    /// to `Exited` at shutdown. Mutex because `open` runs behind `&self`.
    record: std::sync::Mutex<SessionRecord>,
    /// Persists [`Self::record`] under `<base_repo>/.bitrouter/sessions/`.
    records: RecordStore,
    /// Receiver for telemetry records emitted by the pipeline's [`TelemetryHook`].
    /// Handed out once by [`Session::telemetry`].
    telemetry_rx: std::sync::Mutex<Option<UnboundedReceiver<RequestCompleted>>>,
    /// Session-scoped registry of outstanding permission requests. The sole
    /// consumer of the upstream (take-once) permission stream; re-exposes it as a
    /// re-subscribable stream so a reattached manager sees the outstanding set
    /// instead of an empty stream. See [`crate::permissions`].
    permissions: Arc<PermissionRegistry>,
}

impl Session {
    /// Launch a session and **open it immediately**: resolve `agent_id` in
    /// `catalog`, spawn the upstream connection, run `initialize` +
    /// `session/new` (cwd = `base_repo`, `mcpServers` from
    /// [`LaunchOptions::mcp_servers`]), build the pipeline and turn queue, and
    /// record the session identity. Used by the headless `prompt` path and
    /// library callers that have no manager to relay from.
    pub async fn launch(
        catalog: &ConfigAcpRoutingTable,
        agent_id: &str,
        base_repo: PathBuf,
        options: LaunchOptions,
    ) -> anyhow::Result<Self> {
        Self::launch_inner(catalog, agent_id, base_repo, options, true).await
    }

    /// Launch a session with the upstream `session/new` **deferred**: the
    /// agent is spawned and initialized (so its capabilities can be relayed to
    /// the manager), but the session is created only when [`Session::open`] is
    /// called — by the down-facing endpoint, with the **manager's** `cwd` and
    /// `mcpServers` relayed verbatim. Prompts before `open` fail with a clear
    /// error. Used by `bitrouter acp serve`.
    pub async fn launch_deferred(
        catalog: &ConfigAcpRoutingTable,
        agent_id: &str,
        base_repo: PathBuf,
        options: LaunchOptions,
    ) -> anyhow::Result<Self> {
        Self::launch_inner(catalog, agent_id, base_repo, options, false).await
    }

    async fn launch_inner(
        catalog: &ConfigAcpRoutingTable,
        agent_id: &str,
        base_repo: PathBuf,
        options: LaunchOptions,
        open_now: bool,
    ) -> anyhow::Result<Self> {
        let LaunchOptions {
            env,
            strip_inherited_env,
            turn_timeout,
            mcp_servers,
        } = options;
        // ── Resolve the agent's stdio transport ────────────────────────────
        let transport = catalog
            .lookup(agent_id)
            .ok_or_else(|| anyhow::anyhow!("no acp agent configured for '{agent_id}'"))?
            .clone();

        // ── Identity (D8/D10) ──────────────────────────────────────────────
        // `record_id` is a STABLE, distinct manager-facing id — minted here,
        // NOT the upstream `acp_session_id`. Keeping it separate from the wire
        // id lets the manager-facing id survive an upstream reconnect (v2)
        // while the upstream wire id can change.
        let record_id = uuid::Uuid::new_v4().to_string();

        Self::build(
            agent_id,
            BuildArgs {
                transport,
                base_repo,
                record_id,
                env,
                strip_inherited_env,
                turn_timeout,
                mcp_servers,
                open_now,
            },
        )
        .await
    }

    /// The body of [`launch`]/[`launch_deferred`]: returns a fully wired
    /// `Session`, or an error.
    async fn build(agent_id: &str, args: BuildArgs) -> anyhow::Result<Self> {
        let BuildArgs {
            transport,
            base_repo,
            record_id,
            env: extra_env,
            strip_inherited_env,
            turn_timeout,
            mcp_servers,
            open_now,
        } = args;
        let AcpTransport::Stdio { command, args, env } = &transport;

        // ── Upstream connection (agent child): spawn + initialize only ─────
        // `session/new` happens later — immediately (`open_now`) for the
        // headless/prompt path, or when the manager sends its own
        // `session/new` (whose cwd + mcpServers are relayed) for `serve`.
        // Launch options overlay the transport env.
        let env = if extra_env.is_empty() {
            env.clone()
        } else {
            let mut merged = env.clone();
            merged.extend(extra_env.iter().cloned());
            merged
        };
        let conn = Arc::new(
            UpstreamConnection::spawn_with_stripped_env(command, args, &env, &strip_inherited_env)
                .await?,
        );

        // The record id was minted in `launch_inner`. The down-facing
        // `SessionAgent` returns `record_id` for `session/new`; the upstream
        // `acp_session_id` stays internal.
        let mut state = SessionState::new(record_id, agent_id.to_string());
        state.status = SessionStatus::Idle;

        // Wire identity slot: set exactly once, either right below (`open_now`)
        // or by `Session::open` when the manager's `session/new` arrives. The
        // turn closure and `cancel` read it; a prompt before the session is
        // open fails with a clear error.
        let wire: Arc<OnceLock<UpstreamSessionIds>> = Arc::new(OnceLock::new());
        if open_now {
            let ids = conn.new_session(base_repo.clone(), mcp_servers).await?;
            state.set_acp_session_id(ids.acp_session_id.clone());
            if let Some(agent_sid) = &ids.agent_session_id {
                state.set_agent_session_id(agent_sid.clone());
            }
            let _ = wire.set(ids);
        }

        // ── Permission registry (sole consumer of the take-once upstream feed) ─
        // One pump drains the upstream permission stream into a session-scoped
        // registry; every manager connection (re)subscribes to the registry, so a
        // reattached manager sees any permission that was outstanding when it
        // left instead of an empty stream. See `crate::permissions`.
        let permissions = Arc::new(PermissionRegistry::new());

        {
            use futures::StreamExt as _;
            let registry = Arc::clone(&permissions);
            let mut upstream_permissions = conn.subscribe_permissions();
            tokio::spawn(async move {
                while let Some(pending) = upstream_permissions.next().await {
                    // Park the pending permission in the registry for a manager to
                    // answer; every manager connection (re)subscribes to see the set.
                    registry.insert(pending);
                }
            });
        }

        // ── Pipeline (pinned table + session executor + telemetry hook) ─────
        let target = AcpTarget {
            agent_name: agent_id.to_string(),
            transport,
        };
        let (telemetry_tx, telemetry_rx) = unbounded_channel::<RequestCompleted>();
        let executor = Arc::new(SessionExecutor::new(Arc::clone(&conn)));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(Arc::new(PinnedTable { target }))
            .executor(executor)
            .execution_hook(TelemetryHook::new(telemetry_tx, conn.context_usage()));
        let pipeline = Arc::new(
            builder
                .build()
                .map_err(|e| anyhow::anyhow!("building acp pipeline: {e}"))?,
        );

        // ── Turn queue ─────────────────────────────────────────────────────
        // Each turn builds an `AcpRequest` for the prompt's content blocks
        // (forwarded verbatim — multi-modal, not text-flattened) and drives it
        // through the pipeline under the optional per-turn deadline, returning
        // the typed `PromptResponse`. A queued turn flushed by `cancel`
        // resolves as `StopReason::Cancelled` without running.
        let turn = {
            let pipeline = Arc::clone(&pipeline);
            let conn_for_turn = Arc::clone(&conn);
            // The request's agent field carries the configured agent id — the
            // pinned table resolves any name to this session's target, and the
            // telemetry hook reports this field, so it must be the real agent
            // name (not the record id).
            let agent = agent_id.to_string();
            let turn_wire = Arc::clone(&wire);
            let caller = CallerContext::local();
            TurnController::new(
                TURN_QUEUE_BOUND,
                move |blocks: Vec<ContentBlock>| {
                    let pipeline = Arc::clone(&pipeline);
                    let conn = Arc::clone(&conn_for_turn);
                    let agent = agent.clone();
                    let wire = Arc::clone(&turn_wire);
                    let caller = caller.clone();
                    async move {
                        let Some(ids) = wire.get() else {
                            return Err(anyhow::anyhow!(
                                "no session open: the manager must send session/new first"
                            ));
                        };
                        let acp_session_id = ids.acp_session_id.clone();
                        let req = AcpRequest::new(
                            agent,
                            AcpRequestPayload::Prompt(PromptRequest::new(
                                SessionId::new(acp_session_id.clone()),
                                blocks,
                            )),
                            caller,
                        );
                        let run = async {
                            pipeline
                                .execute(req)
                                .await
                                .map(|resp| resp.result)
                                .map_err(|e: BitrouterError| anyhow::anyhow!(e.to_string()))
                        };
                        tokio::pin!(run);
                        let result: anyhow::Result<PromptResponse> = match turn_timeout {
                            None => run.await,
                            Some(deadline) => {
                                match tokio::time::timeout(deadline, &mut run).await {
                                    Ok(result) => result,
                                    Err(_) => {
                                        // Deadline hit: ask the upstream to end the
                                        // turn cooperatively, then give it a short
                                        // grace to comply (it should resolve with
                                        // `StopReason::Cancelled`).
                                        let _ = conn.cancel(&acp_session_id).await;
                                        match tokio::time::timeout(TURN_CANCEL_GRACE, &mut run)
                                            .await
                                        {
                                            Ok(result) => result,
                                            Err(_) => Err(anyhow::anyhow!(
                                                "turn timed out after {deadline:?} and the upstream \
                                                 did not cancel within {TURN_CANCEL_GRACE:?}"
                                            )),
                                        }
                                    }
                                }
                            }
                        };
                        result
                    }
                },
                || Ok(PromptResponse::new(StopReason::Cancelled)),
            )
        };

        // ── Durable session record ─────────────────────────────────────────
        // Best-effort: a record-write failure must not fail the launch, but it
        // is surfaced because managers depend on records existing.
        let record = SessionRecord {
            record_id: state.record_id.clone(),
            agent_id: state.agent_id.clone(),
            acp_session_id: state.acp_session_id.clone(),
            agent_session_id: state.agent_session_id.clone(),
            pid: std::process::id(),
            started_at: now_unix(),
            status: RecordStatus::Running,
            ended_at: None,
        };
        let records = RecordStore::new(&base_repo);
        if let Err(e) = records.write(&record).await {
            tracing::warn!(error = %e, "failed to write session record");
        }

        Ok(Self {
            state,
            conn,
            pipeline,
            turn,
            base_repo,
            wire,
            record: std::sync::Mutex::new(record),
            records,
            telemetry_rx: std::sync::Mutex::new(Some(telemetry_rx)),
            permissions,
        })
    }

    /// Open the upstream session (`session/new`) for a
    /// [`launch_deferred`](Self::launch_deferred) session, relaying the
    /// **manager's** `cwd` and `mcpServers`. Without a relayed `cwd` the base
    /// repo is used.
    ///
    /// Idempotent: opening an already-open session (including one launched
    /// with the immediate-open [`launch`](Self::launch)) is a no-op — the
    /// first opener's arguments win, matching the endpoint contract that
    /// `session/new` always answers with the same `record_id`.
    pub async fn open(
        &self,
        manager_cwd: Option<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> anyhow::Result<()> {
        if self.wire.get().is_some() {
            tracing::debug!("session already open; ignoring session/new arguments");
            return Ok(());
        }
        let cwd = manager_cwd.unwrap_or_else(|| self.base_repo.clone());
        let ids = self.conn.new_session(cwd, mcp_servers).await?;
        // A concurrent open may have won the race; first one in wins.
        if self.wire.set(ids.clone()).is_err() {
            return Ok(());
        }
        // Persist the wire identity into the durable record.
        let updated = {
            let mut guard = match self.record.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.acp_session_id = Some(ids.acp_session_id.clone());
            guard.agent_session_id = ids.agent_session_id.clone();
            guard.clone()
        };
        if let Err(e) = self.records.write(&updated).await {
            tracing::warn!(error = %e, "failed to update session record after open");
        }
        Ok(())
    }

    /// Enqueue a text prompt, await its turn, and return the typed
    /// [`PromptResponse`]. Convenience over [`prompt_blocks`](Self::prompt_blocks).
    pub async fn prompt(&self, text: &str) -> anyhow::Result<PromptResponse> {
        self.prompt_blocks(vec![ContentBlock::Text(TextContent::new(text.to_string()))])
            .await
    }

    /// Enqueue a prompt carrying arbitrary content blocks (text, images,
    /// resources, …) **verbatim**, await its turn, and return the typed
    /// [`PromptResponse`]. The down-facing `SessionAgent` forwards each
    /// manager `session/prompt` through this, so multi-modal content reaches
    /// the upstream agent unmodified.
    pub async fn prompt_blocks(&self, blocks: Vec<ContentBlock>) -> anyhow::Result<PromptResponse> {
        let rx = self.turn.submit(blocks);
        rx.await
            .map_err(|_| anyhow::anyhow!("turn worker dropped the reply"))?
    }

    /// Cancel the session's work, matching ACP's session-scoped
    /// `session/cancel`: the queued backlog is flushed (each queued turn
    /// resolves as `StopReason::Cancelled` without running) and the active
    /// turn is cancelled cooperatively via the upstream. A no-op before the
    /// session is open (nothing can be in flight).
    pub async fn cancel(&self) -> anyhow::Result<()> {
        self.turn.flush();
        match self.wire.get() {
            Some(ids) => self.conn.cancel(&ids.acp_session_id).await,
            None => Ok(()),
        }
    }

    /// Stream of translated `session/update` notifications. Each call yields an
    /// independent stream from the current point onward.
    pub fn updates(&self) -> Pin<Box<dyn Stream<Item = SessionUpdateKind> + Send>> {
        self.conn.subscribe_updates()
    }

    /// Stream of **raw** ACP `session/update` notifications, untranslated. Each
    /// call yields an independent stream from the current point onward. The
    /// down-facing `SessionAgent` uses this to forward upstream updates to its
    /// manager verbatim.
    pub fn raw_updates(&self) -> Pin<Box<dyn Stream<Item = SessionUpdate> + Send>> {
        self.conn.subscribe_raw_updates()
    }

    /// Stream of pending permission requests. **Re-subscribable**: each call
    /// yields its own stream that first replays every still-unresolved permission,
    /// then streams new ones. A reattached manager therefore sees any permission
    /// that was outstanding when the previous connection dropped, and dropping a
    /// stream (a manager detach) no longer defaults the upstream to Deny while the
    /// session lives. Backed by the session's [`PermissionRegistry`].
    pub fn permissions(&self) -> Pin<Box<dyn Stream<Item = PendingPermission> + Send>> {
        self.permissions.subscribe()
    }

    /// Receiver of [`RequestCompleted`] telemetry records emitted by the
    /// pipeline's hook. Single-consumer: the first call returns the receiver,
    /// later calls return `None`.
    pub fn telemetry(&self) -> Option<UnboundedReceiver<RequestCompleted>> {
        self.telemetry_rx.lock().ok().and_then(|mut g| g.take())
    }

    /// The session's identity + status.
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// The upstream agent's `initialize` response, captured at handshake. The
    /// down-facing `SessionAgent` reflects these capabilities to its manager.
    pub fn upstream_init(&self) -> &agent_client_protocol_schema::v1::InitializeResponse {
        self.conn.upstream_init()
    }

    /// Tears the upstream connection down deterministically (killing the agent
    /// child) and settles the session's durable record.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        let Session {
            conn,
            pipeline,
            turn,
            record,
            records,
            ..
        } = self;

        // No new turns: dropping the controller closes the worker's job channel.
        drop(turn);
        drop(pipeline);

        // Explicit teardown: the command loop exits, the connection drops, and
        // the agent child is killed; returns once the driver confirms. A
        // failure must not skip record settlement, so it is logged instead of
        // propagated.
        if let Err(e) = conn.shutdown().await {
            tracing::warn!(error = %e, "upstream teardown unconfirmed; child may not have terminated");
        }
        drop(conn);

        // Settle the durable record last so it reflects the final state.
        let mut record = match record.into_inner() {
            Ok(record) => record,
            Err(poisoned) => poisoned.into_inner(),
        };
        record.status = RecordStatus::Exited;
        record.ended_at = Some(now_unix());
        if let Err(e) = records.write(&record).await {
            tracing::warn!(error = %e, "failed to update session record");
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashMap;

    use agent_client_protocol_schema::v1::StopReason;
    use bitrouter_sdk::acp::{AcpAgentConfig, AcpTransport, ConfigAcpRoutingTable};
    use futures::StreamExt;

    use super::{LaunchOptions, Session};

    /// Bash stub: ACP handshake + a streamed `session/update` (message chunk,
    /// then a `usage_update`) + prompt result. Mirrors the `up.rs` stub so we
    /// exercise `launch` + `prompt` end-to-end without a real agent.
    const BASH_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
            *session/prompt*) printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}\n';
                              printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"usage_update","used":1500,"size":200000}}}\n';
                              printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
          esac
        done
    "#;

    fn stub_catalog() -> ConfigAcpRoutingTable {
        let cfg = AcpAgentConfig {
            name: "stub".to_string(),
            transport: AcpTransport::Stdio {
                command: "bash".to_string(),
                args: vec!["-c".to_string(), BASH_STUB.to_string()],
                env: HashMap::new(),
            },
        };
        ConfigAcpRoutingTable::from_configs([("stub".to_string(), cfg)]).expect("catalog")
    }

    #[tokio::test]
    async fn launch_then_prompt_returns_response() {
        let base = tempfile::tempdir().expect("tempdir");
        let catalog = stub_catalog();

        let session = Session::launch(
            &catalog,
            "stub",
            base.path().to_path_buf(),
            LaunchOptions::default(),
        )
        .await
        .expect("launch");

        // Subscribe BEFORE prompting so the streamed update is observed.
        let mut updates = session.updates();

        let resp = session.prompt("hi").await.expect("prompt");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);

        let ev = updates.next().await.expect("streamed update");
        assert!(format!("{ev:?}").contains("hi"), "unexpected: {ev:?}");

        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn state_carries_identity() {
        let base = tempfile::tempdir().expect("tempdir");
        let catalog = stub_catalog();

        let session = Session::launch(
            &catalog,
            "stub",
            base.path().to_path_buf(),
            LaunchOptions::default(),
        )
        .await
        .expect("launch");

        assert_eq!(session.state().acp_session_id.as_deref(), Some("u1"));
        assert_eq!(session.state().agent_id, "stub");

        session.shutdown().await.expect("shutdown");
    }

    /// Deferred launch: prompting before `open` fails with a clear error;
    /// `open` relays the manager's `mcpServers` (and cwd) into the upstream
    /// `session/new` — the stub proves it by echoing a marker session id when
    /// the request carried the probe MCP server; the wire id lands in the
    /// durable record.
    #[tokio::test]
    async fn deferred_launch_relays_manager_mcp_servers_on_open() {
        use agent_client_protocol_schema::v1::{McpServer, McpServerStdio};

        const RELAY_STUB: &str = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*) printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*)
                    case "$line" in
                      *relay-probe-server*) sid="saw-mcp";;
                      *) sid="no-mcp";;
                    esac
                    printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"%s"}}\n' "$id" "$sid";;
                *session/prompt*) printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
              esac
            done
        "#;
        let cfg = bitrouter_sdk::acp::AcpAgentConfig {
            name: "relay".to_string(),
            transport: AcpTransport::Stdio {
                command: "bash".to_string(),
                args: vec!["-c".to_string(), RELAY_STUB.to_string()],
                env: HashMap::new(),
            },
        };
        let catalog =
            ConfigAcpRoutingTable::from_configs([("relay".to_string(), cfg)]).expect("catalog");
        let base = tempfile::tempdir().expect("tempdir");

        let session = Session::launch_deferred(
            &catalog,
            "relay",
            base.path().to_path_buf(),
            LaunchOptions::default(),
        )
        .await
        .expect("launch_deferred");

        // Prompting before the manager opens the session must fail clearly.
        let early = session.prompt("too early").await;
        let err = early.expect_err("prompt before open must fail");
        assert!(
            err.to_string().contains("session/new"),
            "unexpected error: {err}"
        );

        // Open with the manager's cwd + an MCP server; the stub marks the
        // session id when it sees the server in the request.
        let probe = McpServer::Stdio(McpServerStdio::new("relay-probe-server", "probe-cmd"));
        session
            .open(Some(base.path().to_path_buf()), vec![probe])
            .await
            .expect("open");

        // The relayed request produced the marker wire id, persisted into the
        // durable record.
        let records = crate::record::RecordStore::new(base.path())
            .list()
            .await
            .expect("list records");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].acp_session_id.as_deref(),
            Some("saw-mcp"),
            "upstream session/new must carry the manager's mcpServers"
        );

        // Prompting now works, and open is idempotent.
        let resp = session.prompt("hi").await.expect("prompt after open");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        session
            .open(None, vec![])
            .await
            .expect("second open is a no-op");

        session.shutdown().await.expect("shutdown");
    }

    /// Immediate launch: `LaunchOptions::mcp_servers` rides the upstream
    /// `session/new` — the same stub echoes a marker session id when the
    /// request carried the probe server.
    #[tokio::test]
    async fn launch_passes_options_mcp_servers_in_session_new() {
        use agent_client_protocol_schema::v1::{McpServer, McpServerStdio};

        const RELAY_STUB: &str = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*) printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*)
                    case "$line" in
                      *launch-probe-server*) sid="saw-mcp";;
                      *) sid="no-mcp";;
                    esac
                    printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"%s"}}\n' "$id" "$sid";;
              esac
            done
        "#;
        let cfg = bitrouter_sdk::acp::AcpAgentConfig {
            name: "relay".to_string(),
            transport: AcpTransport::Stdio {
                command: "bash".to_string(),
                args: vec!["-c".to_string(), RELAY_STUB.to_string()],
                env: HashMap::new(),
            },
        };
        let catalog =
            ConfigAcpRoutingTable::from_configs([("relay".to_string(), cfg)]).expect("catalog");
        let base = tempfile::tempdir().expect("tempdir");

        let session = Session::launch(
            &catalog,
            "relay",
            base.path().to_path_buf(),
            LaunchOptions {
                mcp_servers: vec![McpServer::Stdio(McpServerStdio::new(
                    "launch-probe-server",
                    "probe-cmd",
                ))],
                ..Default::default()
            },
        )
        .await
        .expect("launch");
        assert_eq!(
            session.state().acp_session_id.as_deref(),
            Some("saw-mcp"),
            "immediate session/new must carry LaunchOptions::mcp_servers"
        );
        session.shutdown().await.expect("shutdown");
    }

    /// A permission outstanding when a manager "detaches" (drops its
    /// `permissions()` stream without answering) is **not** denied, and a
    /// reattached manager (a fresh `permissions()` subscription) is re-issued the
    /// same permission and can answer it — end-to-end through the real upstream
    /// stub, the engine pump, and the session registry. Proves the Phase-1
    /// detach/reattach fix above the `permissions` unit tests.
    #[cfg(unix)]
    #[tokio::test]
    async fn outstanding_permission_survives_detach_and_reissues_on_reattach() {
        use std::sync::Arc;

        use agent_client_protocol_schema::v1::{
            PermissionOptionKind, RequestPermissionOutcome, SelectedPermissionOutcome,
        };

        // Stub issues a permission mid-prompt (allow + reject options), reads the
        // client's response, echoes the chosen optionId, then ends the turn.
        const PERM_STUB: &str = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
                *session/prompt*)
                    printf '{"jsonrpc":"2.0","id":"99","method":"session/request_permission","params":{"sessionId":"u1","toolCall":{"toolCallId":"tc1","title":"do thing"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"rej","name":"Reject","kind":"reject_once"}]}}\n'
                    read resp
                    chosen=$(echo "$resp" | sed -n 's/.*"optionId":"\([^"]*\)".*/\1/p')
                    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"chose:%s"}}}}\n' "$chosen"
                    printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
              esac
            done
        "#;

        let base = tempfile::tempdir().expect("tempdir");
        let cfg = AcpAgentConfig {
            name: "stub".to_string(),
            transport: AcpTransport::Stdio {
                command: "bash".to_string(),
                args: vec!["-c".to_string(), PERM_STUB.to_string()],
                env: HashMap::new(),
            },
        };
        let catalog =
            ConfigAcpRoutingTable::from_configs([("stub".to_string(), cfg)]).expect("catalog");
        let session = Arc::new(
            Session::launch(
                &catalog,
                "stub",
                base.path().to_path_buf(),
                LaunchOptions::default(),
            )
            .await
            .expect("launch"),
        );

        // Drive the prompt concurrently; it completes only after the permission
        // is answered.
        let prompt_session = Arc::clone(&session);
        let prompt = tokio::spawn(async move { prompt_session.prompt("do X").await });

        // "Manager 1" receives the permission but detaches without answering.
        let mut first = session.permissions();
        let pending = first.next().await.expect("permission forwarded");
        assert_eq!(pending.tool_call.fields.title.as_deref(), Some("do thing"));
        let request_id = pending.request_id.clone();
        drop(pending);
        drop(first);

        // "Manager 2" reattaches: a fresh subscription re-issues the SAME
        // outstanding permission (same request id), proving it was neither lost
        // nor denied on detach.
        let mut second = session.permissions();
        let reissued = second
            .next()
            .await
            .expect("permission re-issued on reattach");
        assert_eq!(
            reissued.request_id, request_id,
            "must be the same permission"
        );
        assert_eq!(reissued.tool_call.fields.title.as_deref(), Some("do thing"));

        // Answer with the allow option; the exact selection reaches the upstream.
        let allow_id = reissued
            .options
            .iter()
            .find(|o| matches!(o.kind, PermissionOptionKind::AllowOnce))
            .map(|o| o.option_id.clone())
            .expect("allow option present");
        reissued.resolve(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(allow_id),
        ));

        // With the permission answered, the turn completes end-to-end.
        let resp = tokio::time::timeout(std::time::Duration::from_secs(5), prompt)
            .await
            .expect("prompt did not hang")
            .expect("join")
            .expect("prompt");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);

        // Dropping the last Arc reaps the upstream child.
        drop(session);
    }

    /// Turn timeout: the stub never answers `session/prompt` directly, but
    /// honors `session/cancel` by resolving the pending prompt with
    /// `stopReason: "cancelled"`. A short `turn_timeout` must trigger the
    /// cooperative-cancel path and return `Cancelled` promptly.
    #[tokio::test]
    async fn turn_timeout_cancels_cooperatively() {
        const STALL_STUB: &str = r#"
            pending=""
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
                *session/prompt*) pending="$id";;
                *session/cancel*)
                    if [ -n "$pending" ]; then
                      printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"cancelled"}}\n' "$pending"
                      pending=""
                    fi;;
              esac
            done
        "#;
        let cfg = bitrouter_sdk::acp::AcpAgentConfig {
            name: "stall".to_string(),
            transport: AcpTransport::Stdio {
                command: "bash".to_string(),
                args: vec!["-c".to_string(), STALL_STUB.to_string()],
                env: HashMap::new(),
            },
        };
        let catalog =
            ConfigAcpRoutingTable::from_configs([("stall".to_string(), cfg)]).expect("catalog");
        let base = tempfile::tempdir().expect("tempdir");

        let session = Session::launch(
            &catalog,
            "stall",
            base.path().to_path_buf(),
            LaunchOptions {
                turn_timeout: Some(std::time::Duration::from_millis(200)),
                ..LaunchOptions::default()
            },
        )
        .await
        .expect("launch");

        let started = std::time::Instant::now();
        let resp = session.prompt("never answered").await.expect("prompt");
        assert_eq!(resp.stop_reason, StopReason::Cancelled);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "cooperative cancel must resolve well before the grace bound"
        );

        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn session_record_written_running_then_exited() {
        use crate::record::{RecordStatus, RecordStore};

        let base = tempfile::tempdir().expect("tempdir");
        let catalog = stub_catalog();

        let session = Session::launch(
            &catalog,
            "stub",
            base.path().to_path_buf(),
            LaunchOptions::default(),
        )
        .await
        .expect("launch");
        let record_id = session.state().record_id.clone();

        let store = RecordStore::new(base.path());
        let records = store.list().await.expect("list");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_id, record_id);
        assert_eq!(records[0].status, RecordStatus::Running);
        assert_eq!(records[0].pid, std::process::id());
        assert!(records[0].ended_at.is_none());

        session.shutdown().await.expect("shutdown");

        let records = store.list().await.expect("list");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, RecordStatus::Exited);
        assert!(records[0].ended_at.is_some());
    }

    #[tokio::test]
    async fn telemetry_emits_request_completed() {
        let base = tempfile::tempdir().expect("tempdir");
        let catalog = stub_catalog();

        let session = Session::launch(
            &catalog,
            "stub",
            base.path().to_path_buf(),
            LaunchOptions::default(),
        )
        .await
        .expect("launch");

        let mut telemetry = session.telemetry().expect("telemetry receiver");

        let resp = session.prompt("hi").await.expect("prompt");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);

        let record = telemetry.recv().await.expect("telemetry record");
        assert_eq!(record.stop_reason, "EndTurn");
        // The stub streamed a usage_update mid-turn; the hook snapshots it.
        assert_eq!(
            record.context,
            Some(crate::telemetry::ContextUsage {
                used: 1500,
                size: 200_000,
            })
        );

        session.shutdown().await.expect("shutdown");
    }
}
