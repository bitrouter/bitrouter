//! Engine — the integration core that wires one live session end-to-end.
//!
//! [`Session`] owns the four substrate pieces for a single agent session and
//! makes them run as one unit:
//!
//! - the [`UpstreamConnection`] (the agent child process + ACP client),
//! - the SDK [`Pipeline`] (`PreRequest → Route → Execute`) whose [`Executor`] is
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
use std::sync::Arc;

use agent_client_protocol_schema::v1::{
    ContentBlock, PromptRequest, PromptResponse, SessionId, SessionUpdate, TextContent,
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
use crate::session::{SessionState, SessionStatus};
use crate::telemetry::{RequestCompleted, TelemetryHook};
use crate::translate::SessionUpdateKind;
use crate::turn::TurnController;
use crate::up::{PendingPermission, UpstreamConnection};
use crate::worktree::{WorktreeManager, WorktreeSpec};

/// Bound on the per-session turn queue: how many prompts may be enqueued at once
/// before [`TurnController::try_submit`] reports backpressure. `prompt` uses the
/// non-panicking `submit`, so an over-full queue surfaces as a turn error rather
/// than a panic.
const TURN_QUEUE_BOUND: usize = 64;

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
    /// Receiver for telemetry records emitted by the pipeline's [`TelemetryHook`].
    /// Handed out once by [`Session::telemetry`].
    telemetry_rx: std::sync::Mutex<Option<UnboundedReceiver<RequestCompleted>>>,
}

impl Session {
    /// Launch a session: resolve `agent_id` in `catalog`, optionally provision
    /// a worktree, spawn the upstream connection, build the pipeline and turn
    /// queue, and record the session identity.
    pub async fn launch(
        catalog: &ConfigAcpRoutingTable,
        agent_id: &str,
        base_repo: PathBuf,
        worktree: Option<WorktreeSpec>,
    ) -> anyhow::Result<Self> {
        // ── Resolve the agent's stdio transport ────────────────────────────
        let transport = catalog
            .lookup(agent_id)
            .ok_or_else(|| anyhow::anyhow!("no acp agent configured for '{agent_id}'"))?
            .clone();

        // ── Worktree (optional) ────────────────────────────────────────────
        let worktrees = WorktreeManager::new(base_repo.clone());
        let provisioned = match &worktree {
            Some(spec) => Some(worktrees.create(&spec.name).await?),
            None => None,
        };
        let worktree_path = provisioned.as_ref().map(|p| p.path.clone());
        // Removal is honored only for a worktree this session newly created; a
        // reused (pre-existing) worktree is never removed by the session.
        let newly_created = provisioned.as_ref().is_some_and(|p| p.newly_created);
        let remove_on_shutdown =
            newly_created && worktree.as_ref().is_some_and(|s| s.remove_on_shutdown);

        // Everything after a successful `create` runs in `build`. If it fails we
        // must remove a just-created worktree before propagating the error, or
        // it leaks on disk. A reused worktree is left untouched.
        match Self::build(
            agent_id,
            transport,
            base_repo,
            worktree_path.clone(),
            remove_on_shutdown,
        )
        .await
        {
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

    /// The body of [`launch`] after the worktree has been created (if any).
    /// Returns a fully wired `Session`, or an error; on error the caller
    /// ([`launch`]) removes the worktree.
    async fn build(
        agent_id: &str,
        transport: AcpTransport,
        base_repo: PathBuf,
        worktree_path: Option<PathBuf>,
        remove_worktree_on_shutdown: bool,
    ) -> anyhow::Result<Self> {
        let AcpTransport::Stdio { command, args, env } = &transport;
        let cwd = worktree_path.clone().unwrap_or_else(|| base_repo.clone());

        // ── Upstream connection (agent child) ──────────────────────────────
        let conn = Arc::new(UpstreamConnection::spawn(command, args, env, Some(cwd)).await?);
        let acp_session_id = conn.acp_session_id().to_string();

        // ── Identity (D8/D10) ──────────────────────────────────────────────
        // `record_id` is a STABLE, distinct manager-facing id — minted here, NOT
        // the upstream `acp_session_id`. Keeping them separate lets the
        // manager-facing id survive an upstream reconnect (v2) while the upstream
        // wire id can change. The down-facing `SessionAgent` returns `record_id`
        // for `session/new`; the upstream `acp_session_id` stays internal.
        let record_id = uuid::Uuid::new_v4().to_string();
        let mut state = SessionState::new(record_id, agent_id.to_string());
        state.set_acp_session_id(acp_session_id.clone());
        if let Some(agent_sid) = conn.agent_session_id() {
            state.set_agent_session_id(agent_sid.to_string());
        }
        state.status = SessionStatus::Idle;

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
        // through the pipeline, returning the typed `PromptResponse`.
        let turn = {
            let pipeline = Arc::clone(&pipeline);
            let record_id = state.record_id.clone();
            let acp_session_id = acp_session_id.clone();
            let caller = CallerContext::local();
            TurnController::new(TURN_QUEUE_BOUND, move |blocks: Vec<ContentBlock>| {
                let pipeline = Arc::clone(&pipeline);
                let record_id = record_id.clone();
                let acp_session_id = acp_session_id.clone();
                let caller = caller.clone();
                async move {
                    let req = AcpRequest::new(
                        record_id,
                        AcpRequestPayload::Prompt(PromptRequest::new(
                            SessionId::new(acp_session_id),
                            blocks,
                        )),
                        caller,
                    );
                    let resp = pipeline
                        .execute(req)
                        .await
                        .map_err(|e: BitrouterError| anyhow::anyhow!(e.to_string()))?;
                    Ok::<PromptResponse, anyhow::Error>(resp.result)
                }
            })
        };

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
            telemetry_rx: std::sync::Mutex::new(Some(telemetry_rx)),
        })
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

    /// Cancel the in-flight turn cooperatively via the upstream
    /// (`session/cancel`). v1: this affects the active turn, not the queued
    /// backlog (see [`crate::turn`]).
    pub async fn cancel(&self) -> anyhow::Result<()> {
        self.conn.cancel(self.conn.acp_session_id()).await
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

    /// Stream of pending permission requests. Single-consumer: the first call
    /// returns the live stream, later calls return an empty stream.
    pub fn permissions(&self) -> Pin<Box<dyn Stream<Item = PendingPermission> + Send>> {
        self.conn.subscribe_permissions()
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

        if let Some(path) = worktree {
            if remove_worktree_on_shutdown {
                worktrees.remove(&path).await?;
            } else {
                // The worktree holds the agent's work; surface where it lives.
                tracing::info!(path = %path.display(), "worktree retained");
            }
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

    use super::Session;
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

        let session = Session::launch(&catalog, "stub", base.path().to_path_buf(), None)
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

        let session = Session::launch(&catalog, "stub", base.path().to_path_buf(), None)
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
            Some(WorktreeSpec {
                name: "feat-1".to_string(),
                remove_on_shutdown: true,
            }),
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
    async fn shutdown_retains_worktree_by_default() {
        let repo = init_repo();
        let catalog = stub_catalog();

        let session = Session::launch(
            &catalog,
            "stub",
            repo.path().to_path_buf(),
            Some(WorktreeSpec {
                name: "feat-keep".to_string(),
                remove_on_shutdown: false,
            }),
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
            Some(WorktreeSpec {
                name: "doomed".to_string(),
                remove_on_shutdown: false,
            }),
        )
        .await;

        assert!(result.is_err(), "launch should fail on a bad binary");
        assert!(
            !worktree_path.exists(),
            "a newly created worktree must be removed when launch fails"
        );
    }

    #[tokio::test]
    async fn telemetry_emits_request_completed() {
        let base = tempfile::tempdir().expect("tempdir");
        let catalog = stub_catalog();

        let session = Session::launch(&catalog, "stub", base.path().to_path_buf(), None)
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
