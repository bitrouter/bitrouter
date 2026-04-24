use std::time::Instant;

use bitrouter_core::agents::event::AgentEvent;

use crate::model::{AgentStatus, EntryKind, ObsEvent, ObsEventKind};

use super::App;

impl App {
    pub(super) fn handle_agent_event(&mut self, agent_id: String, event: AgentEvent) {
        match event {
            AgentEvent::Disconnected => self.handle_agent_disconnected(agent_id),
            AgentEvent::Error { message } => {
                self.handle_agent_error(agent_id, message);
            }
            AgentEvent::MessageChunk { text } => {
                self.apply_agent_message_chunk(&agent_id, text);
            }
            AgentEvent::NonTextContent { description } => {
                self.apply_non_text_content(&agent_id, description);
            }
            AgentEvent::ThoughtChunk { text } => {
                self.apply_thought_chunk(&agent_id, text);
            }
            AgentEvent::ToolCall {
                tool_call_id,
                title,
                status,
            } => {
                self.apply_tool_call(&agent_id, tool_call_id, title, status);
            }
            AgentEvent::ToolCallUpdate {
                tool_call_id,
                title,
                status,
            } => {
                self.apply_tool_call_update(&agent_id, tool_call_id, title, status);
            }
            AgentEvent::PermissionRequest { id, request } => {
                self.handle_permission_request(agent_id, id, request);
            }
            AgentEvent::TurnDone { .. } => {
                self.handle_prompt_done(agent_id);
            }
        }
    }

    pub(super) fn handle_agent_connected(&mut self, agent_id: String, session_id: String) {
        if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            agent.status = AgentStatus::Connected;
            agent.session_id = Some(session_id);
        }
        let session_idx = self.ensure_session_for_agent(&agent_id);
        self.push_system_msg_to_session(session_idx, &format!("Connected to {agent_id}"));
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::Connected,
            timestamp: Instant::now(),
        });
    }

    fn handle_agent_disconnected(&mut self, agent_id: String) {
        // Clean up provider handle.
        self.session_system.forget(&agent_id);
        if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            // Only reset status if not already in Error state.
            if !matches!(agent.status, AgentStatus::Error(_)) {
                // Agents without a binary on PATH go back to Available.
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
            agent.session_id = None;
        }
        // Clear streaming cursor for this agent.
        if let Some(sb) = self.scrollback_for_agent(&agent_id) {
            sb.streaming_entry.remove(&agent_id);
        }

        if let Some(session_idx) = self.session_for_agent(&agent_id) {
            self.push_system_msg_to_session(session_idx, &format!("Disconnected from {agent_id}"));
        }
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::Disconnected,
            timestamp: Instant::now(),
        });
    }

    fn handle_agent_error(&mut self, agent_id: String, message: String) {
        if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            agent.status = AgentStatus::Error(message.clone());
        }
        if let Some(sb) = self.scrollback_for_agent(&agent_id) {
            sb.streaming_entry.remove(&agent_id);
        }
        let session_idx = self.ensure_session_for_agent(&agent_id);
        self.push_system_msg_to_session(session_idx, &format!("[{agent_id}] Error: {message}"));
        self.state.obs_log.push(ObsEvent {
            agent_id,
            kind: ObsEventKind::Error { message },
            timestamp: Instant::now(),
        });
    }

    pub(super) fn handle_prompt_done(&mut self, agent_id: String) {
        if let Some(sb) = self.scrollback_for_agent(&agent_id) {
            // Mark the streaming entry as complete.
            if let Some(entry_id) = sb.streaming_entry.remove(&agent_id)
                && let Some(idx) = sb.index_of(entry_id)
            {
                match &mut sb.entries[idx].kind {
                    EntryKind::AgentResponse(resp) => resp.is_streaming = false,
                    EntryKind::Thinking(th) => {
                        th.is_streaming = false;
                        // Auto-collapse completed thinking entries.
                        sb.entries[idx].collapsed = true;
                    }
                    _ => {}
                }
                sb.invalidate_entry(idx);
            }
        }
        // Update agent status.
        if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == agent_id)
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
