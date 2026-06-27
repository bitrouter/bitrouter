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
//! - `session/update` notifications → [`translate`] → a `tokio` broadcast of
//!   [`SessionUpdateKind`], exposed as a `Stream` by [`UpstreamConnection::subscribe_updates`].
//! - upstream `session/request_permission` requests → a [`PendingPermission`]
//!   (summary + diff + resolver) pushed onto a `futures` mpsc, exposed by
//!   [`UpstreamConnection::subscribe_permissions`].
//!
//! ## Deadlock avoidance & lock discipline
//!
//! The command loop never blocks on a prompt turn to completion: each prompt is
//! driven inside `connection.spawn(...)` so the loop returns to selecting on the
//! command channel immediately (mirrors `feed.rs`). The permission handler does
//! its parked wait + respond inside `connection.spawn(...)` too, never in the
//! dispatch callback. The `std::sync::Mutex` guarding the pending-permission map
//! is only ever held for the synchronous insert/remove — never across an
//! `.await`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, McpServer, McpServerStdio,
    NewSessionRequest, PromptRequest, PromptResponse, RequestPermissionRequest,
    RequestPermissionResponse, SessionId, SessionNotification, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo, Responder};
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt, Stream, StreamExt};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::translate::{
    PermissionOutcome, SessionUpdateKind, render_diff, select_option, translate,
};

/// Capacity of the broadcast channel that fans `session/update`-derived
/// [`SessionUpdateKind`]s out to subscribers. Sized to absorb a streaming burst
/// without dropping; a subscriber that lags past this sees the broadcast's
/// `Lagged` skip, which [`UpstreamConnection::subscribe_updates`] filters out.
const UPDATE_CHANNEL_CAPACITY: usize = 1024;

/// Shared registry of in-flight permission requests, keyed by the id we mint.
/// The sender resolves the parked permission handler with the caller's outcome.
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<PermissionOutcome>>>>;

/// A permission request awaiting a decision. Carries enough context to render a
/// prompt plus a one-shot [`resolve`](PendingPermission::resolve) to answer it.
#[derive(Debug)]
pub struct PendingPermission {
    /// Id we minted for this request; stable for the life of the request.
    pub request_id: String,
    /// Human-readable summary (the tool call's title, when present).
    pub summary: String,
    /// Rendered diff for the tool call, if it carries one.
    pub diff: Option<String>,
    resolver: oneshot::Sender<PermissionOutcome>,
}

impl PendingPermission {
    /// Answer this permission request. Consumes the pending item; a dropped
    /// [`PendingPermission`] defaults to [`PermissionOutcome::Deny`] upstream.
    pub fn resolve(self, outcome: PermissionOutcome) {
        let _ = self.resolver.send(outcome);
    }
}

/// One command driven inside the connection's command loop.
enum Command {
    /// Drive a prompt turn; reply with the typed [`PromptResponse`].
    Prompt {
        req: Box<PromptRequest>,
        reply: oneshot::Sender<anyhow::Result<PromptResponse>>,
    },
    /// Send a `session/cancel` notification for `session_id`.
    Cancel { session_id: String },
    /// A caller-side resolution for a pending permission request.
    ResolvePermission {
        request_id: String,
        outcome: PermissionOutcome,
    },
}

/// What the handshake reports back to [`UpstreamConnection::spawn`] before the
/// command loop takes over.
struct Handshake {
    acp_session_id: String,
    agent_session_id: Option<String>,
}

/// A live upstream ACP `Client` connection to one agent process.
pub struct UpstreamConnection {
    acp_session_id: String,
    agent_session_id: Option<String>,
    /// Submits [`Command`]s into the connection's command loop.
    cmd_tx: mpsc::UnboundedSender<Command>,
    /// Source of [`SessionUpdateKind`]s; cloned per `subscribe_updates`.
    updates_tx: broadcast::Sender<SessionUpdateKind>,
    /// Single permissions receiver, handed out once by `subscribe_permissions`.
    permissions_rx: Mutex<Option<mpsc::UnboundedReceiver<PendingPermission>>>,
    /// Keeps the driver thread alive for the connection's lifetime.
    _thread: std::thread::JoinHandle<()>,
}

impl UpstreamConnection {
    /// Spawn the agent process, connect as an ACP `Client`, and run
    /// `initialize` + `session/new`. Returns once the handshake completes and
    /// the command loop is resident, or an error if spawn/handshake failed.
    pub async fn spawn(
        command: &str,
        args: &[String],
        cwd: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        // `AcpAgent::spawn_process` does not set the child's working directory;
        // ACP carries the session cwd at the protocol level via `NewSessionRequest`.
        let name = std::path::Path::new(command)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("agent")
            .to_string();
        let server = McpServerStdio::new(name, command).args(args.to_vec());
        let agent = AcpAgent::new(McpServer::Stdio(server));
        let session_cwd =
            cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

        let (cmd_tx, cmd_rx) = mpsc::unbounded::<Command>();
        let (updates_tx, _) = broadcast::channel::<SessionUpdateKind>(UPDATE_CHANNEL_CAPACITY);
        let (perm_tx, perm_rx) = mpsc::unbounded::<PendingPermission>();
        let (handshake_tx, handshake_rx) = oneshot::channel::<anyhow::Result<Handshake>>();

        let updates_for_thread = updates_tx.clone();
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
                    agent,
                    session_cwd,
                    cmd_rx,
                    updates_for_thread,
                    perm_tx,
                    handshake_tx,
                ));
            })?;

        let handshake = handshake_rx
            .await
            .map_err(|_| anyhow::anyhow!("upstream driver thread exited before handshake"))??;

        Ok(Self {
            acp_session_id: handshake.acp_session_id,
            agent_session_id: handshake.agent_session_id,
            cmd_tx,
            updates_tx,
            permissions_rx: Mutex::new(Some(perm_rx)),
            _thread: thread,
        })
    }

    /// The ACP wire session id returned by `session/new`.
    pub fn acp_session_id(&self) -> &str {
        &self.acp_session_id
    }

    /// The provider-native session id from the `session/new` response
    /// `_meta.agentSessionId`, when the upstream exposes one. Never synthesized.
    pub fn agent_session_id(&self) -> Option<&str> {
        self.agent_session_id.as_deref()
    }

    /// Subscribe to the stream of translated `session/update` notifications.
    /// Each call yields an independent stream from the current point onward.
    pub fn subscribe_updates(
        &self,
    ) -> std::pin::Pin<Box<dyn Stream<Item = SessionUpdateKind> + Send>> {
        // Drop `Lagged` markers: a slow subscriber skips ahead rather than
        // seeing an error item in the stream.
        Box::pin(
            BroadcastStream::new(self.updates_tx.subscribe()).filter_map(|r| async move { r.ok() }),
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

    /// Resolve a pending permission request by id with the caller's outcome.
    /// Used when a consumer answers via the connection rather than holding the
    /// [`PendingPermission`] directly.
    pub fn resolve_permission(&self, request_id: &str, outcome: PermissionOutcome) {
        let _ = self.cmd_tx.unbounded_send(Command::ResolvePermission {
            request_id: request_id.to_string(),
            outcome,
        });
    }
}

/// Build the ACP client, perform the handshake (reporting it back over
/// `handshake_tx`), then run the command loop until the command channel closes.
async fn drive(
    agent: AcpAgent,
    session_cwd: PathBuf,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    updates_tx: broadcast::Sender<SessionUpdateKind>,
    perm_tx: mpsc::UnboundedSender<PendingPermission>,
    handshake_tx: oneshot::Sender<anyhow::Result<Handshake>>,
) {
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

    let notif_updates = updates_tx.clone();
    let handler_perm_tx = perm_tx.clone();
    let handler_pending = pending.clone();

    // The handshake oneshot is consumed exactly once. The `connect_with`
    // closure reports `Ok` on success then enters the command loop; if the
    // connection ends before the closure took it, the post-await arm reports the
    // error so `spawn()` never hangs on the oneshot. Shared so both arms can
    // take it.
    let handshake_tx: Arc<Mutex<Option<oneshot::Sender<anyhow::Result<Handshake>>>>> =
        Arc::new(Mutex::new(Some(handshake_tx)));
    let closure_handshake_tx = handshake_tx.clone();

    let result = agent_client_protocol::Client
        .builder()
        .name("bitrouter-substrate")
        .on_receive_notification(
            move |notification: SessionNotification, _cx| {
                let notif_updates = notif_updates.clone();
                async move {
                    if let Some(update) = translate(notification.update) {
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
                let pending = handler_pending.clone();
                async move {
                    let request_id = uuid::Uuid::new_v4().to_string();
                    let summary = request
                        .tool_call
                        .fields
                        .title
                        .clone()
                        .unwrap_or_else(|| "permission requested".to_string());
                    let diff = request
                        .tool_call
                        .fields
                        .content
                        .as_deref()
                        .and_then(render_diff);

                    // Register the resolver BEFORE emitting the item so a fast
                    // resolve can never race ahead of the insert.
                    let (tx, rx) = oneshot::channel::<PermissionOutcome>();
                    {
                        // Lock held only for the insert — never across an await.
                        let mut guard = match pending.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        guard.insert(request_id.clone(), tx);
                    }

                    let (item_tx, item_rx) = oneshot::channel::<PermissionOutcome>();
                    let pending_item = PendingPermission {
                        request_id: request_id.clone(),
                        summary,
                        diff,
                        resolver: item_tx,
                    };
                    // If no one is listening on the permissions stream the item
                    // is dropped and `item_rx` resolves to `Cancelled`/`Deny`.
                    let _ = perm_tx.unbounded_send(pending_item);

                    // Park the wait + respond OUTSIDE the dispatch loop so other
                    // messages keep flowing while the decision is pending.
                    let options = request.options.clone();
                    let cleanup_id = request_id.clone();
                    let cleanup_pending = pending.clone();
                    if let Err(e) = connection.spawn(async move {
                        // Resolve from EITHER the held `PendingPermission`
                        // (`item_rx`) OR the by-id channel (`rx`). Default to
                        // Deny if both senders drop.
                        let outcome = futures::select! {
                            o = item_rx.fuse() => o.unwrap_or(PermissionOutcome::Deny),
                            o = rx.fuse() => o.unwrap_or(PermissionOutcome::Deny),
                        };
                        {
                            let mut guard = match cleanup_pending.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            guard.remove(&cleanup_id);
                        }
                        let outcome = select_option(outcome, &options);
                        responder.respond(RequestPermissionResponse::new(outcome))
                    }) {
                        // Spawn failed: drop the resolver we inserted so the map
                        // does not leak an entry that can never be fulfilled.
                        let mut guard = match pending.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        guard.remove(&request_id);
                        return Err(e);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
            // ── Handshake ──────────────────────────────────────────────────
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            let new_session = connection
                .send_request(NewSessionRequest::new(session_cwd))
                .block_task()
                .await?;
            let acp_session_id = new_session.session_id.0.to_string();
            // `_meta.agentSessionId`, when the upstream exposes one. Never synthesized.
            let agent_session_id = new_session
                .meta
                .as_ref()
                .and_then(|m| m.get("agentSessionId"))
                .and_then(|v| v.as_str())
                .map(str::to_string);

            let report = closure_handshake_tx
                .lock()
                .ok()
                .and_then(|mut guard| guard.take());
            if let Some(tx) = report {
                let _ = tx.send(Ok(Handshake {
                    acp_session_id: acp_session_id.clone(),
                    agent_session_id,
                }));
            }

            // ── Command loop ───────────────────────────────────────────────
            // Never blocks on a prompt turn: each prompt runs in its own task so
            // the loop stays responsive to `ResolvePermission` mid-turn.
            while let Some(cmd) = cmd_rx.next().await {
                match cmd {
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
                    Command::ResolvePermission {
                        request_id,
                        outcome,
                    } => {
                        let sender = {
                            let mut guard = match pending.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            guard.remove(&request_id)
                        };
                        if let Some(sender) = sender {
                            let _ = sender.send(outcome);
                        }
                    }
                }
            }

            Ok(())
        })
        .await;

    // If the handshake never completed (connect/initialize/session-new failed),
    // surface the error to `spawn()` so it doesn't hang on the oneshot.
    let report = handshake_tx.lock().ok().and_then(|mut guard| guard.take());
    if let Some(tx) = report {
        let err = match result {
            Ok(()) => anyhow::anyhow!("upstream connection ended before handshake"),
            Err(e) => anyhow::anyhow!("upstream connection failed: {e}"),
        };
        let _ = tx.send(Err(err));
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
        let conn = UpstreamConnection::spawn("bash", &["-c".into(), script.into()], None)
            .await
            .expect("spawn");
        let usid = conn.acp_session_id().to_string();
        assert_eq!(usid, "u1");
        let mut updates = conn.subscribe_updates();
        let resp = conn.prompt(&usid, "do X").await.expect("prompt");
        assert!(format!("{resp:?}").contains("EndTurn"));
        let ev = updates.next().await.expect("update");
        assert!(format!("{ev:?}").contains("hi"), "unexpected: {ev:?}");
    }
}
