//! Upstream path — spawns the chosen agent process and speaks the ACP `Client`
//! role to it (we are the client; the agent is the upstream).
//!
//! [`UpstreamConnection`] owns a dedicated thread running a multi-thread tokio
//! runtime (the `feed.rs` / `ai.rs` pattern). That thread drives one ACP
//! connection via [`agent_client_protocol::Client::connect_with`]: the
//! connection's background actors run only while the `main_fn` closure is alive,
//! so `main_fn` runs a **command loop** that stays resident for the connection's
//! lifetime. Callers on other runtimes reach that loop through a `futures` mpsc
//! of [`Command`]s — `prompt_typed`/`cancel` enqueue a command carrying a
//! oneshot reply channel; the loop drives the request and answers the oneshot.
//!
//! ## Callback plane
//!
//! - `session/update` notifications fan out on two `tokio` broadcasts from the
//!   same handler: the **translated** [`SessionUpdateKind`] stream (for the GUI /
//!   telemetry consumers), exposed by [`UpstreamConnection::subscribe_updates`],
//!   and the **raw** ACP [`SessionUpdate`] stream, exposed by
//!   [`UpstreamConnection::subscribe_raw_updates`]. The raw stream exists so the
//!   down-facing `SessionAgent` can forward each upstream update to its manager
//!   verbatim, with no lossy reverse-mapping.
//! - upstream `session/request_permission` requests → a [`PendingPermission`]
//!   (raw tool-call + options + resolver) pushed onto a `futures` mpsc, exposed by
//!   [`UpstreamConnection::subscribe_permissions`].
//!
//! ## Deadlock avoidance
//!
//! The command loop never blocks on a prompt turn to completion: each prompt is
//! driven inside `connection.spawn(...)` so the loop returns to selecting on the
//! command channel immediately (mirrors `feed.rs`). The permission handler does
//! its parked wait + respond inside `connection.spawn(...)` too, never in the
//! dispatch callback, so it never blocks the SDK's message-dispatch loop.
//!
//! ## Single permission resolver
//!
//! Each `session/request_permission` has exactly **one** resolver: the oneshot
//! sender carried by the emitted [`PendingPermission`]. The resolver carries the
//! **exact** [`RequestPermissionOutcome`] (the chosen `optionId`, validated
//! against the offered set by [`sanitize_selection`]) — never a coarse
//! allow/deny that would collapse same-kind options. If the sender is dropped
//! (i.e. the consumer dropped the `PendingPermission` without resolving), the
//! parked handler task defaults to the reject option
//! ([`select_option`]`(`[`PermissionOutcome::Deny`]`)`), so the upstream never
//! hangs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use std::collections::HashMap;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, InitializeResponse, McpServer,
    NewSessionRequest, PermissionOption, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionNotification,
    SessionUpdate, TextContent, ToolCallUpdate,
};
use agent_client_protocol::{Agent, ByteStreams, ConnectionTo, Responder};
use futures::channel::{mpsc, oneshot};
use futures::{Stream, StreamExt};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::telemetry::{ContextUsage, SharedContextUsage};
use crate::translate::{
    PermissionOutcome, SessionUpdateKind, sanitize_selection, select_option, translate,
};

/// Capacity of the broadcast channel that fans `session/update`-derived
/// [`SessionUpdateKind`]s out to subscribers. Sized to absorb a streaming burst
/// without dropping; a subscriber that lags past this sees the broadcast's
/// `Lagged` skip, which [`UpstreamConnection::subscribe_updates`] filters out.
const UPDATE_CHANNEL_CAPACITY: usize = 1024;

/// A permission request awaiting a decision. Carries the raw tool-call payload
/// and permission options plus a one-shot [`resolve`](PendingPermission::resolve) to answer it.
///
/// There is exactly **one** resolver per request — the one carried here. A
/// consumer that cannot answer should simply **drop** the `PendingPermission`:
/// dropping the resolver makes the parked upstream handler respond with the
/// reject option ([`PermissionOutcome::Deny`] mapped via
/// [`select_option`](crate::translate::select_option)), so the upstream never
/// hangs.
///
/// Unresolved permissions are otherwise reaped only when the connection tears
/// down — ACP v1 has no per-turn-cancel cleanup for in-flight permission
/// requests — so a consumer must always resolve or drop each `PendingPermission`.
#[derive(Debug)]
pub struct PendingPermission {
    /// Id we minted for this request; stable for the life of the request.
    pub request_id: String,
    /// The verbatim tool-call payload from the upstream `request_permission`.
    /// The down-facing `SessionAgent` re-issues it to its manager unchanged.
    pub tool_call: ToolCallUpdate,
    /// The verbatim permission options from the upstream `request_permission`.
    /// Carried so a consumer that re-issues the request (the down-facing agent)
    /// forwards the same options and resolves with the exact selection.
    pub options: Vec<PermissionOption>,
    resolver: oneshot::Sender<RequestPermissionOutcome>,
}

impl PendingPermission {
    /// Answer this permission request with the **exact** outcome — the chosen
    /// `optionId` (or `Cancelled`) as selected by the consumer. The parked
    /// upstream handler validates the id against the offered options
    /// ([`sanitize_selection`](crate::translate::sanitize_selection)) before
    /// responding. Consumes the pending item; if the `PendingPermission` is
    /// instead **dropped** without calling this, the upstream handler defaults
    /// the response to the reject option.
    pub fn resolve(self, outcome: RequestPermissionOutcome) {
        let _ = self.resolver.send(outcome);
    }
}

/// The wire identity minted by the upstream's `session/new`.
#[derive(Debug, Clone)]
pub struct UpstreamSessionIds {
    /// The ACP wire session id.
    pub acp_session_id: String,
    /// The provider-native id from `_meta.agentSessionId`, when the upstream
    /// exposes one. Never synthesized.
    pub agent_session_id: Option<String>,
}

/// One command driven inside the connection's command loop.
enum Command {
    /// Create the upstream session (`session/new`) with the given working
    /// directory and MCP servers (the manager's, relayed verbatim); reply
    /// with the minted wire identity.
    NewSession {
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
        reply: oneshot::Sender<anyhow::Result<UpstreamSessionIds>>,
    },
    /// Drive a prompt turn; reply with the typed [`PromptResponse`].
    Prompt {
        req: Box<PromptRequest>,
        reply: oneshot::Sender<anyhow::Result<PromptResponse>>,
    },
    /// Send a `session/cancel` notification for `session_id`.
    Cancel { session_id: String },
    /// Exit the command loop, tearing the connection down (which kills the
    /// agent child). `done` fires once teardown has completed.
    Shutdown { done: oneshot::Sender<()> },
}

/// How long [`UpstreamConnection::shutdown`] waits for the driver to confirm
/// teardown before reporting failure. Killing the child is a synchronous
/// signal, so this is generous.
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What the handshake reports back to [`UpstreamConnection::spawn`] before the
/// command loop takes over.
struct Handshake {
    /// The upstream agent's `initialize` response — capabilities, agent info,
    /// auth methods. Kept so the down-facing endpoint can reflect the real
    /// agent's capabilities to its manager instead of fabricating minimal ones.
    init: Box<InitializeResponse>,
}

/// A live upstream ACP `Client` connection to one agent process.
///
/// `spawn` runs `initialize` only; the session itself is created later via
/// [`new_session`](Self::new_session) so the caller (the engine / down-facing
/// endpoint) can relay the **manager's** `cwd` and `mcpServers` into the
/// upstream `session/new` instead of fabricating them at launch.
pub struct UpstreamConnection {
    /// The upstream agent's `initialize` response, captured at handshake.
    init: Box<InitializeResponse>,
    /// Submits [`Command`]s into the connection's command loop.
    cmd_tx: mpsc::UnboundedSender<Command>,
    /// Source of [`SessionUpdateKind`]s; cloned per `subscribe_updates`.
    updates_tx: broadcast::Sender<SessionUpdateKind>,
    /// Source of raw ACP [`SessionUpdate`]s; cloned per `subscribe_raw_updates`.
    /// Fed from the same `session/update` handler as `updates_tx`, so the
    /// down-facing agent can forward updates to its manager verbatim.
    raw_updates_tx: broadcast::Sender<SessionUpdate>,
    /// Single permissions receiver, handed out once by `subscribe_permissions`.
    permissions_rx: Mutex<Option<mpsc::UnboundedReceiver<PendingPermission>>>,
    /// Latest context-window usage from upstream `UsageUpdate`s; written by the
    /// `session/update` handler, snapshotted by the telemetry hook.
    usage: SharedContextUsage,
    /// Single non-lossy raw-update feed for the transcript writer, handed out
    /// once by `take_transcript_feed`. Unbounded, unlike the broadcasts.
    transcript_rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>>>,
    /// Keeps the driver thread alive for the connection's lifetime.
    _thread: std::thread::JoinHandle<()>,
}

/// The ACP-over-stdio transport wired to a spawned agent child.
type AgentTransport =
    ByteStreams<Compat<tokio::process::ChildStdin>, Compat<tokio::process::ChildStdout>>;

/// Spawn `command args` with `env` applied, wired for ACP over stdio.
///
/// The child is made **its own process-group leader** (unix) so teardown can
/// kill the whole tree: agents are commonly wrapper chains (`npx → node`,
/// `uvx → python`), and killing only the immediate child orphans the real
/// agent — the process re-parents to pid 1 and does not reliably exit on
/// stdin EOF. Must run inside a tokio runtime (both call sites do). Shared by
/// [`UpstreamConnection::spawn`] and [`health_check`] so both paths spawn
/// identically. Stderr is inherited: agent logs land on our stderr alongside
/// the substrate's own.
fn spawn_agent_process(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> anyhow::Result<(AgentTransport, tokio::process::Child)> {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args)
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        // Belt-and-braces: the reaper below is the real teardown; this covers
        // the child handle being dropped without it ever running.
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawning agent '{command}': {e}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("agent child has no stdin pipe"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("agent child has no stdout pipe"))?;
    Ok((
        ByteStreams::new(stdin.compat_write(), stdout.compat()),
        child,
    ))
}

/// SIGKILL the child's whole process group (it is its own group leader via
/// `process_group(0)`). `ESRCH` just means everyone is already gone. On
/// non-unix targets group semantics don't apply; `kill_on_drop` covers the
/// direct child there.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // SAFETY: plain syscall on a pid we spawned; failure is ignored by design.
    unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Own the agent child for its whole life. Two exits:
///
/// - the child dies on its own (agent crash/EOF) → SIGKILL its process group
///   anyway (a dead wrapper can leave the real agent running), then signal
///   `dead_tx` so the command loop ends;
/// - a kill order arrives — `kill_rx` firing **or its sender dropping**
///   (teardown, caller panic, `health_check` timeout: all collapse to the
///   same cleanup) → group-kill, then reap the direct child.
///
/// Either way `done_tx` confirms once the group is killed and the child
/// reaped, so teardown can wait for it before dropping the runtime.
fn spawn_child_reaper(
    mut child: tokio::process::Child,
    kill_rx: oneshot::Receiver<()>,
    dead_tx: oneshot::Sender<()>,
    done_tx: oneshot::Sender<()>,
) {
    tokio::spawn(async move {
        let pid = child.id();
        tokio::select! {
            _ = child.wait() => {
                if let Some(pid) = pid {
                    kill_process_group(pid);
                }
            }
            // Resolves on an explicit send AND on sender drop.
            _ = kill_rx => {
                if let Some(pid) = pid {
                    kill_process_group(pid);
                }
                let _ = child.wait().await;
            }
        }
        let _ = dead_tx.send(());
        let _ = done_tx.send(());
    });
}

impl UpstreamConnection {
    /// Spawn the agent process, connect as an ACP `Client`, and run
    /// `initialize`. Returns once the handshake completes and the command
    /// loop is resident, or an error if spawn/handshake failed. The session
    /// itself is created afterwards via [`new_session`](Self::new_session)
    /// (which carries the cwd — the child's working directory is not set;
    /// ACP carries it at the protocol level).
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        let command = command.to_string();
        let args = args.to_vec();
        let env = env.clone();

        let (cmd_tx, cmd_rx) = mpsc::unbounded::<Command>();
        let (updates_tx, _) = broadcast::channel::<SessionUpdateKind>(UPDATE_CHANNEL_CAPACITY);
        let (raw_updates_tx, _) = broadcast::channel::<SessionUpdate>(UPDATE_CHANNEL_CAPACITY);
        let (perm_tx, perm_rx) = mpsc::unbounded::<PendingPermission>();
        let (handshake_tx, handshake_rx) = oneshot::channel::<anyhow::Result<Handshake>>();
        let usage: SharedContextUsage = Arc::new(Mutex::new(None));
        let (transcript_tx, transcript_rx) =
            tokio::sync::mpsc::unbounded_channel::<SessionUpdate>();

        let updates_for_thread = updates_tx.clone();
        let raw_updates_for_thread = raw_updates_tx.clone();
        let usage_for_thread = usage.clone();
        let thread = std::thread::Builder::new()
            .name("bitrouter-substrate-up".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = handshake_tx
                            .send(Err(anyhow::anyhow!("failed to start ACP runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(drive(
                    command,
                    args,
                    env,
                    cmd_rx,
                    CallbackPlane {
                        updates_tx: updates_for_thread,
                        raw_updates_tx: raw_updates_for_thread,
                        usage: usage_for_thread,
                        transcript_tx,
                        perm_tx,
                    },
                    handshake_tx,
                ));
            })?;

        let handshake = handshake_rx
            .await
            .map_err(|_| anyhow::anyhow!("upstream driver thread exited before handshake"))??;

        Ok(Self {
            init: handshake.init,
            cmd_tx,
            updates_tx,
            raw_updates_tx,
            permissions_rx: Mutex::new(Some(perm_rx)),
            usage,
            transcript_rx: Mutex::new(Some(transcript_rx)),
            _thread: thread,
        })
    }

    /// Create the upstream session: `session/new` with `cwd` and the given
    /// MCP servers (the manager's, relayed verbatim). Returns the minted wire
    /// identity. The caller decides when this happens — the engine's
    /// immediate-open launch calls it right away; the down-facing endpoint
    /// calls it when its manager sends `session/new`.
    pub async fn new_session(
        &self,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> anyhow::Result<UpstreamSessionIds> {
        let (reply, reply_rx) = oneshot::channel();
        self.cmd_tx
            .unbounded_send(Command::NewSession {
                cwd,
                mcp_servers,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("upstream command loop closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("upstream dropped the session/new reply"))?
    }

    /// The upstream agent's `initialize` response, captured at handshake. The
    /// down-facing endpoint reflects these capabilities (masked for what the
    /// substrate itself cannot honor) to its manager.
    pub fn upstream_init(&self) -> &InitializeResponse {
        &self.init
    }

    /// Take the **non-lossy** feed of raw upstream `session/update`s for the
    /// transcript writer. Unbounded — every update arrives in order, unlike
    /// the lossy UI broadcasts. Single-consumer: the first call returns the
    /// receiver, later calls return `None`.
    pub fn take_transcript_feed(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>> {
        self.transcript_rx.lock().ok().and_then(|mut g| g.take())
    }

    /// Handle to the latest context-window usage reported by the upstream
    /// (`session/update UsageUpdate`); `None` until the upstream reports one.
    /// The telemetry hook snapshots this into each [`RequestCompleted`]
    /// record.
    ///
    /// [`RequestCompleted`]: crate::telemetry::RequestCompleted
    pub fn context_usage(&self) -> SharedContextUsage {
        self.usage.clone()
    }

    /// Subscribe to the stream of translated `session/update` notifications.
    /// Each call yields an independent stream from the current point onward.
    ///
    /// **Lossy under lag.** Updates ride a bounded `tokio` broadcast: a
    /// subscriber that falls more than [`UPDATE_CHANNEL_CAPACITY`] messages
    /// behind silently skips the dropped chunks (the broadcast's `Lagged` marker
    /// is filtered out, not surfaced as an error). A consumer that needs a
    /// complete transcript must subscribe immediately after
    /// [`spawn`](Self::spawn) and keep up with the stream.
    pub fn subscribe_updates(
        &self,
    ) -> std::pin::Pin<Box<dyn Stream<Item = SessionUpdateKind> + Send>> {
        // Drop `Lagged` markers: a slow subscriber skips ahead rather than
        // seeing an error item in the stream.
        Box::pin(
            BroadcastStream::new(self.updates_tx.subscribe()).filter_map(|r| async move { r.ok() }),
        )
    }

    /// Subscribe to the stream of **raw** ACP `session/update` notifications,
    /// untranslated. Each call yields an independent stream from the current
    /// point onward. The down-facing `SessionAgent` uses this to forward each
    /// upstream update to its manager verbatim (no lossy reverse-mapping).
    ///
    /// **Lossy under lag**, exactly like [`subscribe_updates`](Self::subscribe_updates):
    /// rides the same bounded [`UPDATE_CHANNEL_CAPACITY`] broadcast and silently
    /// skips ahead (filters the `Lagged` marker) for a subscriber that falls
    /// behind.
    pub fn subscribe_raw_updates(
        &self,
    ) -> std::pin::Pin<Box<dyn Stream<Item = SessionUpdate> + Send>> {
        Box::pin(
            BroadcastStream::new(self.raw_updates_tx.subscribe())
                .filter_map(|r| async move { r.ok() }),
        )
    }

    /// Take the stream of pending permission requests. Single-consumer: the
    /// first call returns the receiver; later calls return an empty stream.
    pub fn subscribe_permissions(
        &self,
    ) -> std::pin::Pin<Box<dyn Stream<Item = PendingPermission> + Send>> {
        let taken = self
            .permissions_rx
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        match taken {
            Some(rx) => Box::pin(rx),
            None => Box::pin(futures::stream::empty()),
        }
    }

    /// Send a typed `PromptRequest` and return the typed `PromptResponse`.
    /// Later tasks (the session executor) call this directly — zero round-trip.
    pub async fn prompt_typed(&self, req: PromptRequest) -> anyhow::Result<PromptResponse> {
        let (reply, reply_rx) = oneshot::channel();
        self.cmd_tx
            .unbounded_send(Command::Prompt {
                req: Box::new(req),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("upstream command loop closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("upstream dropped the prompt reply"))?
    }

    /// Text convenience over [`prompt_typed`](Self::prompt_typed).
    pub async fn prompt(&self, session_id: &str, text: &str) -> anyhow::Result<PromptResponse> {
        self.prompt_typed(PromptRequest::new(
            SessionId::new(session_id),
            vec![ContentBlock::Text(TextContent::new(text.to_string()))],
        ))
        .await
    }

    /// Send a `session/cancel` notification for `session_id`.
    pub async fn cancel(&self, session_id: &str) -> anyhow::Result<()> {
        self.cmd_tx
            .unbounded_send(Command::Cancel {
                session_id: session_id.to_string(),
            })
            .map_err(|_| anyhow::anyhow!("upstream command loop closed"))
    }

    /// Tear the connection down **deterministically**: the command loop exits,
    /// the connection (and its transport) drops — killing the agent child —
    /// and this returns once the driver confirms teardown. Idempotent: if the
    /// loop is already gone the connection is already down and this returns
    /// `Ok`. Errs only when the driver fails to confirm within
    /// [`SHUTDOWN_TIMEOUT`], in which case the child may still be alive.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let (done_tx, done_rx) = oneshot::channel::<()>();
        if self
            .cmd_tx
            .unbounded_send(Command::Shutdown { done: done_tx })
            .is_err()
        {
            // Command loop already ended — the connection is already down.
            return Ok(());
        }
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, done_rx).await {
            // Confirmed, or the driver ended before processing the command
            // (receiver dropped) — either way the connection is down.
            Ok(_) => Ok(()),
            Err(_) => Err(anyhow::anyhow!(
                "upstream teardown did not confirm within {SHUTDOWN_TIMEOUT:?}"
            )),
        }
    }
}

/// Build the ACP client, perform the handshake (reporting it back over
/// `handshake_tx`), then run the command loop until the command channel closes.
/// The callback-plane outputs `drive` fans upstream events onto: the two
/// update broadcasts, the latest-usage slot, and the permissions channel.
struct CallbackPlane {
    updates_tx: broadcast::Sender<SessionUpdateKind>,
    raw_updates_tx: broadcast::Sender<SessionUpdate>,
    usage: SharedContextUsage,
    /// Non-lossy raw-update feed for the transcript writer.
    transcript_tx: tokio::sync::mpsc::UnboundedSender<SessionUpdate>,
    perm_tx: mpsc::UnboundedSender<PendingPermission>,
}

async fn drive(
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    plane: CallbackPlane,
    handshake_tx: oneshot::Sender<anyhow::Result<Handshake>>,
) {
    // Spawn the agent child ourselves (own process group) and hand its stdio
    // to the SDK as a ByteStreams transport; the reaper owns the child.
    let (transport, child) = match spawn_agent_process(&command, &args, &env) {
        Ok(spawned) => spawned,
        Err(e) => {
            let _ = handshake_tx.send(Err(e));
            return;
        }
    };
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let (dead_tx, mut dead_rx) = oneshot::channel::<()>();
    let (done_tx, done_rx) = oneshot::channel::<()>();
    spawn_child_reaper(child, kill_rx, dead_tx, done_tx);

    let notif_updates = plane.updates_tx.clone();
    let notif_raw_updates = plane.raw_updates_tx.clone();
    let notif_usage = plane.usage.clone();
    let notif_transcript = plane.transcript_tx.clone();
    let handler_perm_tx = plane.perm_tx.clone();

    // The handshake oneshot is consumed exactly once. The `connect_with`
    // closure reports `Ok` on success then enters the command loop; if the
    // connection ends before the closure took it, the post-await arm reports the
    // error so `spawn()` never hangs on the oneshot. Shared so both arms can
    // take it.
    let handshake_tx: Arc<Mutex<Option<oneshot::Sender<anyhow::Result<Handshake>>>>> =
        Arc::new(Mutex::new(Some(handshake_tx)));
    let closure_handshake_tx = handshake_tx.clone();

    // Confirmation for an explicit `Command::Shutdown`: the command loop stashes
    // the sender here and breaks; it fires AFTER `connect_with` returns (the
    // connection and its transport dropped, the agent child killed).
    let shutdown_done: Arc<Mutex<Option<oneshot::Sender<()>>>> = Arc::new(Mutex::new(None));
    let closure_shutdown_done = shutdown_done.clone();

    let connect = agent_client_protocol::Client
        .builder()
        .name("bitrouter-substrate")
        .on_receive_notification(
            move |notification: SessionNotification, _cx| {
                let notif_updates = notif_updates.clone();
                let notif_raw_updates = notif_raw_updates.clone();
                let notif_usage = notif_usage.clone();
                let notif_transcript = notif_transcript.clone();
                async move {
                    let raw = notification.update;
                    // Forward the raw ACP update verbatim (down-facing agent), and
                    // — when it maps to one — the translated kind (GUI/telemetry).
                    // A `send` error just means no subscriber is attached yet.
                    // The transcript feed is non-lossy; a send error there just
                    // means no transcript writer was attached (disabled).
                    let _ = notif_transcript.send(raw.clone());
                    let _ = notif_raw_updates.send(raw.clone());
                    if let Some(update) = translate(raw) {
                        // Keep the latest context usage snapshot current for the
                        // telemetry hook.
                        if let SessionUpdateKind::Usage { used, size, .. } = &update
                            && let Ok(mut slot) = notif_usage.lock()
                        {
                            *slot = Some(ContextUsage {
                                used: *used,
                                size: *size,
                            });
                        }
                        let _ = notif_updates.send(update);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            move |request: RequestPermissionRequest,
                  responder: Responder<RequestPermissionResponse>,
                  connection: ConnectionTo<Agent>| {
                let perm_tx = handler_perm_tx.clone();
                async move {
                    let request_id = uuid::Uuid::new_v4().to_string();

                    // Exactly one resolver per request: the oneshot sender carried
                    // by the emitted `PendingPermission`. The parked task below
                    // awaits its receiver; if the consumer drops the
                    // `PendingPermission` without resolving, the sender drops and
                    // the receiver yields `Err`, which defaults to the reject
                    // option.
                    let (item_tx, item_rx) = oneshot::channel::<RequestPermissionOutcome>();
                    // `options` is needed both by the emitted item (so a consumer
                    // can re-issue the request with the same options) and by the
                    // parked task below (to validate the chosen id / pick the
                    // reject default). Clone once for the parked task; move the
                    // rest into the item.
                    let options = request.options.clone();
                    let pending_item = PendingPermission {
                        request_id,
                        tool_call: request.tool_call,
                        options: request.options,
                        resolver: item_tx,
                    };
                    // If no one is listening on the permissions stream the item
                    // is dropped immediately and `item_rx` resolves to `Deny`.
                    let _ = perm_tx.unbounded_send(pending_item);

                    // Park the wait + respond OUTSIDE the dispatch loop so other
                    // messages keep flowing while the decision is pending.
                    connection.spawn(async move {
                        // The consumer's exact selection passes through verbatim
                        // (validated against the offered set); a dropped resolver
                        // (the consumer dropped the `PendingPermission`) defaults
                        // to the reject option so the upstream never hangs.
                        let outcome = match item_rx.await {
                            Ok(selection) => sanitize_selection(selection, &options),
                            Err(_) => select_option(PermissionOutcome::Deny, &options),
                        };
                        responder.respond(RequestPermissionResponse::new(outcome))
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            // ── Handshake: initialize only ─────────────────────────────────
            // `session/new` is a command (below) so the caller can relay the
            // manager's cwd + mcpServers into it instead of fabricating them
            // here at spawn time. Client capabilities are deliberately left at
            // their defaults (no fs / no terminal): ACP v2 removes that client
            // surface, and a manager provides such tooling via the relayed MCP
            // servers instead.
            let init = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            let report = closure_handshake_tx
                .lock()
                .ok()
                .and_then(|mut guard| guard.take());
            if let Some(tx) = report {
                let _ = tx.send(Ok(Handshake {
                    init: Box::new(init),
                }));
            }

            // ── Command loop ───────────────────────────────────────────────
            // Never blocks on a prompt turn: each prompt runs in its own task so
            // the loop stays responsive while a turn (and its mid-turn permission
            // requests) is in flight. Ends when the command channel closes or an
            // explicit `Shutdown` arrives; agent death is handled one level up
            // (the whole connection future is raced against the reaper's death
            // signal), because a ByteStreams transport EOF does NOT fail
            // in-flight requests — a request racing a dying agent would park
            // forever inside this loop's awaits otherwise.
            while let Some(cmd) = cmd_rx.next().await {
                match cmd {
                    Command::NewSession {
                        cwd,
                        mcp_servers,
                        reply,
                    } => {
                        let session_connection = connection.clone();
                        connection.spawn(async move {
                            let mut req = NewSessionRequest::new(cwd);
                            req.mcp_servers = mcp_servers;
                            let result = session_connection
                                .send_request(req)
                                .block_task()
                                .await
                                .map(|resp| UpstreamSessionIds {
                                    acp_session_id: resp.session_id.0.to_string(),
                                    // `_meta.agentSessionId`, when the upstream
                                    // exposes one. Never synthesized.
                                    agent_session_id: resp
                                        .meta
                                        .as_ref()
                                        .and_then(|m| m.get("agentSessionId"))
                                        .and_then(|v| v.as_str())
                                        .map(str::to_string),
                                })
                                .map_err(anyhow::Error::from);
                            let _ = reply.send(result);
                            Ok(())
                        })?;
                    }
                    Command::Prompt { req, reply } => {
                        let turn_connection = connection.clone();
                        connection.spawn(async move {
                            let result = turn_connection
                                .send_request(*req)
                                .block_task()
                                .await
                                .map_err(anyhow::Error::from);
                            // Returning Err here would tear the whole connection
                            // down (SDK contract); deliver it over the reply
                            // oneshot instead.
                            let _ = reply.send(result);
                            Ok(())
                        })?;
                    }
                    Command::Cancel { session_id } => {
                        let _ = connection
                            .send_notification(CancelNotification::new(SessionId::new(session_id)));
                    }
                    Command::Shutdown { done } => {
                        // Stash the confirmation; it fires after `connect_with`
                        // returns (teardown complete), not here.
                        if let Ok(mut guard) = closure_shutdown_done.lock() {
                            *guard = Some(done);
                        }
                        break;
                    }
                }
            }

            Ok(())
        });

    // Race the connection against agent death. The SDK's ByteStreams
    // transport does not fail pending requests on EOF, so a dead child would
    // otherwise leave the handshake / prompts parked forever; dropping the
    // connection future cancels everything (dispatch, spawned request tasks,
    // the command loop), which drops every pending reply oneshot — callers
    // get errors instead of hangs. This mirrors the child-monitor race the
    // SDK's own AcpAgent transport performs.
    tokio::pin!(connect);
    let result = tokio::select! {
        result = &mut connect => result,
        _ = &mut dead_rx => {
            // The pinned connection future is simply never polled again; it
            // (and every task it owns) drops at the end of this function,
            // failing all pending reply oneshots.
            tracing::warn!("agent process exited; tearing the connection down");
            Ok(())
        }
    };

    // Teardown: order the reaper to SIGKILL the child's process group (a
    // no-op if the child already died — the reaper group-killed on that path
    // too) and wait for the reap to complete before the runtime drops.
    let _ = kill_tx.send(());
    if tokio::time::timeout(std::time::Duration::from_secs(2), done_rx)
        .await
        .is_err()
    {
        tracing::warn!("agent child reaper did not confirm within 2s");
    }

    // An explicit shutdown was requested and the connection is now fully torn
    // down (transport dropped, agent process group killed): confirm it.
    if let Some(tx) = shutdown_done.lock().ok().and_then(|mut guard| guard.take()) {
        let _ = tx.send(());
    }

    // If the handshake never completed (connect/initialize failed), surface
    // the error to `spawn()` so it doesn't hang on the oneshot.
    let report = handshake_tx.lock().ok().and_then(|mut guard| guard.take());
    if let Some(tx) = report {
        let err = match result {
            Ok(()) => anyhow::anyhow!("upstream connection ended before handshake"),
            Err(e) => anyhow::anyhow!("upstream connection failed: {e}"),
        };
        let _ = tx.send(Err(err));
    }
}

/// How long `health_check` waits for `initialize` before declaring the agent
/// unhealthy. Generous enough for a cold npm start; tight enough to keep
/// `bitrouter agents check` snappy when an agent hangs.
const HEALTH_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Spawn the agent, run ACP `initialize` only (no session), return elapsed on
/// success or an error string. Used by `bitrouter agents check`.
///
/// `env` is applied to the spawned child process (same plumbing as
/// [`UpstreamConnection::spawn`]) so an agent that needs API-key vars answers
/// the health-check.
///
/// Tears the connection down (drops) immediately after `initialize` succeeds
/// or after [`HEALTH_CHECK_TIMEOUT`] elapses.
pub async fn health_check(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<std::time::Duration, String> {
    tokio::time::timeout(HEALTH_CHECK_TIMEOUT, health_check_inner(command, args, env))
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "initialize timed out after {HEALTH_CHECK_TIMEOUT:?}"
            ))
        })
}

async fn health_check_inner(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<std::time::Duration, String> {
    let (transport, child) =
        spawn_agent_process(command, args, env).map_err(|e| format!("spawn failed: {e}"))?;
    // Reaper teardown: explicitly ordered (and awaited) below so the group is
    // gone before `agents check` moves on; if the caller's 10s timeout cancels
    // this future mid-await instead, `kill_tx` drops and the receiver resolves
    // all the same — the reaper group-kills + reaps on both paths.
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let (dead_tx, _dead_rx) = oneshot::channel::<()>();
    let (done_tx, done_rx) = oneshot::channel::<()>();
    spawn_child_reaper(child, kill_rx, dead_tx, done_tx);

    let (result_tx, result_rx) =
        futures::channel::oneshot::channel::<Result<std::time::Duration, String>>();
    let result_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(result_tx)));
    let closure_result_tx = result_tx.clone();

    let connect_result = agent_client_protocol::Client
        .builder()
        .name("bitrouter-health-check")
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            let started = std::time::Instant::now();
            let init_result = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await;
            let outcome = match init_result {
                Ok(_) => Ok(started.elapsed()),
                Err(e) => Err(format!("initialize failed: {e}")),
            };
            let tx = closure_result_tx
                .lock()
                .ok()
                .and_then(|mut guard| guard.take());
            if let Some(tx) = tx {
                let _ = tx.send(outcome);
            }
            // Return Ok so the connection closes cleanly (no command loop needed).
            Ok(())
        })
        .await;

    // The check is done either way: kill the agent's process group and wait
    // for the reap so no wrapper-chain grandchild outlives the CLI.
    let _ = kill_tx.send(());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), done_rx).await;

    // If the result was already sent via the closure, use it. Otherwise surface
    // the connect-level error (spawn failed, process exited before initialize, etc.).
    match result_rx.await {
        Ok(outcome) => outcome,
        Err(_) => {
            // Closure never ran or never sent — surface the connect error.
            match connect_result {
                Ok(()) => Err("agent exited before initialize".to_string()),
                Err(e) => Err(format!("connect failed: {e}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[cfg(unix)]
    #[tokio::test]
    async fn connects_initializes_and_prompts() {
        let script = r#"
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
        let conn =
            UpstreamConnection::spawn("bash", &["-c".into(), script.into()], &HashMap::new())
                .await
                .expect("spawn");
        let ids = conn
            .new_session(std::path::PathBuf::from("/"), vec![])
            .await
            .expect("session/new");
        let usid = ids.acp_session_id;
        assert_eq!(usid, "u1");
        let mut updates = conn.subscribe_updates();
        let resp = conn.prompt(&usid, "do X").await.expect("prompt");
        assert!(format!("{resp:?}").contains("EndTurn"));
        let ev = updates.next().await.expect("update");
        assert!(format!("{ev:?}").contains("hi"), "unexpected: {ev:?}");
    }

    /// Safety invariant: dropping a [`PendingPermission`] without resolving it
    /// must make the upstream handler answer `Deny`, so the agent never hangs.
    ///
    /// The stub sends a `session/request_permission` whose only option is a
    /// `reject_once` kind (id `rej`). The test subscribes to permissions,
    /// receives the [`PendingPermission`], and **drops** it. `select_option`
    /// maps the defaulted `Deny` onto the `reject_once` option, so the client's
    /// response selects `rej`. The stub reads that response line, echoes the
    /// chosen optionId back as a `session/update`, and completes the prompt; the
    /// test asserts the echoed id is `rej`.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_pending_permission_defaults_to_deny() {
        let script = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
                *session/prompt*)
                    # Ask for permission; the only option is a reject_once kind.
                    printf '{"jsonrpc":"2.0","id":"99","method":"session/request_permission","params":{"sessionId":"u1","toolCall":{"toolCallId":"tc1","title":"do thing"},"options":[{"optionId":"rej","name":"Reject","kind":"reject_once"}]}}\n'
                    # Read the client's permission response and echo its optionId.
                    read resp
                    chosen=$(echo "$resp" | sed -n 's/.*"optionId":"\([^"]*\)".*/\1/p')
                    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"chose:%s"}}}}\n' "$chosen"
                    printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
              esac
            done
        "#;
        let conn =
            UpstreamConnection::spawn("bash", &["-c".into(), script.into()], &HashMap::new())
                .await
                .expect("spawn");
        let usid = conn
            .new_session(std::path::PathBuf::from("/"), vec![])
            .await
            .expect("session/new")
            .acp_session_id;
        let mut updates = conn.subscribe_updates();
        let mut perms = conn.subscribe_permissions();

        // Drive the prompt concurrently; it completes only after the permission
        // round-trip finishes.
        let prompt = tokio::spawn(async move { conn.prompt(&usid, "do X").await });

        // Receive the pending permission and DROP it without resolving.
        let pending = perms.next().await.expect("permission request");
        assert_eq!(pending.tool_call.fields.title.as_deref(), Some("do thing"));
        drop(pending);

        // The echoed update proves the client answered with the reject option.
        let mut saw_reject = false;
        for _ in 0..4 {
            if let Some(ev) = updates.next().await
                && format!("{ev:?}").contains("chose:rej")
            {
                saw_reject = true;
                break;
            }
        }
        assert!(
            saw_reject,
            "dropped permission did not default to Deny/reject"
        );

        let resp = prompt.await.expect("join").expect("prompt");
        assert!(format!("{resp:?}").contains("EndTurn"));
    }

    /// An explicit `shutdown` confirms teardown promptly, after which the
    /// command loop is gone: further commands fail fast instead of hanging.
    /// A second `shutdown` is an idempotent no-op.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_confirms_teardown_and_closes_loop() {
        let script = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*) printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
              esac
            done
        "#;
        let conn =
            UpstreamConnection::spawn("bash", &["-c".into(), script.into()], &HashMap::new())
                .await
                .expect("spawn");

        conn.shutdown().await.expect("shutdown confirms");

        // The loop is gone: a prompt fails fast on the closed command channel.
        let err = conn.prompt("u1", "x").await;
        assert!(err.is_err(), "prompt after shutdown must fail, got Ok");

        // Idempotent.
        conn.shutdown().await.expect("second shutdown is a no-op");
    }

    /// The whole process GROUP dies at shutdown, wrapper chains included.
    /// The agent is spawned as `bash → bash <inner script>` (mimicking
    /// `npx → node`): the INNER process writes its pid to a file, and after
    /// `shutdown` that pid must be gone — killing only the outer wrapper
    /// (the old ChildGuard behavior) left it orphaned on pid 1.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_wrapper_chain_process_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("inner.pid");
        let inner = dir.path().join("inner.sh");
        std::fs::write(
            &inner,
            format!(
                r#"echo $$ > {pid}
while read line; do
  id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *initialize*) printf '{{"jsonrpc":"2.0","id":"%s","result":{{"protocolVersion":1}}}}\n' "$id";;
  esac
done
"#,
                pid = pid_file.display()
            ),
        )
        .expect("write inner script");

        // `; :` keeps the outer bash alive as a parent instead of exec-ing
        // the inner command (which would collapse the chain to one process).
        let outer = format!("bash {} ; :", inner.display());
        let conn = UpstreamConnection::spawn("bash", &["-c".into(), outer], &HashMap::new())
            .await
            .expect("spawn wrapper chain");

        // The inner (grand)child is alive and identified.
        let mut inner_pid = String::new();
        for _ in 0..100 {
            if let Ok(raw) = std::fs::read_to_string(&pid_file) {
                let raw = raw.trim().to_string();
                if !raw.is_empty() {
                    inner_pid = raw;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!inner_pid.is_empty(), "inner script never reported its pid");
        assert!(pid_alive(&inner_pid), "inner process should be alive");

        conn.shutdown().await.expect("shutdown");

        // The grandchild must die with the group, not linger orphaned.
        let mut gone = false;
        for _ in 0..100 {
            if !pid_alive(&inner_pid) {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            gone,
            "wrapper-chain grandchild (pid {inner_pid}) survived shutdown"
        );
    }

    /// `kill -0` liveness probe (same technique as `acp sessions`).
    #[cfg(unix)]
    fn pid_alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// An agent that dies mid-session must not wedge the connection: the whole
    /// connection future is raced against the reaper's death signal (a
    /// ByteStreams transport EOF does NOT fail in-flight requests on its own),
    /// so a pending prompt fails fast instead of hanging forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_crash_fails_pending_commands_fast() {
        // Answers the handshake, lingers briefly (so `spawn` completes), then
        // dies with the prompt in flight.
        let script = r#"
            read line
            id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
            printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id"
            sleep 0.3
            exit 0
        "#;
        let conn =
            UpstreamConnection::spawn("bash", &["-c".into(), script.into()], &HashMap::new())
                .await
                .expect("spawn");

        // The child dies with this prompt unanswered. It must resolve to an
        // error promptly (bounded), never hang.
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            conn.prompt("u1", "anyone home?"),
        )
        .await;
        match outcome {
            Ok(result) => assert!(result.is_err(), "prompt to a dead agent must fail"),
            Err(_) => panic!("prompt to a dead agent hung instead of failing fast"),
        }
    }

    /// health_check: a stub that answers `initialize` → returns Ok with an
    /// elapsed duration.
    #[cfg(unix)]
    #[tokio::test]
    async fn health_check_succeeds_when_agent_answers_initialize() {
        let script = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*) printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
              esac
            done
        "#;
        let result = health_check("bash", &["-c".into(), script.into()], &HashMap::new()).await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    /// health_check: a stub that returns a JSON-RPC error for `initialize` →
    /// `Err(_)`. Uses a bash script that replies with an error immediately so
    /// the test does not hit the timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn health_check_fails_when_agent_returns_error() {
        let script = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*) printf '{"jsonrpc":"2.0","id":"%s","error":{"code":-32600,"message":"not supported"}}\n' "$id";;
              esac
            done
        "#;
        let result = health_check("bash", &["-c".into(), script.into()], &HashMap::new()).await;
        assert!(result.is_err(), "expected Err, got: {result:?}");
    }

    /// health_check: env vars reach the spawned child. The stub answers
    /// `initialize` with success ONLY when `$HEALTHVAR` is set, otherwise it
    /// returns a JSON-RPC error. Run twice: once with the var (expect Ok),
    /// once with empty env (expect Err). Proves env plumbing end-to-end and
    /// gives mixed success/failure coverage.
    #[cfg(unix)]
    #[tokio::test]
    async fn health_check_passes_env_to_child() {
        let script = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)
                  if [ -n "$HEALTHVAR" ]; then
                    printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id"
                  else
                    printf '{"jsonrpc":"2.0","id":"%s","error":{"code":-32000,"message":"HEALTHVAR unset"}}\n' "$id"
                  fi;;
              esac
            done
        "#;
        let args = ["-c".to_string(), script.to_string()];

        let mut env = HashMap::new();
        env.insert("HEALTHVAR".to_string(), "1".to_string());
        let with_env = health_check("bash", &args, &env).await;
        assert!(
            with_env.is_ok(),
            "expected Ok with env set, got: {with_env:?}"
        );

        let without_env = health_check("bash", &args, &HashMap::new()).await;
        assert!(
            without_env.is_err(),
            "expected Err without env, got: {without_env:?}"
        );
    }
}
