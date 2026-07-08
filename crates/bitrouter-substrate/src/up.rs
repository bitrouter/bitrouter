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
    CancelNotification, ContentBlock, EnvVariable, InitializeRequest, InitializeResponse,
    McpServer, McpServerStdio, NewSessionRequest, PermissionOption, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SessionId,
    SessionNotification, SessionUpdate, TextContent, ToolCallUpdate,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo, Responder};
use futures::channel::{mpsc, oneshot};
use futures::{Stream, StreamExt};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

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

/// One command driven inside the connection's command loop.
enum Command {
    /// Drive a prompt turn; reply with the typed [`PromptResponse`].
    Prompt {
        req: Box<PromptRequest>,
        reply: oneshot::Sender<anyhow::Result<PromptResponse>>,
    },
    /// Send a `session/cancel` notification for `session_id`.
    Cancel { session_id: String },
}

/// What the handshake reports back to [`UpstreamConnection::spawn`] before the
/// command loop takes over.
struct Handshake {
    acp_session_id: String,
    agent_session_id: Option<String>,
    /// The upstream agent's `initialize` response — capabilities, agent info,
    /// auth methods. Kept so the down-facing endpoint can reflect the real
    /// agent's capabilities to its manager instead of fabricating minimal ones.
    init: Box<InitializeResponse>,
}

/// A live upstream ACP `Client` connection to one agent process.
pub struct UpstreamConnection {
    acp_session_id: String,
    agent_session_id: Option<String>,
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
    /// Keeps the driver thread alive for the connection's lifetime.
    _thread: std::thread::JoinHandle<()>,
}

/// Build an [`AcpAgent`] that spawns `command args` with `env` applied to the
/// child process. Shared by [`UpstreamConnection::spawn`] and [`health_check`]
/// so both spawn paths pass the configured agent environment identically —
/// `AcpAgent::spawn_process` forwards each [`EnvVariable`] to the child via
/// `Command::env`.
fn build_agent(command: &str, args: &[String], env: &HashMap<String, String>) -> AcpAgent {
    let name = std::path::Path::new(command)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("agent")
        .to_string();
    let env_vars: Vec<EnvVariable> = env
        .iter()
        .map(|(k, v)| EnvVariable::new(k.clone(), v.clone()))
        .collect();
    let server = McpServerStdio::new(name, command)
        .args(args.to_vec())
        .env(env_vars);
    AcpAgent::new(McpServer::Stdio(server))
}

impl UpstreamConnection {
    /// Spawn the agent process, connect as an ACP `Client`, and run
    /// `initialize` + `session/new`. Returns once the handshake completes and
    /// the command loop is resident, or an error if spawn/handshake failed.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        // `AcpAgent::spawn_process` does not set the child's working directory;
        // ACP carries the session cwd at the protocol level via `NewSessionRequest`.
        let agent = build_agent(command, args, env);
        let session_cwd =
            cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

        let (cmd_tx, cmd_rx) = mpsc::unbounded::<Command>();
        let (updates_tx, _) = broadcast::channel::<SessionUpdateKind>(UPDATE_CHANNEL_CAPACITY);
        let (raw_updates_tx, _) = broadcast::channel::<SessionUpdate>(UPDATE_CHANNEL_CAPACITY);
        let (perm_tx, perm_rx) = mpsc::unbounded::<PendingPermission>();
        let (handshake_tx, handshake_rx) = oneshot::channel::<anyhow::Result<Handshake>>();

        let updates_for_thread = updates_tx.clone();
        let raw_updates_for_thread = raw_updates_tx.clone();
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
                    raw_updates_for_thread,
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
            init: handshake.init,
            cmd_tx,
            updates_tx,
            raw_updates_tx,
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

    /// The upstream agent's `initialize` response, captured at handshake. The
    /// down-facing endpoint reflects these capabilities (masked for what the
    /// substrate itself cannot honor) to its manager.
    pub fn upstream_init(&self) -> &InitializeResponse {
        &self.init
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
}

/// Build the ACP client, perform the handshake (reporting it back over
/// `handshake_tx`), then run the command loop until the command channel closes.
async fn drive(
    agent: AcpAgent,
    session_cwd: PathBuf,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    updates_tx: broadcast::Sender<SessionUpdateKind>,
    raw_updates_tx: broadcast::Sender<SessionUpdate>,
    perm_tx: mpsc::UnboundedSender<PendingPermission>,
    handshake_tx: oneshot::Sender<anyhow::Result<Handshake>>,
) {
    let notif_updates = updates_tx.clone();
    let notif_raw_updates = raw_updates_tx.clone();
    let handler_perm_tx = perm_tx.clone();

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
                let notif_raw_updates = notif_raw_updates.clone();
                async move {
                    let raw = notification.update;
                    // Forward the raw ACP update verbatim (down-facing agent), and
                    // — when it maps to one — the translated kind (GUI/telemetry).
                    // A `send` error just means no subscriber is attached yet.
                    let _ = notif_raw_updates.send(raw.clone());
                    if let Some(update) = translate(raw) {
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
        .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
            // ── Handshake ──────────────────────────────────────────────────
            let init = connection
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
                    init: Box::new(init),
                }));
            }

            // ── Command loop ───────────────────────────────────────────────
            // Never blocks on a prompt turn: each prompt runs in its own task so
            // the loop stays responsive while a turn (and its mid-turn permission
            // requests) is in flight.
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
    let agent = build_agent(command, args, env);

    let (result_tx, result_rx) =
        futures::channel::oneshot::channel::<Result<std::time::Duration, String>>();
    let result_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(result_tx)));
    let closure_result_tx = result_tx.clone();

    let connect_result = agent_client_protocol::Client
        .builder()
        .name("bitrouter-health-check")
        .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
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
            UpstreamConnection::spawn("bash", &["-c".into(), script.into()], &HashMap::new(), None)
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
            UpstreamConnection::spawn("bash", &["-c".into(), script.into()], &HashMap::new(), None)
                .await
                .expect("spawn");
        let usid = conn.acp_session_id().to_string();
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
