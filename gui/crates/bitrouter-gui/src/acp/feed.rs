//! `AcpFeed` — a real `Feed` that drives one ACP session through
//! `bitrouter acp serve --agent <id>`. Owns a tokio runtime on a dedicated thread
//! (the `ai.rs` pattern) and bridges ACP to the `Feed`'s `futures` channels.
//!
//! ## Deadlock avoidance
//!
//! The command loop (inside the `connect_with` main closure) MUST NOT block on a
//! prompt turn to completion: if it did and the agent issued a
//! `session/request_permission` mid-turn, the loop could not process the
//! resolving `ResolvePending` command and we would deadlock. So `SendPrompt`
//! drives the `PromptRequest` inside a `connection.spawn(...)` task and the loop
//! returns to selecting on the command channel immediately.
//!
//! The permission handler likewise does its blocking wait (awaiting the GUI's
//! `ResolvePending` over a oneshot) inside a `connection.spawn(...)` task rather
//! than directly in the `on_receive_request` callback, so it never blocks the
//! dispatch loop (per the SDK's `concepts::ordering` guidance).
//!
//! ## Lock discipline
//!
//! The `std::sync::Mutex` guarding the pending-permission map is only ever held
//! for the synchronous insert/remove — never across an `.await`.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, RequestPermissionRequest,
    RequestPermissionResponse, SessionNotification, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo, Responder};
use bitrouter_gui_core::feed::{Feed, FeedHandle};
use bitrouter_gui_core::protocol::{
    Command, Event, PermissionOutcome, RenderMode, Session, SessionId, SessionStatus,
    SessionUpdateKind, TabId,
};
use futures::channel::{mpsc, oneshot};
use futures::StreamExt;

use super::translate::{render_diff, select_option, translate};

/// Shared registry of in-flight permission requests, keyed by the GUI-facing
/// request id we mint. The sender resolves the parked permission handler.
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<PermissionOutcome>>>>;

/// Fixed display session id used for every GUI-facing [`SessionId`]. The ACP
/// `session_id` returned by `new_session` is a *separate* value, used only to
/// address `PromptRequest`s.
const GUI_SESSION: &str = "acp-session";

pub struct AcpFeed {
    agent_command: String,
    agent_id: String,
}

impl AcpFeed {
    pub fn new(bin: &str, agent_id: &str) -> Self {
        Self {
            agent_command: format!("{bin} acp serve --agent {agent_id}"),
            agent_id: agent_id.to_string(),
        }
    }

    pub fn from_env() -> Self {
        let bin = std::env::var("BITROUTER_BIN").unwrap_or_else(|_| "bitrouter".into());
        // `claude-acp` is the bitrouter catalog id for Anthropic Claude (Zed's
        // `claude-code-acp`), passed as `acp serve --agent <id>`; verified against
        // `bitrouter agents list`. Override with BITROUTER_GUI_AGENT for any other
        // configured agent.
        let agent = std::env::var("BITROUTER_GUI_AGENT").unwrap_or_else(|_| "claude-acp".into());
        Self::new(&bin, &agent)
    }
}

impl Feed for AcpFeed {
    fn connect(self) -> FeedHandle {
        let (event_tx, event_rx) = mpsc::unbounded::<Event>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded::<Command>();
        let agent_command = self.agent_command;
        let agent_id = self.agent_id;
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = event_tx.unbounded_send(Event::SessionUpdate {
                        session: SessionId(GUI_SESSION.into()),
                        update: SessionUpdateKind::Message {
                            text: format!("failed to start ACP runtime: {e}"),
                        },
                    });
                    let _ = event_tx.unbounded_send(Event::AgentExited {
                        session: SessionId(GUI_SESSION.into()),
                        code: 1,
                    });
                    return;
                }
            };
            rt.block_on(run(agent_command, agent_id, event_tx, cmd_rx));
        });
        FeedHandle {
            events: Box::pin(event_rx),
            commands: cmd_tx,
        }
    }
}

/// Top-level async entry: drive the ACP session and, on error/exit, emit a
/// closing `SessionUpdate` + `AgentExited` so the GUI sees a clean teardown.
async fn run(
    agent_command: String,
    agent_id: String,
    event_tx: mpsc::UnboundedSender<Event>,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
) {
    let result = drive(agent_command, agent_id, event_tx.clone(), cmd_rx).await;
    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = event_tx.unbounded_send(Event::SessionUpdate {
                session: SessionId(GUI_SESSION.into()),
                update: SessionUpdateKind::Message {
                    text: format!("ACP session ended: {e}"),
                },
            });
            1
        }
    };
    let _ = event_tx.unbounded_send(Event::AgentExited {
        session: SessionId(GUI_SESSION.into()),
        code,
    });
}

/// Build the ACP client, perform the handshake, emit `AgentSpawned`, then run a
/// command loop that stays responsive to `ResolvePending` while a prompt turn is
/// in flight.
async fn drive(
    agent_command: String,
    agent_id: String,
    event_tx: mpsc::UnboundedSender<Event>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
) -> anyhow::Result<()> {
    let agent = AcpAgent::from_str(&agent_command)?;
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

    // Clones captured by the notification handler.
    let notif_tx = event_tx.clone();
    // Clones captured by the permission handler.
    let perm_tx = event_tx.clone();
    let perm_pending = pending.clone();

    agent_client_protocol::Client
        .builder()
        .name("bitrouter-gui")
        .on_receive_notification(
            move |notification: SessionNotification, _cx| {
                let notif_tx = notif_tx.clone();
                async move {
                    if let Some(update) = translate(notification.update) {
                        let _ = notif_tx.unbounded_send(Event::SessionUpdate {
                            session: SessionId(GUI_SESSION.into()),
                            update,
                        });
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
                let perm_tx = perm_tx.clone();
                let perm_pending = perm_pending.clone();
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

                    // Register the resolver BEFORE emitting the event so a fast
                    // `ResolvePending` can never race ahead of the insert.
                    let (tx, rx) = oneshot::channel::<PermissionOutcome>();
                    {
                        // Lock held only for the insert — never across an await.
                        let mut guard = perm_pending.lock().expect("pending mutex poisoned");
                        guard.insert(request_id.clone(), tx);
                    }

                    let _ = perm_tx.unbounded_send(Event::PermissionRequested {
                        session: SessionId(GUI_SESSION.into()),
                        request_id: request_id.clone(),
                        summary,
                        diff,
                    });

                    // Park the wait + respond OUTSIDE the dispatch loop so other
                    // messages keep flowing while the user decides.
                    let options = request.options.clone();
                    let request_id_for_cleanup = request_id.clone();
                    if let Err(e) = connection.spawn({
                        let perm_pending = perm_pending.clone();
                        async move {
                            // Default to Deny if the oneshot is dropped (GUI gone).
                            let outcome = rx.await.unwrap_or(PermissionOutcome::Deny);
                            // Defensive: drop any lingering entry for this id.
                            {
                                let mut guard =
                                    perm_pending.lock().expect("pending mutex poisoned");
                                guard.remove(&request_id);
                            }
                            let outcome = select_option(outcome, &options);
                            responder.respond(RequestPermissionResponse::new(outcome))
                        }
                    }) {
                        // Spawn failed: remove the entry we just inserted so the map
                        // does not leak a resolver that will never be fulfilled.
                        // NOTE: unresolved permission entries are otherwise reaped only
                        // when the session/connection tears down (no per-turn-cancel
                        // cleanup in v1).
                        perm_pending
                            .lock()
                            .expect("pending mutex poisoned")
                            .remove(&request_id_for_cleanup);
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
                .send_request(NewSessionRequest::new(
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
                ))
                .block_task()
                .await?;
            let acp_session_id = new_session.session_id;

            // Announce the session to the GUI.
            let _ = event_tx.unbounded_send(Event::AgentSpawned {
                session: Session {
                    id: SessionId(GUI_SESSION.into()),
                    name: agent_id.clone(),
                    tab: TabId("acp".into()),
                    harness: agent_id.clone(),
                    model: agent_id.clone(),
                    status: SessionStatus::Running,
                    render_mode: RenderMode::Acp,
                },
            });

            // ── Command loop ───────────────────────────────────────────────
            // NEVER blocks on a prompt turn: prompts are driven in spawned
            // tasks so the loop can still process `ResolvePending`.
            while let Some(cmd) = cmd_rx.next().await {
                match cmd {
                    Command::SendPrompt { text, .. } => {
                        // Drive the turn in its own task so the command loop stays
                        // responsive to `ResolvePending` mid-turn — see module docs.
                        let turn_connection = connection.clone();
                        let acp_session_id = acp_session_id.clone();
                        let turn_events = event_tx.clone();
                        connection.spawn(async move {
                            if let Err(e) = turn_connection
                                .send_request(PromptRequest::new(
                                    acp_session_id,
                                    vec![ContentBlock::Text(TextContent::new(text))],
                                ))
                                .block_task()
                                .await
                            {
                                let _ = turn_events.unbounded_send(Event::SessionUpdate {
                                    session: SessionId(GUI_SESSION.into()),
                                    update: SessionUpdateKind::Message {
                                        text: format!("prompt failed: {e}"),
                                    },
                                });
                            }
                            // Returning Err here would shut down the whole connection (SDK contract); surface it as a transcript message instead.
                            Ok(())
                        })?;
                    }
                    Command::ResolvePending {
                        request_id: Some(rid),
                        outcome,
                        ..
                    } => {
                        let sender = {
                            let mut guard = pending.lock().expect("pending mutex poisoned");
                            guard.remove(&rid)
                        };
                        if let Some(sender) = sender {
                            let _ = sender.send(outcome);
                        }
                    }
                    Command::ResolvePending {
                        request_id: None,
                        outcome,
                        ..
                    } => {
                        // No id: resolve the single in-flight request if there is
                        // exactly one (the GUI may omit the id for convenience).
                        let sender = {
                            let mut guard = pending.lock().expect("pending mutex poisoned");
                            let only = (guard.len() == 1)
                                .then(|| guard.keys().next().cloned())
                                .flatten();
                            only.and_then(|k| guard.remove(&k))
                        };
                        if let Some(sender) = sender {
                            let _ = sender.send(outcome);
                        }
                    }
                    Command::StopAgent { .. } => break,
                    // SpawnAgent is a no-op for a single-session feed.
                    Command::SpawnAgent { .. } => {}
                }
            }

            Ok(())
        })
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_builds_serve_command() {
        let feed = AcpFeed::new("bitrouter", "claude-code");
        assert_eq!(feed.agent_command, "bitrouter acp serve --agent claude-code");
    }
}
