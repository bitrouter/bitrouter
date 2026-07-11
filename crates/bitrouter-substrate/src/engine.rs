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
//! The agent child dies when the upstream connection's driver future ends, which
//! happens once **every** `Arc<UpstreamConnection>` clone is dropped (that closes
//! the command channel the driver selects on). Two long-lived clones exist: the
//! one [`Session`] holds, and the one the pipeline's [`SessionExecutor`] holds.
//! The pipeline clone is reachable from two places — `Session`'s own
//! `Arc<Pipeline>` and the [`TurnController`] worker task that captured a clone
//! of it. [`Session::shutdown`] therefore drops the turn controller (which lets
//! the worker task finish and release its pipeline clone), drops `Session`'s
//! pipeline clone, waits for the connection's strong count to fall to one, then
//! drops the connection and finally removes the worktree.

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
use crate::worktree::WorktreeManager;

/// Bound on the per-session turn queue: how many prompts may be enqueued at once
/// before [`TurnController::try_submit`] reports backpressure. `prompt` uses the
/// non-panicking `submit`, so an over-full queue surfaces as a turn error rather
/// than a panic.
const TURN_QUEUE_BOUND: usize = 64;

/// How long [`Session::shutdown`] waits for the turn worker to finish (an
/// in-flight turn is nudged with `session/cancel` first). Bounded so a stuck
/// turn can never wedge shutdown.
const SHUTDOWN_WORKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long [`Session::shutdown`] waits for the upstream driver thread to be
/// joined (which kills and reaps the agent child). Generous: the SDK unwind
/// includes killing the child and draining its pipes.
const SHUTDOWN_DRIVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// Serialises prompts into ordered turns, each yielding a [`PromptResponse`].
    turn: TurnController<PromptResponse>,
    /// The worktree this session runs in, if one was created. Removed on
    /// shutdown.
    worktree: Option<PathBuf>,
    /// Manages the worktree lifecycle (rooted at the base repo).
    worktrees: WorktreeManager,
    /// Receiver for telemetry records emitted by the pipeline's [`TelemetryHook`].
    /// Handed out once by [`Session::telemetry`].
    telemetry_rx: std::sync::Mutex<Option<UnboundedReceiver<RequestCompleted>>>,
}

impl Session {
    /// Launch a session: resolve `agent_id` in `catalog`, optionally create a
    /// worktree, spawn the upstream connection, build the pipeline and turn
    /// queue, and record the session identity.
    pub async fn launch(
        catalog: &ConfigAcpRoutingTable,
        agent_id: &str,
        base_repo: PathBuf,
        worktree: Option<&str>,
    ) -> anyhow::Result<Self> {
        // ── Resolve the agent's stdio transport ────────────────────────────
        let transport = catalog
            .lookup(agent_id)
            .ok_or_else(|| anyhow::anyhow!("no acp agent configured for '{agent_id}'"))?
            .clone();

        // ── Worktree (optional) ────────────────────────────────────────────
        let worktrees = WorktreeManager::new(base_repo.clone());
        let worktree_path = match worktree {
            Some(name) => Some(worktrees.create(name).await?),
            None => None,
        };

        // Everything after a successful `create` runs in `build`. If it fails we
        // must remove the just-created worktree before propagating the error, or
        // it leaks on disk.
        match Self::build(agent_id, transport, base_repo, worktree_path.clone()).await {
            Ok(session) => Ok(session),
            Err(original) => {
                if let Some(path) = &worktree_path {
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
            .execution_hook(TelemetryHook::new(telemetry_tx));
        let pipeline = Arc::new(
            builder
                .build()
                .map_err(|e| anyhow::anyhow!("building acp pipeline: {e}"))?,
        );

        // ── Turn queue ─────────────────────────────────────────────────────
        // Each turn builds an `AcpRequest` for `text` and drives it through the
        // pipeline, returning the typed `PromptResponse`.
        let turn = {
            let pipeline = Arc::clone(&pipeline);
            let record_id = state.record_id.clone();
            let acp_session_id = acp_session_id.clone();
            let caller = CallerContext::local();
            TurnController::new(TURN_QUEUE_BOUND, move |text: String| {
                let pipeline = Arc::clone(&pipeline);
                let record_id = record_id.clone();
                let acp_session_id = acp_session_id.clone();
                let caller = caller.clone();
                async move {
                    let req = AcpRequest::new(
                        record_id,
                        AcpRequestPayload::Prompt(PromptRequest::new(
                            SessionId::new(acp_session_id),
                            vec![ContentBlock::Text(TextContent::new(text))],
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
            // `WorktreeManager` is a thin `base_repo` wrapper; a fresh one for
            // the session's own shutdown removal is equivalent to the one
            // `launch` keeps for error-path cleanup.
            worktrees: WorktreeManager::new(base_repo),
            telemetry_rx: std::sync::Mutex::new(Some(telemetry_rx)),
        })
    }

    /// Enqueue a prompt, await its turn, and return the typed [`PromptResponse`].
    pub async fn prompt(&self, text: &str) -> anyhow::Result<PromptResponse> {
        let rx = self.turn.submit(text.to_string());
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

    /// Deterministic teardown: cancel any in-flight turn, await the turn
    /// worker's exit (releasing its pipeline → executor → connection clone),
    /// then shut the connection down — which **joins the driver thread**, so
    /// the SDK's child-kill destructor has provably run before this returns.
    /// Finally removes the worktree (if any).
    ///
    /// Every wait is bounded; on a bound being exceeded the affected task or
    /// thread keeps unwinding detached and a warning is logged, because a
    /// process that exits immediately afterwards may strand the child alive.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        let Session {
            conn,
            pipeline,
            turn,
            worktree,
            worktrees,
            ..
        } = self;

        // Nudge any in-flight turn to complete cooperatively so the worker can
        // exit within its bound. Best-effort: an idle session has nothing to
        // cancel and a closed loop returns an error either way.
        let _ = conn.cancel(conn.acp_session_id()).await;

        // Close the queue and await the worker task — deterministic release of
        // its captured `Arc<Pipeline>` (and through it the executor's
        // connection clone).
        if !turn.shutdown(SHUTDOWN_WORKER_TIMEOUT).await {
            tracing::warn!("turn worker still running an in-flight turn at teardown");
        }
        // Drop `Session`'s own pipeline clone (the other `Arc<Pipeline>`).
        drop(pipeline);

        // With the worker gone this is the sole connection handle; consume it
        // and join the driver thread so the agent child is killed and reaped
        // before we return.
        match Arc::try_unwrap(conn) {
            Ok(only) => {
                if !only.shutdown(SHUTDOWN_DRIVER_TIMEOUT).await {
                    tracing::warn!(
                        "upstream driver did not exit within its shutdown bound; child may not have terminated"
                    );
                }
            }
            Err(conn) => {
                // The worker (or another holder) still has a clone — dropping
                // ours won't end the driver, so the child may outlive us.
                tracing::warn!(
                    strong_count = Arc::strong_count(&conn),
                    "upstream connection still referenced at teardown; child may not have terminated"
                );
            }
        }

        if let Some(path) = worktree {
            worktrees.remove(&path).await?;
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

    /// Bash stub: ACP handshake + a streamed `session/update` + prompt result.
    /// Mirrors the `up.rs` stub so we exercise `launch` + `prompt` end-to-end
    /// without a real agent.
    const BASH_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
            *session/prompt*) printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}\n';
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
    async fn launch_in_worktree_then_shutdown_removes_it() {
        let repo = init_repo();
        let catalog = stub_catalog();

        let session = Session::launch(&catalog, "stub", repo.path().to_path_buf(), Some("feat-1"))
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
            "worktree should be removed after shutdown"
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

        let result =
            Session::launch(&catalog, "stub", repo.path().to_path_buf(), Some("doomed")).await;

        assert!(result.is_err(), "launch should fail on a bad binary");
        assert!(
            !worktree_path.exists(),
            "worktree must be removed when launch fails, not leaked"
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

        session.shutdown().await.expect("shutdown");
    }
}
