//! Engine — the integration core that wires one live session end-to-end.
//!
//! [`Session`] owns the four substrate pieces for a single agent session and
//! makes them run as one unit:
//!
//! - the [`UpstreamConnection`] (the agent child process + ACP client),
//! - the SDK [`Pipeline`] (`PreRequest → Route → Execute`) whose executor is
//!   a [`SessionExecutor`] bound to this connection and whose `ExecutionHook` is
//!   a [`TelemetryHook`],
//! - the [`TurnController`] that serialises prompts into ordered turns, and
//! - an optional git worktree the agent runs inside.
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
//! still settles the worktree.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
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
use tokio::sync::broadcast;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::executor::SessionExecutor;
use crate::permissions::PermissionRegistry;
use crate::record::{RecordStatus, RecordStore, SessionRecord, now_unix};
use crate::session::{SessionState, SessionStatus};
use crate::telemetry::{RequestCompleted, TelemetryHook};
use crate::transcript::TranscriptEvent;
use crate::translate::SessionUpdateKind;
use crate::turn::TurnController;
use crate::turn_state::TurnState;
use crate::up::{PendingPermission, UpstreamConnection, UpstreamSessionIds};
use crate::worktree::{WorktreeManager, WorktreeSpec};

/// Bound on the per-session turn queue: how many prompts may be enqueued at once
/// before [`TurnController::try_submit`] reports backpressure. `prompt` uses the
/// non-panicking `submit`, so an over-full queue surfaces as a turn error rather
/// than a panic.
const TURN_QUEUE_BOUND: usize = 64;

/// How long a timed-out turn waits for the upstream to honor the
/// `session/cancel` it was sent before the turn is failed outright.
const TURN_CANCEL_GRACE: Duration = Duration::from_secs(3);

/// How long [`Session::shutdown`] waits for the transcript writer to flush its
/// tail after every sender has been dropped.
const TRANSCRIPT_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Capacity of the broadcast that fans [`TurnState`] transitions out to live
/// consumers (the down-facing turn-state forwarder). Turn-state events are
/// human-scale (a few per turn), so this only needs to absorb a brief burst; a
/// consumer that lags past it skips ahead (the durable transcript is the
/// non-lossy record used on reattach).
const TURN_STATE_CHANNEL_CAPACITY: usize = 256;

/// Options for [`Session::launch`].
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Provision (or reuse) a git worktree for the session. `{record16}` in
    /// its `name`/`branch` is replaced with the first 16 hex chars of the
    /// session's record id.
    pub worktree: Option<WorktreeSpec>,
    /// Shell command run (`sh -c`, cwd = the worktree) after a worktree is
    /// **newly created**, before the agent child spawns — the bootstrap hook
    /// for untracked files a worktree doesn't carry (`.env`, `node_modules`).
    /// It executes code: callers must treat it as a gated, human-visible
    /// surface. A failing bootstrap fails the launch (and removes the
    /// just-created worktree). Ignored when no worktree is provisioned or an
    /// existing worktree is reused.
    pub worktree_bootstrap: Option<String>,
    /// Extra environment for the agent child (and the bootstrap hook),
    /// overlaid on the transport's `env` — e.g. a fleet-allocated `PORT`.
    pub env: Vec<(String, String)>,
    /// Write the durable NDJSON transcript (prompts, raw updates, results) to
    /// `.bitrouter/sessions/<record_id>.transcript.ndjson`. On by default.
    pub transcript: bool,
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

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            worktree: None,
            worktree_bootstrap: None,
            env: Vec::new(),
            transcript: true,
            turn_timeout: None,
            mcp_servers: Vec::new(),
        }
    }
}

/// Everything [`Session::build`] needs beyond the agent id; bundled so the
/// launch paths stay readable.
struct BuildArgs {
    transport: AcpTransport,
    base_repo: PathBuf,
    /// The session's record id, minted in `launch_inner` (before worktree
    /// provisioning, so `{record16}` naming can derive from it).
    record_id: String,
    worktree_path: Option<PathBuf>,
    /// Branch checked out in the worktree at provisioning, when known.
    worktree_branch: Option<String>,
    /// Base-repo `HEAD` a newly created worktree branch was cut from.
    worktree_base_ref: Option<String>,
    remove_worktree_on_shutdown: bool,
    /// Extra environment overlaid on the transport's `env` for the child.
    env: Vec<(String, String)>,
    transcript: bool,
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

/// One live session: upstream connection + SDK pipeline + turn queue + worktree.
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
    /// The worktree this session runs in, if one was provisioned.
    worktree: Option<PathBuf>,
    /// Remove the worktree at [`shutdown`](Self::shutdown). Only `true` when
    /// the caller opted in **and** this session newly created the worktree —
    /// worktrees are retained by default because removal (`git worktree remove
    /// --force`) destroys the agent's uncommitted work.
    remove_worktree_on_shutdown: bool,
    /// Manages the worktree lifecycle (rooted at the base repo).
    worktrees: WorktreeManager,
    /// Wire identity, set exactly once — at launch (immediate open) or when
    /// the manager's `session/new` arrives ([`Session::open`]).
    wire: Arc<OnceLock<UpstreamSessionIds>>,
    /// The session's durable on-disk record; wire ids added at open, settled
    /// to `Exited` at shutdown. Mutex because `open` runs behind `&self`.
    record: std::sync::Mutex<SessionRecord>,
    /// Persists [`Self::record`] under `<base_repo>/.bitrouter/sessions/`.
    records: RecordStore,
    /// The transcript writer task, when the transcript is enabled. Awaited at
    /// shutdown so the tail is flushed to disk.
    transcript_writer: Option<tokio::task::JoinHandle<()>>,
    /// Where the transcript lives, when enabled. The down-facing endpoint
    /// replays it for `session/load`.
    transcript_path: Option<PathBuf>,
    /// Receiver for telemetry records emitted by the pipeline's [`TelemetryHook`].
    /// Handed out once by [`Session::telemetry`].
    telemetry_rx: std::sync::Mutex<Option<UnboundedReceiver<RequestCompleted>>>,
    /// Session-scoped registry of outstanding permission requests. The sole
    /// consumer of the upstream (take-once) permission stream; re-exposes it as a
    /// re-subscribable stream so a reattached manager sees the outstanding set
    /// instead of an empty stream. See [`crate::permissions`].
    permissions: Arc<PermissionRegistry>,
    /// Source of [`TurnState`] transitions (`running`/`idle`/`requires_action`),
    /// the durable, replayable turn lifecycle. The turn worker and the permission
    /// pump emit into it; the down-facing endpoint subscribes and encodes each as
    /// a `_bitrouter/turn_state` notification. Re-subscribable (cloned per
    /// [`Session::turn_state`]); the durable transcript is the non-lossy record.
    turn_state_tx: broadcast::Sender<TurnState>,
}

impl Session {
    /// Launch a session and **open it immediately**: resolve `agent_id` in
    /// `catalog`, optionally provision a worktree, spawn the upstream
    /// connection, run `initialize` + `session/new` (cwd = worktree or
    /// `base_repo`, `mcpServers` from [`LaunchOptions::mcp_servers`]), build
    /// the pipeline, turn queue, and transcript, and record the session
    /// identity. Used by the headless `prompt` path and library callers that
    /// have no manager to relay from.
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
            worktree,
            worktree_bootstrap,
            env,
            transcript,
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
        // NOT the upstream `acp_session_id`, and *before* the worktree so
        // `{record16}` naming can derive from it. Keeping it separate from the
        // wire id lets the manager-facing id survive an upstream reconnect
        // (v2) while the upstream wire id can change.
        let record_id = uuid::Uuid::new_v4().to_string();
        let record16: String = record_id.chars().filter(|c| *c != '-').take(16).collect();

        // ── Worktree (optional) ────────────────────────────────────────────
        let worktrees = WorktreeManager::new(base_repo.clone());
        let provisioned = match &worktree {
            Some(spec) => {
                let name = spec.name.replace("{record16}", &record16);
                let branch = spec
                    .branch
                    .as_ref()
                    .map(|b| b.replace("{record16}", &record16));
                Some(worktrees.create(&name, branch.as_deref()).await?)
            }
            None => None,
        };
        let worktree_path = provisioned.as_ref().map(|p| p.path.clone());
        let worktree_branch = provisioned.as_ref().and_then(|p| p.branch.clone());
        let worktree_base_ref = provisioned.as_ref().and_then(|p| p.base_ref.clone());
        // Removal is honored only for a worktree this session newly created; a
        // reused (pre-existing) worktree is never removed by the session.
        let newly_created = provisioned.as_ref().is_some_and(|p| p.newly_created);
        let remove_on_shutdown =
            newly_created && worktree.as_ref().is_some_and(|s| s.remove_on_shutdown);

        // Everything after a successful `create` runs here. If it fails we
        // must remove a just-created worktree before propagating the error, or
        // it leaks on disk. A reused worktree is left untouched.
        let launched = async {
            // ── Bootstrap hook (newly created worktrees only) ──────────────
            // Runs before the agent child spawns so the tree is ready when the
            // agent takes its first look. Callers gate this human-visibly —
            // it executes shell.
            if newly_created && let (Some(cmd), Some(path)) = (&worktree_bootstrap, &worktree_path)
            {
                Self::run_bootstrap_hook(cmd, path, &base_repo, &env).await?;
            }
            Self::build(
                agent_id,
                BuildArgs {
                    transport,
                    base_repo: base_repo.clone(),
                    record_id,
                    worktree_path: worktree_path.clone(),
                    worktree_branch,
                    worktree_base_ref,
                    remove_worktree_on_shutdown: remove_on_shutdown,
                    env,
                    transcript,
                    turn_timeout,
                    mcp_servers,
                    open_now,
                },
            )
            .await
        }
        .await;
        match launched {
            Ok(session) => Ok(session),
            Err(original) => {
                if newly_created && let Some(path) = &worktree_path {
                    // Best-effort cleanup; a remove error must not mask the
                    // original failure that triggered the cleanup.
                    if let Err(remove_err) = worktrees.remove(path).await {
                        tracing::warn!(
                            error = %remove_err,
                            path = %path.display(),
                            "failed to remove worktree after launch error"
                        );
                    }
                }
                Err(original)
            }
        }
    }

    /// Run the worktree bootstrap hook: the shell command with cwd = the
    /// worktree, the launch env overlay applied, and `BITROUTER_BASE_REPO`
    /// pointing at the base repository (so hooks can copy untracked files —
    /// `.env`, caches — from it). A non-zero exit fails the launch.
    async fn run_bootstrap_hook(
        cmd: &str,
        worktree: &std::path::Path,
        base_repo: &std::path::Path,
        env: &[(String, String)],
    ) -> anyhow::Result<()> {
        #[cfg(unix)]
        let (shell, flag) = ("sh", "-c");
        #[cfg(windows)]
        let (shell, flag) = ("cmd", "/C");
        let mut command = tokio::process::Command::new(shell);
        command
            .arg(flag)
            .arg(cmd)
            .current_dir(worktree)
            .env("BITROUTER_BASE_REPO", base_repo);
        for (k, v) in env {
            command.env(k, v);
        }
        let output = command
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("spawning worktree bootstrap hook: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "worktree bootstrap hook failed (status {}): {}",
                output.status,
                stderr.trim()
            );
        }
        Ok(())
    }

    /// The body of [`launch`]/[`launch_deferred`] after the worktree has been
    /// created (if any). Returns a fully wired `Session`, or an error; on
    /// error the caller removes a newly created worktree.
    async fn build(agent_id: &str, args: BuildArgs) -> anyhow::Result<Self> {
        let BuildArgs {
            transport,
            base_repo,
            record_id,
            worktree_path,
            worktree_branch,
            worktree_base_ref,
            remove_worktree_on_shutdown,
            env: extra_env,
            transcript,
            turn_timeout,
            mcp_servers,
            open_now,
        } = args;
        let AcpTransport::Stdio { command, args, env } = &transport;

        // ── Upstream connection (agent child): spawn + initialize only ─────
        // `session/new` happens later — immediately (`open_now`) for the
        // headless/prompt path, or when the manager sends its own
        // `session/new` (whose cwd + mcpServers are relayed) for `serve`.
        // Launch options overlay the transport env (e.g. a fleet `PORT`).
        let env = if extra_env.is_empty() {
            env.clone()
        } else {
            let mut merged = env.clone();
            merged.extend(extra_env.iter().cloned());
            merged
        };
        let conn = Arc::new(UpstreamConnection::spawn(command, args, &env).await?);

        // The record id was minted in `launch_inner` (before the worktree, so
        // `{record16}` naming could derive from it). The down-facing
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
            let cwd = worktree_path.clone().unwrap_or_else(|| base_repo.clone());
            let ids = conn.new_session(cwd, mcp_servers).await?;
            state.set_acp_session_id(ids.acp_session_id.clone());
            if let Some(agent_sid) = &ids.agent_session_id {
                state.set_agent_session_id(agent_sid.clone());
            }
            let _ = wire.set(ids);
        }

        // ── Transcript (durable, non-lossy NDJSON) ─────────────────────────
        // A single writer task owns the file; the connection's non-lossy raw
        // update feed is pumped into it, and the turn closure below adds
        // prompt/result/error events on the same channel.
        let (transcript_tx, transcript_writer, transcript_path) = if transcript {
            let (tx, rx) = unbounded_channel::<TranscriptEvent>();
            let path = crate::transcript::transcript_path(&base_repo, &state.record_id);
            let writer = crate::transcript::spawn_writer(path.clone(), rx);
            if let Some(mut feed) = conn.take_transcript_feed() {
                let update_tx = tx.clone();
                tokio::spawn(async move {
                    while let Some(update) = feed.recv().await {
                        if update_tx
                            .send(TranscriptEvent::Update {
                                update: Box::new(update),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
            (Some(tx), Some(writer), Some(path))
        } else {
            (None, None, None)
        };

        // ── Permission registry (sole consumer of the take-once upstream feed) ─
        // One pump drains the upstream permission stream into a session-scoped
        // registry; every manager connection (re)subscribes to the registry, so a
        // reattached manager sees any permission that was outstanding when it
        // left instead of an empty stream. See `crate::permissions`.
        let permissions = Arc::new(PermissionRegistry::new());

        // ── Turn-state lifecycle (v2-shaped running/idle/requires_action) ──────
        // The turn worker mints a per-session monotonic `turn_seq` and emits
        // `running`/`idle`; the permission pump emits `requires_action` for the
        // active turn. Turns are serialized, so `current_turn` (the last-minted
        // seq) is always the turn a mid-turn permission belongs to.
        let (turn_state_tx, _) = broadcast::channel::<TurnState>(TURN_STATE_CHANNEL_CAPACITY);
        let turn_counter = Arc::new(AtomicU64::new(0));
        let current_turn = Arc::new(AtomicU64::new(0));

        {
            use futures::StreamExt as _;
            let registry = Arc::clone(&permissions);
            let mut upstream_permissions = conn.subscribe_permissions();
            let pump_turn_state = turn_state_tx.clone();
            let pump_current_turn = Arc::clone(&current_turn);
            tokio::spawn(async move {
                while let Some(pending) = upstream_permissions.next().await {
                    // A pending permission blocks the active turn — surface it as
                    // `requires_action` (live-only; not persisted) before parking
                    // it in the registry for a manager to answer.
                    let _ = pump_turn_state.send(TurnState::RequiresAction {
                        turn_seq: pump_current_turn.load(Ordering::Relaxed),
                        request_id: pending.request_id.clone(),
                    });
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
            let turn_transcript = transcript_tx.clone();
            let turn_state_sender = turn_state_tx.clone();
            let turn_counter_for_turns = Arc::clone(&turn_counter);
            let current_turn_for_turns = Arc::clone(&current_turn);
            TurnController::new(
                TURN_QUEUE_BOUND,
                move |blocks: Vec<ContentBlock>| {
                    let pipeline = Arc::clone(&pipeline);
                    let conn = Arc::clone(&conn_for_turn);
                    let agent = agent.clone();
                    let wire = Arc::clone(&turn_wire);
                    let caller = caller.clone();
                    let transcript = turn_transcript.clone();
                    let turn_state = turn_state_sender.clone();
                    let turn_counter = Arc::clone(&turn_counter_for_turns);
                    let current_turn = Arc::clone(&current_turn_for_turns);
                    async move {
                        let Some(ids) = wire.get() else {
                            return Err(anyhow::anyhow!(
                                "no session open: the manager must send session/new first"
                            ));
                        };
                        let acp_session_id = ids.acp_session_id.clone();
                        if let Some(tx) = &transcript {
                            let _ = tx.send(TranscriptEvent::Prompt {
                                blocks: blocks.clone(),
                            });
                        }
                        // Turn start: mint the per-session turn seq, mark it the
                        // active turn (for a mid-turn `requires_action`), and emit
                        // `running` both live and to the durable transcript.
                        let turn_seq = turn_counter.fetch_add(1, Ordering::Relaxed);
                        current_turn.store(turn_seq, Ordering::Relaxed);
                        let _ = turn_state.send(TurnState::Running { turn_seq });
                        if let Some(tx) = &transcript {
                            let _ = tx.send(TranscriptEvent::TurnStart { turn_seq });
                        }
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
                        // Turn end: persist the outcome and emit `idle` live. Our
                        // managers key completion off this `idle` (the v1
                        // `PromptResponse` is a bare ack for them), so it must fire
                        // on both the live and durable paths; a reattaching manager
                        // gets it from the replayed transcript. A failed turn ends
                        // `idle` with no stop reason (v2 `IdleStateUpdate` allows it).
                        if let Some(tx) = &transcript {
                            let _ = tx.send(match &result {
                                Ok(resp) => TranscriptEvent::TurnEnd {
                                    turn_seq,
                                    stop_reason: Some(resp.stop_reason),
                                },
                                Err(e) => TranscriptEvent::Error {
                                    message: e.to_string(),
                                    turn_seq: Some(turn_seq),
                                },
                            });
                        }
                        let _ = turn_state.send(match &result {
                            Ok(resp) => TurnState::Idle {
                                turn_seq,
                                stop_reason: Some(resp.stop_reason),
                            },
                            Err(_) => TurnState::Idle {
                                turn_seq,
                                stop_reason: None,
                            },
                        });
                        result
                    }
                },
                || Ok(PromptResponse::new(StopReason::Cancelled)),
            )
        };

        // ── Durable session record ─────────────────────────────────────────
        // Best-effort: a record-write failure must not fail the launch, but it
        // is surfaced because `bitrouter acp sessions` (and v2 session/load)
        // depend on records existing.
        let record = SessionRecord {
            record_id: state.record_id.clone(),
            agent_id: state.agent_id.clone(),
            acp_session_id: state.acp_session_id.clone(),
            agent_session_id: state.agent_session_id.clone(),
            worktree: worktree_path.clone(),
            branch: worktree_branch,
            base_ref: worktree_base_ref,
            pid: std::process::id(),
            socket: None,
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
            worktree: worktree_path,
            remove_worktree_on_shutdown,
            // `WorktreeManager` is a thin `base_repo` wrapper; a fresh one for
            // the session's own shutdown removal is equivalent to the one
            // `launch` keeps for error-path cleanup.
            worktrees: WorktreeManager::new(base_repo),
            wire,
            record: std::sync::Mutex::new(record),
            records,
            transcript_writer,
            transcript_path,
            telemetry_rx: std::sync::Mutex::new(Some(telemetry_rx)),
            permissions,
            turn_state_tx,
        })
    }

    /// Open the upstream session (`session/new`) for a
    /// [`launch_deferred`](Self::launch_deferred) session, relaying the
    /// **manager's** `cwd` and `mcpServers`. The session's worktree (a launch
    /// argument, operator-chosen) wins over the manager's `cwd`; without
    /// either, the base repo is used.
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
        let cwd = self
            .worktree
            .clone()
            .or(manager_cwd)
            .unwrap_or_else(|| self.worktrees.base_repo().to_path_buf());
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

    /// Stream of [`TurnState`] transitions (`running`/`idle`/`requires_action`).
    /// Each call yields an independent stream from the current point onward. The
    /// down-facing endpoint subscribes and encodes each as a `_bitrouter/turn_state`
    /// notification; the durable transcript carries `TurnStart`/`TurnEnd` for
    /// replay on reattach. **Lossy under lag** (bounded broadcast) — a lagging
    /// subscriber skips ahead; the transcript is the non-lossy record.
    pub fn turn_state(&self) -> Pin<Box<dyn Stream<Item = TurnState> + Send>> {
        use futures::StreamExt as _;
        Box::pin(
            tokio_stream::wrappers::BroadcastStream::new(self.turn_state_tx.subscribe())
                .filter_map(|r| async move { r.ok() }),
        )
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

    /// Where the durable transcript lives, when enabled. The down-facing
    /// endpoint replays it for `session/load`, and advertises `loadSession`
    /// only when this is `Some`.
    pub fn transcript_path(&self) -> Option<&std::path::Path> {
        self.transcript_path.as_deref()
    }

    /// The worktree this session runs in, when one was provisioned. Fleet
    /// managers use it for diff/review over the subagent's work.
    pub fn worktree_path(&self) -> Option<&std::path::Path> {
        self.worktree.as_deref()
    }

    /// Record the unix socket this (warm) session accepts manager reattach on,
    /// so `bitrouter acp attach` / `acp sessions` can discover it. Cleared
    /// automatically at shutdown.
    pub async fn advertise_socket(&self, path: PathBuf) {
        let updated = {
            let mut guard = match self.record.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.socket = Some(path);
            guard.clone()
        };
        if let Err(e) = self.records.write(&updated).await {
            tracing::warn!(error = %e, "failed to record warm-session socket");
        }
    }

    /// Tears the upstream connection down deterministically (killing the agent
    /// child) and settles the worktree (if any): retained by default — it
    /// holds the agent's work — and removed only when the session was launched
    /// with `remove_on_shutdown` and created the worktree itself.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        let Session {
            conn,
            pipeline,
            turn,
            worktree,
            remove_worktree_on_shutdown,
            worktrees,
            record,
            records,
            transcript_writer,
            ..
        } = self;

        // No new turns: dropping the controller closes the worker's job channel.
        drop(turn);
        drop(pipeline);

        // Explicit teardown: the command loop exits, the connection drops, and
        // the agent child is killed; returns once the driver confirms. A
        // failure must not skip worktree settlement, so it is logged instead of
        // propagated.
        if let Err(e) = conn.shutdown().await {
            tracing::warn!(error = %e, "upstream teardown unconfirmed; child may not have terminated");
        }
        drop(conn);

        // Every transcript sender is now gone (the turn worker exited with the
        // controller; the connection's feed closed with the teardown), so the
        // writer flushes its tail and finishes. Bounded wait.
        if let Some(writer) = transcript_writer
            && tokio::time::timeout(TRANSCRIPT_FLUSH_TIMEOUT, writer)
                .await
                .is_err()
        {
            tracing::warn!("transcript writer did not flush within {TRANSCRIPT_FLUSH_TIMEOUT:?}");
        }

        if let Some(path) = worktree {
            if remove_worktree_on_shutdown {
                worktrees.remove(&path).await?;
            } else {
                // The worktree holds the agent's work; surface where it lives.
                tracing::info!(path = %path.display(), "worktree retained");
            }
        }

        // Settle the durable record last so it reflects the final state.
        let mut record = match record.into_inner() {
            Ok(record) => record,
            Err(poisoned) => poisoned.into_inner(),
        };
        record.status = RecordStatus::Exited;
        record.ended_at = Some(now_unix());
        // The socket dies with the process; a stale path must not be advertised.
        record.socket = None;
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
    use crate::worktree::WorktreeSpec;

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

    /// Init a git repo with one commit so `git worktree add` succeeds.
    fn init_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().expect("tempdir");
        for a in [
            &["init", "-q"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
        ] {
            std::process::Command::new("git")
                .current_dir(d.path())
                .args(a)
                .status()
                .expect("git");
        }
        std::fs::write(d.path().join("f"), "x").expect("write");
        std::process::Command::new("git")
            .current_dir(d.path())
            .args(["add", "."])
            .status()
            .expect("git");
        std::process::Command::new("git")
            .current_dir(d.path())
            .args(["commit", "-qm", "init"])
            .status()
            .expect("git");
        d
    }

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

    /// Catalog whose agent points at a non-existent binary, so
    /// `UpstreamConnection::spawn` (and thus `launch`) fails. Used to exercise
    /// the error-path worktree cleanup.
    fn doomed_catalog() -> ConfigAcpRoutingTable {
        let cfg = AcpAgentConfig {
            name: "stub".to_string(),
            transport: AcpTransport::Stdio {
                command: "bitrouter-no-such-binary-xyzzy".to_string(),
                args: vec![],
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

    #[tokio::test]
    async fn launch_in_worktree_then_shutdown_removes_it_when_opted_in() {
        let repo = init_repo();
        let catalog = stub_catalog();

        let session = Session::launch(
            &catalog,
            "stub",
            repo.path().to_path_buf(),
            LaunchOptions {
                worktree: Some(WorktreeSpec {
                    name: "feat-1".to_string(),
                    branch: None,
                    remove_on_shutdown: true,
                }),
                ..LaunchOptions::default()
            },
        )
        .await
        .expect("launch");

        // The worktree was created and the prompt round-trips through it.
        let worktree_path = repo
            .path()
            .join(".bitrouter")
            .join("worktrees")
            .join("feat-1");
        assert!(worktree_path.exists(), "worktree should exist after launch");

        let resp = session.prompt("hi").await.expect("prompt");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);

        session.shutdown().await.expect("shutdown");
        assert!(
            !worktree_path.exists(),
            "worktree should be removed after opt-in shutdown"
        );
    }

    #[tokio::test]
    async fn record16_placeholder_names_worktree_and_branch() {
        let repo = init_repo();
        let catalog = stub_catalog();

        let session = Session::launch(
            &catalog,
            "stub",
            repo.path().to_path_buf(),
            LaunchOptions {
                worktree: Some(WorktreeSpec {
                    name: "codex-{record16}".to_string(),
                    branch: Some("bitrouter/codex-{record16}".to_string()),
                    remove_on_shutdown: false,
                }),
                ..LaunchOptions::default()
            },
        )
        .await
        .expect("launch");

        let record16: String = session
            .state()
            .record_id
            .chars()
            .filter(|c| *c != '-')
            .take(16)
            .collect();
        let worktree_path = repo
            .path()
            .join(".bitrouter")
            .join("worktrees")
            .join(format!("codex-{record16}"));
        assert!(
            worktree_path.exists(),
            "worktree dir derives from the session's record id"
        );
        let head = std::process::Command::new("git")
            .current_dir(&worktree_path)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("git rev-parse");
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            format!("bitrouter/codex-{record16}"),
            "branch derives from the session's record id"
        );

        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn bootstrap_hook_runs_in_new_worktree_before_agent() {
        let repo = init_repo();
        let catalog = stub_catalog();

        let session = Session::launch(
            &catalog,
            "stub",
            repo.path().to_path_buf(),
            LaunchOptions {
                worktree: Some(WorktreeSpec {
                    name: "boot-1".to_string(),
                    branch: None,
                    remove_on_shutdown: false,
                }),
                // Prove cwd = the worktree, the base repo is exported, and the
                // env overlay reaches the hook.
                worktree_bootstrap: Some(
                    "printf '%s %s' \"$BITROUTER_BASE_REPO\" \"$PORT\" > bootstrapped".to_string(),
                ),
                env: vec![("PORT".to_string(), "3101".to_string())],
                ..LaunchOptions::default()
            },
        )
        .await
        .expect("launch");

        let marker = repo
            .path()
            .join(".bitrouter")
            .join("worktrees")
            .join("boot-1")
            .join("bootstrapped");
        let content = std::fs::read_to_string(&marker).expect("bootstrap hook ran");
        assert!(content.contains("3101"), "env overlay reaches the hook");
        assert!(
            content.contains(repo.path().to_str().expect("utf8 path")),
            "BITROUTER_BASE_REPO points at the base repo"
        );

        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn failing_bootstrap_fails_launch_and_removes_new_worktree() {
        let repo = init_repo();
        let catalog = stub_catalog();

        let result = Session::launch(
            &catalog,
            "stub",
            repo.path().to_path_buf(),
            LaunchOptions {
                worktree: Some(WorktreeSpec {
                    name: "boot-bad".to_string(),
                    branch: None,
                    remove_on_shutdown: false,
                }),
                worktree_bootstrap: Some("echo nope >&2; exit 7".to_string()),
                ..LaunchOptions::default()
            },
        )
        .await;

        let err = format!("{:#}", result.err().expect("launch must fail"));
        assert!(err.contains("bootstrap"), "actionable error: {err}");
        assert!(err.contains("nope"), "hook stderr surfaced: {err}");
        assert!(
            !repo
                .path()
                .join(".bitrouter")
                .join("worktrees")
                .join("boot-bad")
                .exists(),
            "just-created worktree cleaned up after bootstrap failure"
        );
    }

    #[tokio::test]
    async fn reused_worktree_skips_the_bootstrap_hook() {
        let repo = init_repo();
        let catalog = stub_catalog();
        let spec = || WorktreeSpec {
            name: "boot-reuse".to_string(),
            branch: None,
            remove_on_shutdown: false,
        };

        // First launch creates the worktree (no hook configured).
        let first = Session::launch(
            &catalog,
            "stub",
            repo.path().to_path_buf(),
            LaunchOptions {
                worktree: Some(spec()),
                ..LaunchOptions::default()
            },
        )
        .await
        .expect("first launch");
        first.shutdown().await.expect("shutdown");

        // Relaunch into the SAME worktree with a hook: it must not run —
        // bootstrap is for newly created trees only.
        let second = Session::launch(
            &catalog,
            "stub",
            repo.path().to_path_buf(),
            LaunchOptions {
                worktree: Some(spec()),
                worktree_bootstrap: Some("touch bootstrapped".to_string()),
                ..LaunchOptions::default()
            },
        )
        .await
        .expect("second launch");
        assert!(
            !repo
                .path()
                .join(".bitrouter")
                .join("worktrees")
                .join("boot-reuse")
                .join("bootstrapped")
                .exists(),
            "reused worktree must not re-run the bootstrap hook"
        );
        second.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn shutdown_retains_worktree_by_default() {
        let repo = init_repo();
        let catalog = stub_catalog();

        let session = Session::launch(
            &catalog,
            "stub",
            repo.path().to_path_buf(),
            LaunchOptions {
                worktree: Some(WorktreeSpec {
                    name: "feat-keep".to_string(),
                    branch: None,
                    remove_on_shutdown: false,
                }),
                ..LaunchOptions::default()
            },
        )
        .await
        .expect("launch");

        let worktree_path = repo
            .path()
            .join(".bitrouter")
            .join("worktrees")
            .join("feat-keep");

        // The agent leaves uncommitted work behind; shutdown must not destroy it.
        std::fs::write(worktree_path.join("wip"), "uncommitted").expect("write");

        session.shutdown().await.expect("shutdown");
        assert!(
            worktree_path.join("wip").exists(),
            "worktree (and uncommitted work) must survive default shutdown"
        );
    }

    #[tokio::test]
    async fn launch_failure_removes_worktree_no_leak() {
        let repo = init_repo();
        let catalog = doomed_catalog();

        let worktree_path = repo
            .path()
            .join(".bitrouter")
            .join("worktrees")
            .join("doomed");

        let result = Session::launch(
            &catalog,
            "stub",
            repo.path().to_path_buf(),
            LaunchOptions {
                worktree: Some(WorktreeSpec {
                    name: "doomed".to_string(),
                    branch: None,
                    remove_on_shutdown: false,
                }),
                ..LaunchOptions::default()
            },
        )
        .await;

        assert!(result.is_err(), "launch should fail on a bad binary");
        assert!(
            !worktree_path.exists(),
            "a newly created worktree must be removed when launch fails"
        );
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

    #[tokio::test]
    async fn transcript_records_prompt_turn_start_updates_and_turn_end() {
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

        session.prompt("hi").await.expect("prompt");
        session.shutdown().await.expect("shutdown");

        let path = crate::transcript::transcript_path(base.path(), &record_id);
        let raw = std::fs::read_to_string(&path).expect("transcript file written");
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid ndjson"))
            .collect();
        let kinds: Vec<&str> = lines
            .iter()
            .map(|l| l["kind"].as_str().expect("kind"))
            .collect();
        // Prompt, turn_start (running), the streamed updates, turn_end (idle).
        assert!(kinds.contains(&"prompt"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"turn_start"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"update"), "kinds: {kinds:?}");
        assert_eq!(*kinds.last().expect("lines"), "turn_end");
        // turn_start precedes turn_end, both carry turn_seq 0.
        let start = lines.iter().find(|l| l["kind"] == "turn_start").unwrap();
        let end = lines.iter().find(|l| l["kind"] == "turn_end").unwrap();
        assert_eq!(start["turn_seq"], 0);
        assert_eq!(end["turn_seq"], 0);
        assert_eq!(end["stop_reason"], "end_turn");
        // seq is strictly monotonic from 0.
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(line["seq"].as_u64(), Some(i as u64));
        }
    }

    /// A prompt turn emits `running` at the start and `idle` (carrying the
    /// upstream `stopReason`) at the end, both on the live `turn_state` stream
    /// and keyed to the same per-session `turn_seq`. This is the completion
    /// signal our managers key off (the `PromptResponse` is a bare ack for them).
    #[tokio::test]
    async fn turn_state_emits_running_then_idle() {
        use crate::turn_state::TurnState;

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

        // Subscribe before prompting so we catch `running`; the broadcast buffers
        // both transitions, which are emitted before `prompt` resolves.
        let mut states = session.turn_state();
        session.prompt("hi").await.expect("prompt");

        assert_eq!(
            states.next().await.expect("running"),
            TurnState::Running { turn_seq: 0 }
        );
        assert_eq!(
            states.next().await.expect("idle"),
            TurnState::Idle {
                turn_seq: 0,
                stop_reason: Some(StopReason::EndTurn),
            }
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

        // No worktree → dropping the last Arc reaps the upstream child.
        drop(session);
    }

    #[tokio::test]
    async fn transcript_disabled_writes_nothing() {
        let base = tempfile::tempdir().expect("tempdir");
        let catalog = stub_catalog();

        let session = Session::launch(
            &catalog,
            "stub",
            base.path().to_path_buf(),
            LaunchOptions {
                transcript: false,
                ..LaunchOptions::default()
            },
        )
        .await
        .expect("launch");
        let record_id = session.state().record_id.clone();
        session.prompt("hi").await.expect("prompt");
        session.shutdown().await.expect("shutdown");

        let path = crate::transcript::transcript_path(base.path(), &record_id);
        assert!(!path.exists(), "transcript must be absent when disabled");
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
