use std::time::Instant;

use bitrouter_core::agents::event::AgentEvent;

use crate::model::{AgentStatus, EntryKind, ObsEvent, ObsEventKind, SessionId, SessionStatus};

use super::App;

impl App {
    pub(super) fn handle_session_event(
        &mut self,
        session_id: SessionId,
        agent_id: String,
        event: AgentEvent,
    ) {
        match event {
            AgentEvent::Disconnected => self.handle_session_disconnected(session_id, agent_id),
            AgentEvent::Error { message } => {
                self.handle_session_error(session_id, agent_id, message);
            }
            AgentEvent::MessageChunk { text } => {
                self.apply_agent_message_chunk(session_id, &agent_id, text);
            }
            AgentEvent::NonTextContent { description } => {
                self.apply_non_text_content(session_id, &agent_id, description);
            }
            AgentEvent::ThoughtChunk { text } => {
                self.apply_thought_chunk(session_id, &agent_id, text);
            }
            AgentEvent::ToolCall {
                tool_call_id,
                title,
                status,
            } => {
                self.apply_tool_call(session_id, &agent_id, tool_call_id, title, status);
            }
            AgentEvent::ToolCallUpdate {
                tool_call_id,
                title,
                status,
            } => {
                self.apply_tool_call_update(session_id, &agent_id, tool_call_id, title, status);
            }
            AgentEvent::PermissionRequest { id, request } => {
                self.handle_permission_request(session_id, id, request);
            }
            AgentEvent::TurnDone { .. } => {
                self.handle_prompt_done(session_id, agent_id);
            }
        }
    }

    pub(super) fn handle_session_connected(
        &mut self,
        session_id: SessionId,
        agent_id: String,
        acp_session_id: String,
    ) {
        if let Some(idx) = self.state.session_store.index_of(session_id) {
            let session = &mut self.state.session_store.active[idx];
            session.acp_session_id = Some(acp_session_id);
            session.status = SessionStatus::Connected;
        }
        if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            agent.status = AgentStatus::Connected;
        }
        if let Some(idx) = self.state.session_store.index_of(session_id) {
            self.push_system_msg_to_session(idx, &format!("Connected to {agent_id}"));
        }
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::Connected,
            timestamp: Instant::now(),
        });
    }

    fn handle_session_disconnected(&mut self, session_id: SessionId, agent_id: String) {
        self.set_session_status(session_id, SessionStatus::Disconnected);
        if let Some(idx) = self.state.session_store.index_of(session_id) {
            let sb = &mut self.state.session_store.active[idx].scrollback;
            sb.streaming_entry.remove(&agent_id);
        }

        // If this was the agent's last LIVE session, drop the provider
        // and bounce the agent's status back to Idle/Available. Sessions
        // already in Disconnected/Error don't keep the provider alive —
        // otherwise an agent crash that fans out to N sessions would
        // never trigger forget_provider (each disconnect sees the others
        // still in active, but already-dead).
        let still_has_sessions = self.state.session_store.active.iter().any(|s| {
            s.agent_id == agent_id
                && s.id != session_id
                && !matches!(
                    s.status,
                    SessionStatus::Disconnected | SessionStatus::Error(_)
                )
        });
        if !still_has_sessions {
            self.session_system.forget_provider(&agent_id);
            if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id)
                && !matches!(agent.status, AgentStatus::Error(_))
            {
                let on_path = agent
                    .config
                    .as_ref()
                    .map(|c| {
                        c.distribution.is_empty()
                            || std::env::var_os("PATH")
                                .and_then(|p| {
                                    std::env::split_paths(&p)
                                        .find(|dir| dir.join(&c.binary).is_file())
                                })
                                .is_some()
                    })
                    .unwrap_or(true);
                agent.status = if on_path {
                    AgentStatus::Idle
                } else {
                    AgentStatus::Available
                };
            }
        }

        if let Some(idx) = self.state.session_store.index_of(session_id) {
            self.push_system_msg_to_session(idx, &format!("Disconnected from {agent_id}"));
        }
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::Disconnected,
            timestamp: Instant::now(),
        });
    }

    fn handle_session_error(&mut self, session_id: SessionId, agent_id: String, message: String) {
        self.set_session_status(session_id, SessionStatus::Error(message.clone()));
        if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            agent.status = AgentStatus::Error(message.clone());
        }
        if let Some(idx) = self.state.session_store.index_of(session_id) {
            let sb = &mut self.state.session_store.active[idx].scrollback;
            sb.streaming_entry.remove(&agent_id);
            self.push_system_msg_to_session(idx, &format!("[{agent_id}] Error: {message}"));
        }
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::Error { message },
            timestamp: Instant::now(),
        });
    }

    pub(super) fn handle_prompt_done(&mut self, session_id: SessionId, agent_id: String) {
        if let Some(idx) = self.state.session_store.index_of(session_id) {
            let sb = &mut self.state.session_store.active[idx].scrollback;
            if let Some(entry_id) = sb.streaming_entry.remove(&agent_id)
                && let Some(eidx) = sb.index_of(entry_id)
            {
                match &mut sb.entries[eidx].kind {
                    EntryKind::AgentResponse(resp) => resp.is_streaming = false,
                    EntryKind::Thinking(th) => {
                        th.is_streaming = false;
                        sb.entries[eidx].collapsed = true;
                    }
                    _ => {}
                }
                sb.invalidate_entry(eidx);
            }
        }
        self.set_session_status(session_id, SessionStatus::Connected);
        // Reflect activity in the agent's aggregate status when no
        // other session is currently busy on this agent.
        let any_busy = self
            .state
            .session_store
            .active
            .iter()
            .any(|s| s.agent_id == agent_id && s.status == SessionStatus::Busy);
        if !any_busy
            && let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id)
            && matches!(agent.status, AgentStatus::Busy)
        {
            agent.status = AgentStatus::Connected;
        }
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::PromptDone,
            timestamp: Instant::now(),
        });
    }
}
