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
//! - `session/prompt` → concatenate the `ContentBlock::Text` parts and call
//!   [`Session::prompt`]. **v1 limitation:** non-text content blocks (images,
//!   resources, resource links, …) are dropped; faithful multi-modal forwarding
//!   is a follow-up.
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
//!   [`RequestPermissionResponse`], map the chosen option back to a
//!   [`PermissionOutcome`] via [`outcome_from_selection`], and resolve the pending
//!   item. The await happens inside a spawned task (off the dispatch path), so a
//!   slow manager never blocks message dispatch.
//!
//! ## Connection handle for the forwarding tasks
//!
//! Request handlers receive a `ConnectionTo<Client>` per call, but the forwarding
//! tasks must run for the connection's whole life. We get a long-lived handle the
//! same way [`crate::up`] does for the client side: `connect_with` runs a
//! `main_fn` closure that owns the `ConnectionTo<Client>`. The closure spawns the
//! two forwarding tasks via [`ConnectionTo::spawn`] and then parks on
//! `future::pending()`, keeping the connection (and its background actors) alive
//! until stdio closes.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, RequestPermissionRequest, SessionId,
    SessionNotification,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Dispatch, Responder, Stdio};
use futures::StreamExt;

use crate::engine::Session;
use crate::translate::outcome_from_selection;

/// Method names this endpoint answers explicitly. Everything else under the
/// `fs/` and `terminal/` namespaces (and any unknown method) gets a JSON-RPC
/// "method not found" reply via the dispatch catch-all.
const METHOD_SESSION_CANCEL: &str = "session/cancel";

/// Concatenate the text of a prompt's content blocks.
///
/// **v1 limitation:** only [`ContentBlock::Text`] parts contribute; every other
/// variant (image, audio, resource, resource link, …) is dropped. The substrate
/// `Session` is text-in/text-out for now, so faithful multi-modal forwarding is a
/// follow-up.
fn prompt_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(t) = block {
            out.push_str(&t.text);
        }
    }
    out
}

/// Serve `session` as a vanilla ACP Agent over stdio until the manager
/// disconnects. The returned future resolves when the stdio connection closes
/// (or errors).
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
    // The manager-facing session id is our stable `record_id`; the upstream
    // `acp_session_id` never crosses this boundary.
    let record_id = session.state().record_id.clone();

    // One `Arc<Session>` clone per handler / forwarding closure.
    let session_new = Arc::clone(&session);
    let record_for_new = record_id.clone();
    let session_prompt = Arc::clone(&session);
    let session_dispatch = Arc::clone(&session);
    let session_forward = Arc::clone(&session);
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
            move |_req: NewSessionRequest,
                  responder: Responder<NewSessionResponse>,
                  _cx: ConnectionTo<Client>| {
                // Keep the session alive for the whole connection: a handler
                // closure must own a clone so the `Arc` count never drops to the
                // forwarding tasks alone.
                let _keep = Arc::clone(&session_new);
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
                    let text = prompt_text(&req.prompt);
                    // Drive the turn OUTSIDE the dispatch loop: a prompt can run
                    // long and triggers mid-turn `session/update` /
                    // `request_permission` traffic that must keep flowing while
                    // the turn is in flight. Returning the response over the
                    // responder from inside the spawned task keeps the dispatch
                    // loop responsive (mirrors the up.rs command-loop discipline).
                    cx.spawn(async move {
                        match session.prompt(&text).await {
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
        .on_receive_dispatch(
            move |message: Dispatch, cx: ConnectionTo<Client>| {
                let session = Arc::clone(&session_dispatch);
                async move {
                    if message.method() == METHOD_SESSION_CANCEL {
                        // `session/cancel` is a notification; cancel the in-flight
                        // turn off the dispatch path and acknowledge.
                        cx.spawn(async move {
                            let _ = session.cancel().await;
                            Ok(())
                        })?;
                        return Ok(());
                    }
                    // fs/*, terminal/*, and any other unhandled method: answer
                    // with a proper "method not found" rather than dropping.
                    message.respond_with_error(
                        agent_client_protocol::schema::v1::Error::method_not_found(),
                        cx,
                    )
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
                // Park: keep the connection (and its handlers) alive until stdio
                // closes, at which point `connect_with` tears the future down.
                std::future::pending::<agent_client_protocol::Result<()>>().await
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
            let options = pending.options.clone();
            // Drive each round-trip in its own task so a slow manager decision
            // never stalls the forwarder (and the dispatch loop) for the next
            // permission. The pending item moves in so its resolver lives until
            // the manager answers; if the connection drops mid-flight the item
            // is dropped, defaulting the upstream to Deny.
            conn.spawn({
                let conn = conn.clone();
                async move {
                    match conn.send_request(request).block_task().await {
                        Ok(resp) => {
                            let outcome = outcome_from_selection(&resp.outcome, &options);
                            pending.resolve(outcome);
                        }
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

    #[test]
    fn prompt_text_concatenates_text_blocks() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("hello ".to_string())),
            ContentBlock::Text(TextContent::new("world".to_string())),
        ];
        assert_eq!(prompt_text(&blocks), "hello world");
    }

    #[test]
    fn prompt_text_drops_non_text_blocks() {
        // A resource-link block carries no text and must be dropped (v1 limit),
        // leaving only the text parts.
        use agent_client_protocol::schema::v1::ResourceLink;
        let blocks = vec![
            ContentBlock::Text(TextContent::new("keep".to_string())),
            ContentBlock::ResourceLink(ResourceLink::new("x", "file:///x")),
        ];
        assert_eq!(prompt_text(&blocks), "keep");
    }

    #[test]
    fn prompt_text_empty_for_no_text() {
        assert_eq!(prompt_text(&[]), "");
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
        use std::collections::HashMap;

        use agent_client_protocol::schema::ProtocolVersion;
        use agent_client_protocol::schema::v1::{
            InitializeRequest, NewSessionRequest, PromptRequest, SessionNotification, StopReason,
        };
        use agent_client_protocol::{Channel, Client, ConnectionTo};
        use bitrouter_sdk::acp::{AcpAgentConfig, AcpTransport, ConfigAcpRoutingTable};
        use tokio::task::LocalSet;

        // ── A real Session backed by a bash ACP stub (mirrors engine.rs). ───
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
                let cfg = AcpAgentConfig {
                    name: "stub".to_string(),
                    transport: AcpTransport::Stdio {
                        command: "bash".to_string(),
                        args: vec!["-c".to_string(), BASH_STUB.to_string()],
                        env: HashMap::new(),
                    },
                };
                let catalog = ConfigAcpRoutingTable::from_configs([("stub".to_string(), cfg)])
                    .expect("catalog");
                let base = tempfile::tempdir().expect("tempdir");
                let session = Arc::new(
                    Session::launch(&catalog, "stub", base.path().to_path_buf(), None)
                        .await
                        .expect("launch"),
                );
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
}
