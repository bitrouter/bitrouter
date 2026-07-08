//! Downstream path — the down-facing ACP `Agent` endpoint.
//!
//! [`serve`] exposes one already-launched [`crate::engine::Session`] as a
//! **vanilla ACP Agent** over stdio. A manager (GUI / CLI / orchestrating agent)
//! connects as an ACP `Client` and drives standard ACP; this endpoint terminates
//! the agent role and delegates every method to the `Session`, which proxies the
//! single upstream agent. No `_conductor/*` extensions are involved.
//!
//! ## Request plane (manager → us → session)
//!
//! - `initialize` → minimal [`AgentCapabilities`].
//! - `session/new` → returns the session's `record_id` as the manager-facing
//!   session id. The upstream `acp_session_id` stays internal.
//! - `session/prompt` → forward the prompt's content blocks **verbatim** via
//!   [`Session::prompt_blocks`] — text, images, resources, and resource links
//!   all reach the upstream agent unmodified.
//! - `session/cancel` → [`Session::cancel`].
//! - `fs/*`, `terminal/*`, and any other method → answered with a JSON-RPC
//!   "method not found" error (spec §11 / R1). We never silently drop. Sandboxed
//!   `fs`/`terminal` handlers are a follow-up.
//!
//! ## Callback plane (session → us → manager)
//!
//! Two background tasks, spawned once the connection is serving, forward the
//! upstream callbacks to the manager:
//!
//! - **Updates:** [`Session::raw_updates`] yields each raw ACP [`SessionUpdate`];
//!   we wrap it in a [`SessionNotification`] (with the manager-facing session id)
//!   and send it as a `session/update` notification — verbatim, no reverse
//!   mapping.
//! - **Permissions:** [`Session::permissions`] yields each [`PendingPermission`];
//!   we re-issue it to the manager as a `session/request_permission` request with
//!   the same tool-call and options, await the manager's
//!   [`RequestPermissionResponse`], and resolve the pending item with the
//!   manager's outcome **verbatim** — the exact chosen `optionId` reaches the
//!   upstream (validated there against the offered set), never a lossy
//!   collapse to option kind. The await happens inside a spawned task (off the
//!   dispatch path), so a slow manager never blocks message dispatch.
//!
//! ## Connection handle + lifecycle for the forwarding tasks
//!
//! Request handlers receive a `ConnectionTo<Client>` per call, but the forwarding
//! tasks must run for the connection's whole life. We get a long-lived handle the
//! same way [`crate::up`] does for the client side: `connect_with` runs a
//! `main_fn` closure that owns the `ConnectionTo<Client>`. The closure spawns the
//! two forwarding tasks via [`ConnectionTo::spawn`], then awaits a disconnect
//! signal.
//!
//! **Exit-on-disconnect.** `main_fn` must NOT park on `future::pending()`: the
//! SDK's `connect_with` runs `run_until(background, main_fn)`, and `background`
//! does not complete on a bare stdin EOF (its incoming actor stays alive while any
//! `ConnectionTo` clone — held by our handlers/forwarders — keeps the connection's
//! internal channels open). A `pending()` `main_fn` therefore never returns,
//! leaking the `bitrouter acp serve` process and orphaning the upstream agent
//! child. Instead we wrap the transport in [`EofSignaling`], which fires a
//! one-shot when the manager's write side hits EOF; `main_fn` awaits that and
//! returns, so `run_until` drops `background` (cancelling the forwarders) and the
//! `Arc<Session>` drops, killing the upstream child.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, RequestPermissionRequest, SessionId,
    SessionNotification,
};
use agent_client_protocol::{
    Agent, Channel, Client, ConnectTo, ConnectionTo, Dispatch, Handled, Responder, Stdio,
};
use futures::StreamExt;
use futures::channel::oneshot;

use crate::engine::Session;

/// Method names this endpoint answers explicitly. Everything else under the
/// `fs/` and `terminal/` namespaces (and any unknown method) gets a JSON-RPC
/// "method not found" reply via the dispatch catch-all.
const METHOD_SESSION_CANCEL: &str = "session/cancel";

/// A transport wrapper that fires a one-shot when the inner transport's
/// **incoming** stream ends (the manager's write side / our stdin hit EOF).
///
/// Why this exists: the SDK's `connect_with` drives `run_until(background,
/// main_fn)`. `background` (the connection's actors) does NOT complete on a bare
/// stdin EOF — its incoming actor stays alive as long as any `ConnectionTo` clone
/// holds the connection's internal channel senders (our handlers and forwarding
/// tasks do). So `serve`'s `main_fn` cannot park on `pending()`: it would never
/// return, leaking the `bitrouter acp serve` process and orphaning the upstream
/// agent child. This wrapper gives `main_fn` an explicit disconnect signal to
/// await; when it fires, `main_fn` returns, `run_until` drops `background` (which
/// cancels the forwarding tasks), and dropping the `Arc<Session>` kills the
/// upstream child.
struct EofSignaling<T> {
    inner: T,
    eof_tx: oneshot::Sender<()>,
}

impl<T: ConnectTo<Agent>> ConnectTo<Agent> for EofSignaling<T> {
    async fn connect_to(self, client: impl ConnectTo<Client>) -> agent_client_protocol::Result<()> {
        // We only ever drive this wrapper through `into_channel_and_future`
        // (that's what `Builder::connect_with` calls). Provide a correct
        // `connect_to` anyway by running the channel↔client copy and our spliced
        // transport future CONCURRENTLY (the channel copy and the splice each run
        // forever until the transport ends, so they must not be sequenced).
        let (channel, future) = self.into_channel_and_future();
        futures::future::try_join(ConnectTo::<Agent>::connect_to(channel, client), future).await?;
        Ok(())
    }

    fn into_channel_and_future(
        self,
    ) -> (
        Channel,
        futures::future::BoxFuture<'static, agent_client_protocol::Result<()>>,
    ) {
        let EofSignaling { inner, eof_tx } = self;
        let (inner_channel, inner_future) = inner.into_channel_and_future();
        let Channel {
            rx: mut inner_rx,
            tx: inner_tx,
        } = inner_channel;

        // Splice the inner incoming stream through a fresh channel so we observe
        // its termination (EOF) and fire `eof_tx`. Outgoing messages pass through
        // `inner_tx` unchanged.
        let (spliced_tx, spliced_rx) = futures::channel::mpsc::unbounded();
        let splice = async move {
            while let Some(msg) = inner_rx.next().await {
                if spliced_tx.unbounded_send(msg).is_err() {
                    break;
                }
            }
            // Inner incoming closed → manager disconnected. Signal once.
            let _ = eof_tx.send(());
            Ok(())
        };

        let combined = async move {
            // Run the inner transport future and the splice together; either
            // ending means the transport is done.
            futures::future::try_join(inner_future, splice).await?;
            Ok(())
        };

        (
            Channel {
                rx: spliced_rx,
                tx: inner_tx,
            },
            Box::pin(combined),
        )
    }
}

/// Serve `session` as a vanilla ACP Agent over stdio until the manager
/// disconnects. The returned future resolves when the stdio connection closes
/// (or errors), at which point the upstream agent child is reaped.
pub fn serve(
    session: Arc<Session>,
) -> impl std::future::Future<Output = agent_client_protocol::Result<()>> {
    serve_on(session, Stdio::new())
}

/// Serve `session` as a vanilla ACP Agent over an arbitrary transport. `serve`
/// pins this to [`Stdio`]; tests drive it over an in-memory
/// [`agent_client_protocol::Channel`] so a `serve`↔client round-trip needs no
/// subprocess.
fn serve_on(
    session: Arc<Session>,
    transport: impl ConnectTo<Agent> + 'static,
) -> impl std::future::Future<Output = agent_client_protocol::Result<()>> {
    // Wrap the transport so we get a one-shot when the manager disconnects
    // (incoming EOF). `main_fn` awaits this instead of parking forever.
    let (eof_tx, eof_rx) = oneshot::channel::<()>();
    let transport = EofSignaling {
        inner: transport,
        eof_tx,
    };

    // The manager-facing session id is our stable `record_id`; the upstream
    // `acp_session_id` never crosses this boundary.
    let record_id = session.state().record_id.clone();

    // One `Arc<Session>` clone per handler / forwarding closure that needs the
    // session. (`session/new` doesn't — it only echoes the record_id.)
    let record_for_new = record_id.clone();
    let session_prompt = Arc::clone(&session);
    let session_dispatch = Arc::clone(&session);
    let session_forward = session;
    let record_for_forward = record_id;

    Agent
        .builder()
        .name("bitrouter-session-agent")
        // ── initialize ──────────────────────────────────────────────────────
        .on_receive_request(
            move |req: InitializeRequest,
                  responder: Responder<InitializeResponse>,
                  _cx: ConnectionTo<Client>| async move {
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── session/new ─────────────────────────────────────────────────────
        .on_receive_request(
            // The handler doesn't touch the session: it just echoes our stable,
            // manager-facing `record_id` as the session id (the upstream
            // acp_session_id stays internal). The session was already launched.
            move |_req: NewSessionRequest,
                  responder: Responder<NewSessionResponse>,
                  _cx: ConnectionTo<Client>| {
                let record_id = record_for_new.clone();
                async move { responder.respond(NewSessionResponse::new(SessionId::new(record_id))) }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── session/prompt ──────────────────────────────────────────────────
        .on_receive_request(
            move |req: PromptRequest,
                  responder: Responder<PromptResponse>,
                  cx: ConnectionTo<Client>| {
                let session = Arc::clone(&session_prompt);
                async move {
                    // Drive the turn OUTSIDE the dispatch loop: a prompt can run
                    // long and triggers mid-turn `session/update` /
                    // `request_permission` traffic that must keep flowing while
                    // the turn is in flight. Returning the response over the
                    // responder from inside the spawned task keeps the dispatch
                    // loop responsive (mirrors the up.rs command-loop discipline).
                    cx.spawn(async move {
                        // Forward the content blocks verbatim (multi-modal).
                        match session.prompt_blocks(req.prompt).await {
                            Ok(resp) => responder.respond(resp),
                            Err(e) => responder
                                .respond_with_error(agent_client_protocol::util::internal_error(e)),
                        }
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ── catch-all: session/cancel, fs/*, terminal/*, unknown ────────────
        // This fallback sees every message no typed handler above claimed —
        // including responses to OUR server-initiated requests. It must
        // discriminate by `Dispatch` variant, not blanket-error every method
        // (the simple_agent example does the latter only because it never sends
        // requests, so it never receives responses).
        .on_receive_dispatch(
            move |message: Dispatch, cx: ConnectionTo<Client>| {
                let session = Arc::clone(&session_dispatch);
                async move {
                    match message {
                        // Responses to the agent's OWN server-initiated requests
                        // (e.g. `request_permission`) must NOT be answered here —
                        // pass them through so the SDK routes them back to the
                        // waiting `send_request`. Erroring them would break the
                        // permission round-trip.
                        Dispatch::Response(..) => Ok(Handled::No {
                            message,
                            retry: false,
                        }),
                        // `session/cancel` is a notification: cancel the in-flight
                        // turn off the dispatch path and claim the message.
                        Dispatch::Notification(_) if message.method() == METHOD_SESSION_CANCEL => {
                            cx.spawn(async move {
                                let _ = session.cancel().await;
                                Ok(())
                            })?;
                            Ok(Handled::Yes)
                        }
                        // Any other unhandled request (fs/*, terminal/*, unknown):
                        // answer with a proper "method not found" rather than
                        // dropping. Claims the message.
                        Dispatch::Request(..) => {
                            message.respond_with_error(
                                agent_client_protocol::schema::v1::Error::method_not_found(),
                                cx,
                            )?;
                            Ok(Handled::Yes)
                        }
                        // Any other unhandled notification: a notification has no
                        // reply, so pass it through rather than fabricating one.
                        Dispatch::Notification(_) => Ok(Handled::No {
                            message,
                            retry: false,
                        }),
                    }
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        // ── forwarding plane: spawn the two upstream→manager pumps ──────────
        .connect_with(
            transport,
            move |connection: ConnectionTo<Client>| async move {
                spawn_update_forwarder(&connection, &session_forward, record_for_forward.clone())?;
                spawn_permission_forwarder(&connection, &session_forward)?;
                // Keep the connection (and its handlers/forwarders) alive until
                // the manager disconnects (incoming EOF), then return so
                // `run_until` tears `background` down — cancelling the forwarding
                // tasks and dropping their `Arc<Session>` clones. The caller's
                // remaining `Arc<Session>` then drops on return, killing the
                // upstream agent child. `eof_rx` resolving `Err` (sender dropped)
                // is also a teardown signal, so either arm returns `Ok(())`.
                let _ = eof_rx.await;
                Ok(())
            },
        )
}

/// Spawn the task that forwards each raw upstream [`SessionUpdate`] to the
/// manager as a `session/update` notification, tagged with the manager-facing
/// session id.
fn spawn_update_forwarder(
    connection: &ConnectionTo<Client>,
    session: &Arc<Session>,
    record_id: String,
) -> agent_client_protocol::Result<()> {
    let mut updates = session.raw_updates();
    let conn = connection.clone();
    connection.spawn(async move {
        while let Some(update) = updates.next().await {
            // A send error means the connection is going away; stop forwarding.
            if conn
                .send_notification(SessionNotification::new(
                    SessionId::new(record_id.clone()),
                    update,
                ))
                .is_err()
            {
                break;
            }
        }
        Ok(())
    })
}

/// Spawn the task that re-issues each upstream [`PendingPermission`] to the
/// manager and resolves it with the manager's decision.
fn spawn_permission_forwarder(
    connection: &ConnectionTo<Client>,
    session: &Arc<Session>,
) -> agent_client_protocol::Result<()> {
    let mut permissions = session.permissions();
    let session_id = session.state().record_id.clone();
    let conn = connection.clone();
    connection.spawn(async move {
        while let Some(pending) = permissions.next().await {
            let request = RequestPermissionRequest::new(
                SessionId::new(session_id.clone()),
                pending.tool_call.clone(),
                pending.options.clone(),
            );
            // Drive each round-trip in its own task so a slow manager decision
            // never stalls the forwarder (and the dispatch loop) for the next
            // permission. The pending item moves in so its resolver lives until
            // the manager answers; if the connection drops mid-flight the item
            // is dropped, defaulting the upstream to Deny.
            conn.spawn({
                let conn = conn.clone();
                async move {
                    match conn.send_request(request).block_task().await {
                        // Resolve with the manager's outcome verbatim: the exact
                        // chosen optionId is preserved end-to-end (up.rs
                        // validates it against the offered set).
                        Ok(resp) => pending.resolve(resp.outcome),
                        // Manager errored or went away: drop the pending item,
                        // which defaults the upstream to Deny so it never hangs.
                        Err(_) => drop(pending),
                    }
                    Ok(())
                }
            })?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ContentBlock, TextContent};

    /// Launch a real [`Session`] whose upstream agent is a bash ACP stub running
    /// `stub`. Shared by the duplex `serve_on`↔client tests below. No worktree is
    /// created, so the caller just drops the returned `Arc` to tear down.
    #[cfg(unix)]
    async fn launch_stub_session(stub: &str) -> (Arc<Session>, tempfile::TempDir) {
        use std::collections::HashMap;

        use bitrouter_sdk::acp::{AcpAgentConfig, AcpTransport, ConfigAcpRoutingTable};

        let cfg = AcpAgentConfig {
            name: "stub".to_string(),
            transport: AcpTransport::Stdio {
                command: "bash".to_string(),
                args: vec!["-c".to_string(), stub.to_string()],
                env: HashMap::new(),
            },
        };
        let catalog =
            ConfigAcpRoutingTable::from_configs([("stub".to_string(), cfg)]).expect("catalog");
        let base = tempfile::tempdir().expect("tempdir");
        let session = Arc::new(
            Session::launch(&catalog, "stub", base.path().to_path_buf(), None)
                .await
                .expect("launch"),
        );
        (session, base)
    }

    /// Full `serve`↔client round-trip over an in-memory duplex transport
    /// ([`agent_client_protocol::Channel`]), no subprocess for the manager side.
    ///
    /// A real [`Session`] backed by a bash ACP stub is served via [`serve_on`]
    /// over one half of the duplex; a test ACP `Client` drives the other half.
    /// The test asserts that `session/new` returns our `record_id`, that
    /// `session/prompt` resolves with `end_turn`, and that the upstream's
    /// streamed `session/update` reaches the client verbatim (carrying "hi").
    ///
    /// Runtime shape mirrors the `agent-client-protocol` duplex tests: a
    /// `current_thread` runtime with a [`tokio::task::LocalSet`], the agent side
    /// driven by `spawn_local`, and the client driving from `run_until`. A
    /// duplex connection needs both ends polled on the same executor.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn serve_round_trips_new_prompt_and_update() {
        use agent_client_protocol::schema::ProtocolVersion;
        use agent_client_protocol::schema::v1::{
            InitializeRequest, NewSessionRequest, PromptRequest, SessionNotification, StopReason,
        };
        use agent_client_protocol::{Channel, Client, ConnectionTo};
        use tokio::task::LocalSet;

        // The upstream stub streams an `agent_message_chunk` update then ends.
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

        let local = LocalSet::new();
        local
            .run_until(async {
                let (session, _base) = launch_stub_session(BASH_STUB).await;
                let record_id = session.state().record_id.clone();

                // ── serve(agent side) ↔ test Client over an in-memory duplex. ──
                let (agent_channel, client_channel) = Channel::duplex();
                let agent = tokio::task::spawn_local(serve_on(Arc::clone(&session), agent_channel));

                // The client captures `session/update` notifications so the
                // main_fn can await one after prompting.
                let (update_tx, mut update_rx) =
                    futures::channel::mpsc::unbounded::<SessionNotification>();

                let client_result = Client
                    .builder()
                    .name("test-manager")
                    .on_receive_notification(
                        move |notif: SessionNotification, _cx: ConnectionTo<Agent>| {
                            let update_tx = update_tx.clone();
                            async move {
                                let _ = update_tx.unbounded_send(notif);
                                Ok(())
                            }
                        },
                        agent_client_protocol::on_receive_notification!(),
                    )
                    .connect_with(client_channel, |cx: ConnectionTo<Agent>| async move {
                        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;

                        let new_session = cx
                            .send_request(NewSessionRequest::new(std::path::PathBuf::from("/")))
                            .block_task()
                            .await?;
                        // session/new returns our manager-facing record_id, not "u1".
                        assert_eq!(new_session.session_id.0.to_string(), record_id);

                        let resp = cx
                            .send_request(PromptRequest::new(
                                new_session.session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new("do X".to_string()))],
                            ))
                            .block_task()
                            .await?;
                        assert_eq!(resp.stop_reason, StopReason::EndTurn);

                        // The upstream's streamed update was forwarded verbatim.
                        let mut saw_hi = false;
                        for _ in 0..8 {
                            match update_rx.next().await {
                                Some(n) if format!("{:?}", n.update).contains("hi") => {
                                    assert_eq!(n.session_id.0.to_string(), record_id);
                                    saw_hi = true;
                                    break;
                                }
                                Some(_) => continue,
                                None => break,
                            }
                        }
                        assert!(saw_hi, "expected a forwarded session/update carrying 'hi'");
                        Ok(())
                    })
                    .await;

                assert!(client_result.is_ok(), "client failed: {client_result:?}");

                // Client `main_fn` returning closes the duplex, so `serve_on`'s
                // connection ends and its future resolves (with an error, since
                // its `main_fn` is `pending` — expected; we only need it to stop).
                agent.abort();
                let _ = agent.await;

                // No worktree (None) → nothing on disk to clean. Dropping all
                // references lets the upstream child be reaped.
                drop(session);
            })
            .await;
    }

    /// End-to-end permission round-trip:
    /// upstream `request_permission` → up.rs `PendingPermission` →
    /// down.rs forwarder re-issues to the manager → test client selects the
    /// allow option → `pending.resolve` passes the exact selection through →
    /// up.rs `sanitize_selection` validates it → upstream gets the allow id
    /// back.
    ///
    /// The upstream stub offers `allow_once` (id `allow`) + `reject_once` (id
    /// `rej`) mid-prompt, reads the response, and echoes the chosen optionId into
    /// a `session/update` (`chose:<id>`). The test asserts the forwarded update
    /// carries `chose:allow`, proving the manager's allow choice reached the
    /// upstream through the full down→manager→down→upstream path.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn serve_forwards_permission_and_resolves_allow() {
        use agent_client_protocol::schema::ProtocolVersion;
        use agent_client_protocol::schema::v1::{
            InitializeRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
            RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
            SessionNotification, StopReason,
        };
        use agent_client_protocol::{Channel, Client, ConnectionTo, Responder};
        use tokio::task::LocalSet;

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

        let local = LocalSet::new();
        local
            .run_until(async {
                let (session, _base) = launch_stub_session(PERM_STUB).await;

                let (agent_channel, client_channel) = Channel::duplex();
                let agent = tokio::task::spawn_local(serve_on(Arc::clone(&session), agent_channel));

                let (update_tx, mut update_rx) =
                    futures::channel::mpsc::unbounded::<SessionNotification>();

                let client_result = Client
                    .builder()
                    .name("test-manager")
                    // Forwarded `session/update`s land here.
                    .on_receive_notification(
                        move |notif: SessionNotification, _cx: ConnectionTo<Agent>| {
                            let update_tx = update_tx.clone();
                            async move {
                                let _ = update_tx.unbounded_send(notif);
                                Ok(())
                            }
                        },
                        agent_client_protocol::on_receive_notification!(),
                    )
                    // The manager answers the forwarded `request_permission` by
                    // selecting the `allow_once` option from the offered set.
                    .on_receive_request(
                        move |req: RequestPermissionRequest,
                              responder: Responder<RequestPermissionResponse>,
                              _cx: ConnectionTo<Agent>| async move {
                            let allow_id = req
                                .options
                                .iter()
                                .find(|o| {
                                    matches!(
                                        o.kind,
                                        agent_client_protocol::schema::v1::PermissionOptionKind::AllowOnce
                                    )
                                })
                                .map(|o| o.option_id.clone())
                                .expect("allow_once option forwarded to manager");
                            responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Selected(
                                    SelectedPermissionOutcome::new(allow_id),
                                ),
                            ))
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .connect_with(client_channel, |cx: ConnectionTo<Agent>| async move {
                        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;
                        let new_session = cx
                            .send_request(NewSessionRequest::new(std::path::PathBuf::from("/")))
                            .block_task()
                            .await?;
                        let resp = cx
                            .send_request(PromptRequest::new(
                                new_session.session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new("do X".to_string()))],
                            ))
                            .block_task()
                            .await?;
                        assert_eq!(resp.stop_reason, StopReason::EndTurn);

                        // The upstream echoed the chosen optionId; it must be the
                        // allow option, proving the full round-trip selected it.
                        let mut chose = None;
                        for _ in 0..8 {
                            match update_rx.next().await {
                                Some(n) => {
                                    let s = format!("{:?}", n.update);
                                    if s.contains("chose:") {
                                        chose = Some(s);
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                        let chose = chose.expect("upstream echoed a chosen optionId");
                        assert!(
                            chose.contains("chose:allow"),
                            "expected allow option chosen, got: {chose}"
                        );
                        Ok(())
                    })
                    .await;

                assert!(client_result.is_ok(), "client failed: {client_result:?}");
                agent.abort();
                let _ = agent.await;
                drop(session);
            })
            .await;
    }

    /// Non-text content blocks survive the manager→substrate→upstream path
    /// verbatim. The manager prompts with a text block **and** a resource-link
    /// block; the upstream stub echoes `sawlink` only if the `session/prompt`
    /// line it received contains a `resource_link` block.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn serve_forwards_non_text_prompt_blocks_verbatim() {
        use agent_client_protocol::schema::ProtocolVersion;
        use agent_client_protocol::schema::v1::{
            InitializeRequest, NewSessionRequest, PromptRequest, ResourceLink,
            SessionNotification, StopReason,
        };
        use agent_client_protocol::{Channel, Client, ConnectionTo};
        use tokio::task::LocalSet;

        const LINK_STUB: &str = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
                *session/prompt*)
                    case "$line" in
                      *resource_link*) printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"sawlink"}}}}\n';;
                      *)               printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"nolink"}}}}\n';;
                    esac
                    printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
              esac
            done
        "#;

        let local = LocalSet::new();
        local
            .run_until(async {
                let (session, _base) = launch_stub_session(LINK_STUB).await;

                let (agent_channel, client_channel) = Channel::duplex();
                let agent = tokio::task::spawn_local(serve_on(Arc::clone(&session), agent_channel));

                let (update_tx, mut update_rx) =
                    futures::channel::mpsc::unbounded::<SessionNotification>();

                let client_result = Client
                    .builder()
                    .name("test-manager")
                    .on_receive_notification(
                        move |notif: SessionNotification, _cx: ConnectionTo<Agent>| {
                            let update_tx = update_tx.clone();
                            async move {
                                let _ = update_tx.unbounded_send(notif);
                                Ok(())
                            }
                        },
                        agent_client_protocol::on_receive_notification!(),
                    )
                    .connect_with(client_channel, |cx: ConnectionTo<Agent>| async move {
                        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;
                        let new_session = cx
                            .send_request(NewSessionRequest::new(std::path::PathBuf::from("/")))
                            .block_task()
                            .await?;
                        let resp = cx
                            .send_request(PromptRequest::new(
                                new_session.session_id.clone(),
                                vec![
                                    ContentBlock::Text(TextContent::new("look at".to_string())),
                                    ContentBlock::ResourceLink(ResourceLink::new(
                                        "x",
                                        "file:///x.rs",
                                    )),
                                ],
                            ))
                            .block_task()
                            .await?;
                        assert_eq!(resp.stop_reason, StopReason::EndTurn);

                        let mut echoed = None;
                        for _ in 0..8 {
                            match update_rx.next().await {
                                Some(n) => {
                                    let s = format!("{:?}", n.update);
                                    if s.contains("sawlink") || s.contains("nolink") {
                                        echoed = Some(s);
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                        let echoed = echoed.expect("upstream echoed a block probe");
                        assert!(
                            echoed.contains("sawlink"),
                            "resource_link block must reach the upstream verbatim; got: {echoed}"
                        );
                        Ok(())
                    })
                    .await;

                assert!(client_result.is_ok(), "client failed: {client_result:?}");
                agent.abort();
                let _ = agent.await;
                drop(session);
            })
            .await;
    }

    /// The manager's exact `optionId` survives the full round-trip when the
    /// upstream offers **two options of the same kind** — the case a
    /// kind-collapsing translation cannot represent. The stub offers two
    /// `allow_once` options (`allow1`, `allow2`); the manager picks `allow2`;
    /// the upstream must see `allow2`, not the first same-kind option.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn serve_preserves_exact_option_id_between_same_kind_options() {
        use agent_client_protocol::schema::ProtocolVersion;
        use agent_client_protocol::schema::v1::{
            InitializeRequest, NewSessionRequest, PermissionOptionId, PromptRequest,
            RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
            SelectedPermissionOutcome, SessionNotification, StopReason,
        };
        use agent_client_protocol::{Channel, Client, ConnectionTo, Responder};
        use tokio::task::LocalSet;

        const TWO_ALLOW_STUB: &str = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
                *session/prompt*)
                    printf '{"jsonrpc":"2.0","id":"99","method":"session/request_permission","params":{"sessionId":"u1","toolCall":{"toolCallId":"tc1","title":"run npm"},"options":[{"optionId":"allow1","name":"Allow this command","kind":"allow_once"},{"optionId":"allow2","name":"Allow all npm commands","kind":"allow_once"},{"optionId":"rej","name":"Reject","kind":"reject_once"}]}}\n'
                    read resp
                    chosen=$(echo "$resp" | sed -n 's/.*"optionId":"\([^"]*\)".*/\1/p')
                    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"chose:%s"}}}}\n' "$chosen"
                    printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
              esac
            done
        "#;

        let local = LocalSet::new();
        local
            .run_until(async {
                let (session, _base) = launch_stub_session(TWO_ALLOW_STUB).await;

                let (agent_channel, client_channel) = Channel::duplex();
                let agent = tokio::task::spawn_local(serve_on(Arc::clone(&session), agent_channel));

                let (update_tx, mut update_rx) =
                    futures::channel::mpsc::unbounded::<SessionNotification>();

                let client_result = Client
                    .builder()
                    .name("test-manager")
                    .on_receive_notification(
                        move |notif: SessionNotification, _cx: ConnectionTo<Agent>| {
                            let update_tx = update_tx.clone();
                            async move {
                                let _ = update_tx.unbounded_send(notif);
                                Ok(())
                            }
                        },
                        agent_client_protocol::on_receive_notification!(),
                    )
                    // The manager picks the SECOND allow_once option by id.
                    .on_receive_request(
                        move |req: RequestPermissionRequest,
                              responder: Responder<RequestPermissionResponse>,
                              _cx: ConnectionTo<Agent>| async move {
                            assert!(
                                req.options
                                    .iter()
                                    .any(|o| o.option_id == PermissionOptionId::new("allow2")),
                                "both allow options must be forwarded to the manager"
                            );
                            responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                    PermissionOptionId::new("allow2"),
                                )),
                            ))
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .connect_with(client_channel, |cx: ConnectionTo<Agent>| async move {
                        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;
                        let new_session = cx
                            .send_request(NewSessionRequest::new(std::path::PathBuf::from("/")))
                            .block_task()
                            .await?;
                        let resp = cx
                            .send_request(PromptRequest::new(
                                new_session.session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new("do X".to_string()))],
                            ))
                            .block_task()
                            .await?;
                        assert_eq!(resp.stop_reason, StopReason::EndTurn);

                        let mut chose = None;
                        for _ in 0..8 {
                            match update_rx.next().await {
                                Some(n) => {
                                    let s = format!("{:?}", n.update);
                                    if s.contains("chose:") {
                                        chose = Some(s);
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                        let chose = chose.expect("upstream echoed a chosen optionId");
                        assert!(
                            chose.contains("chose:allow2"),
                            "exact optionId must survive; got: {chose}"
                        );
                        Ok(())
                    })
                    .await;

                assert!(client_result.is_ok(), "client failed: {client_result:?}");
                agent.abort();
                let _ = agent.await;
                drop(session);
            })
            .await;
    }

    /// `session/cancel` from the manager is dispatched without error. The stub
    /// answers the handshake; the client sends a `session/cancel` notification
    /// and the connection stays healthy (a follow-up `initialize` round-trips),
    /// proving the catch-all dispatched cancel rather than erroring or hanging.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn serve_dispatches_cancel_without_error() {
        use agent_client_protocol::schema::ProtocolVersion;
        use agent_client_protocol::schema::v1::{
            CancelNotification, InitializeRequest, NewSessionRequest, SessionId,
        };
        use agent_client_protocol::{Channel, Client, ConnectionTo};
        use tokio::task::LocalSet;

        const HANDSHAKE_STUB: &str = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*) printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
                *session/cancel*) : ;;
              esac
            done
        "#;

        let local = LocalSet::new();
        local
            .run_until(async {
                let (session, _base) = launch_stub_session(HANDSHAKE_STUB).await;
                let record_id = session.state().record_id.clone();

                let (agent_channel, client_channel) = Channel::duplex();
                let agent = tokio::task::spawn_local(serve_on(Arc::clone(&session), agent_channel));

                let client_result = Client
                    .builder()
                    .name("test-manager")
                    .connect_with(client_channel, move |cx: ConnectionTo<Agent>| async move {
                        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;
                        cx.send_request(NewSessionRequest::new(std::path::PathBuf::from("/")))
                            .block_task()
                            .await?;

                        // Fire a cancel notification (no reply expected).
                        cx.send_notification(CancelNotification::new(SessionId::new(record_id)))?;

                        // The connection is still healthy: another request
                        // round-trips, so cancel was dispatched (not fatal).
                        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;
                        Ok(())
                    })
                    .await;

                assert!(
                    client_result.is_ok(),
                    "cancel must not break the connection: {client_result:?}"
                );
                agent.abort();
                let _ = agent.await;
                drop(session);
            })
            .await;
    }

    /// A method our agent doesn't handle is answered with a JSON-RPC
    /// method-not-found error via the catch-all, never silently dropped or hung.
    ///
    /// We send `authenticate` (a valid Client→Agent request our endpoint does
    /// not implement). It exercises the exact same catch-all arm as `fs/*` /
    /// `terminal/*`: any method that isn't `session/cancel` or a typed-handled
    /// method → `Error::method_not_found()`. (`fs/*` and `terminal/*` are
    /// agent→client requests in ACP, so a Client can't issue them through a typed
    /// `send_request`; `authenticate` is the cleanest unhandled method a manager
    /// can actually send.)
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn serve_rejects_unknown_method_with_method_not_found() {
        use agent_client_protocol::schema::ProtocolVersion;
        use agent_client_protocol::schema::v1::{AuthenticateRequest, InitializeRequest};
        use agent_client_protocol::{Channel, Client, ConnectionTo};
        use tokio::task::LocalSet;

        // Must answer both handshake steps: `Session::launch` runs
        // `initialize` + `session/new` against the upstream before serving.
        const HANDSHAKE_STUB: &str = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*) printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
              esac
            done
        "#;

        let local = LocalSet::new();
        local
            .run_until(async {
                let (session, _base) = launch_stub_session(HANDSHAKE_STUB).await;

                let (agent_channel, client_channel) = Channel::duplex();
                let agent = tokio::task::spawn_local(serve_on(Arc::clone(&session), agent_channel));

                let client_result = Client
                    .builder()
                    .name("test-manager")
                    .connect_with(client_channel, |cx: ConnectionTo<Agent>| async move {
                        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;

                        // Our agent has no `authenticate` handler → catch-all →
                        // method-not-found (same arm fs/* and terminal/* hit).
                        let outcome = cx
                            .send_request(AuthenticateRequest::new("none"))
                            .block_task()
                            .await;
                        assert!(
                            outcome.is_err(),
                            "unhandled method must be method-not-found, got: {outcome:?}"
                        );
                        Ok(())
                    })
                    .await;

                assert!(client_result.is_ok(), "client failed: {client_result:?}");
                agent.abort();
                let _ = agent.await;
                drop(session);
            })
            .await;
    }

    /// Regression: after a full session round-trip (so the forwarding tasks are
    /// live), the manager disconnecting must make `serve_on` complete on its own
    /// — NOT hang. Before the [`EofSignaling`] transport fix, `main_fn` parked on
    /// `pending()` and never returned because the connection's `background`
    /// actors stay alive while any `ConnectionTo` clone (held by the forwarders)
    /// exists; the process leaked and the upstream child was orphaned.
    ///
    /// We also assert that once `serve_on` returns, the test's `Arc<Session>` is
    /// the **sole** owner (strong_count == 1) — proving no forwarding task
    /// retained a clone, so dropping it here reaps the upstream child.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn serve_exits_when_manager_disconnects() {
        use agent_client_protocol::schema::ProtocolVersion;
        use agent_client_protocol::schema::v1::{
            InitializeRequest, NewSessionRequest, PromptRequest, StopReason,
        };
        use agent_client_protocol::{Channel, Client, ConnectionTo};
        use tokio::task::LocalSet;

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

        let local = LocalSet::new();
        local
            .run_until(async {
                let (session, _base) = launch_stub_session(BASH_STUB).await;
                let (agent_channel, client_channel) = Channel::duplex();
                let agent = tokio::task::spawn_local(serve_on(Arc::clone(&session), agent_channel));

                let client_result = Client
                    .builder()
                    .name("m")
                    .connect_with(client_channel, |cx: ConnectionTo<Agent>| async move {
                        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;
                        let ns = cx
                            .send_request(NewSessionRequest::new(std::path::PathBuf::from("/")))
                            .block_task()
                            .await?;
                        let resp = cx
                            .send_request(PromptRequest::new(
                                ns.session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new("x".to_string()))],
                            ))
                            .block_task()
                            .await?;
                        assert_eq!(resp.stop_reason, StopReason::EndTurn);
                        Ok(())
                    })
                    .await;
                assert!(client_result.is_ok());

                // Client returned → its `connect_with` closed the duplex → manager
                // disconnect. `serve_on` MUST now complete on its own (no abort).
                let exited = tokio::time::timeout(std::time::Duration::from_secs(5), agent).await;
                assert!(
                    exited.is_ok(),
                    "serve_on did NOT exit on manager disconnect (hung)"
                );
                let join = exited.expect("serve_on exited");
                assert!(join.is_ok(), "serve_on task panicked: {join:?}");

                // No forwarding task retained an `Arc<Session>` clone, so the test
                // is the sole owner; dropping it reaps the upstream child.
                assert_eq!(
                    Arc::strong_count(&session),
                    1,
                    "a forwarding task leaked an Arc<Session> clone"
                );
                drop(session);
            })
            .await;
    }
}
