use std::time::Instant;

use bitrouter_core::agents::event::ToolCallStatus;

use crate::model::{
    ActivityEntry, AgentResponse, ContentBlock, EntryKind, ObsEvent, ObsEventKind, ScrollbackState,
    ThinkingEntry, ToolCallEntry,
};

use super::App;

impl App {
    pub(super) fn apply_agent_message_chunk(&mut self, agent_id: &str, text: String) {
        self.badge_background_tab(agent_id);
        let tab_idx = self.ensure_tab(agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        // Try to extend existing streaming entry for this agent.
        if let Some(&entry_id) = sb.streaming_entry.get(agent_id)
            && let Some(idx) = sb.index_of(entry_id)
            && let EntryKind::AgentResponse(resp) = &mut sb.entries[idx].kind
        {
            // Extend last text block or push new one.
            if let Some(ContentBlock::Text(existing)) = resp.blocks.last_mut() {
                existing.push_str(&text);
            } else {
                resp.blocks.push(ContentBlock::Text(text));
            }
            sb.invalidate_entry(idx);
            return;
        }

        // Finalize any previous streaming entry before starting new.
        Self::finalize_streaming_in(sb, agent_id);

        // Start a new agent message entry.
        let id = sb.next_id();
        sb.push_entry(ActivityEntry {
            id,
            kind: EntryKind::AgentResponse(AgentResponse {
                agent_id: agent_id.to_string(),
                blocks: vec![ContentBlock::Text(text)],
                is_streaming: true,
            }),
            collapsed: false,
        });
        sb.streaming_entry.insert(agent_id.to_string(), id);
    }

    pub(super) fn apply_non_text_content(&mut self, agent_id: &str, desc: String) {
        self.badge_background_tab(agent_id);
        let tab_idx = self.ensure_tab(agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        // Append as an Other block to the current streaming entry, or create new.
        if let Some(&entry_id) = sb.streaming_entry.get(agent_id)
            && let Some(idx) = sb.index_of(entry_id)
            && let EntryKind::AgentResponse(resp) = &mut sb.entries[idx].kind
        {
            resp.blocks.push(ContentBlock::Other(desc));
            sb.invalidate_entry(idx);
            return;
        }

        let id = sb.next_id();
        sb.push_entry(ActivityEntry {
            id,
            kind: EntryKind::AgentResponse(AgentResponse {
                agent_id: agent_id.to_string(),
                blocks: vec![ContentBlock::Other(desc)],
                is_streaming: true,
            }),
            collapsed: false,
        });
        sb.streaming_entry.insert(agent_id.to_string(), id);
    }

    pub(super) fn apply_thought_chunk(&mut self, agent_id: &str, text: String) {
        self.badge_background_tab(agent_id);
        let tab_idx = self.ensure_tab(agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        // Try to extend existing streaming thinking entry.
        if let Some(&entry_id) = sb.streaming_entry.get(agent_id)
            && let Some(idx) = sb.index_of(entry_id)
            && let EntryKind::Thinking(th) = &mut sb.entries[idx].kind
            && th.is_streaming
        {
            th.text.push_str(&text);
            sb.invalidate_entry(idx);
            return;
        }

        // Finalize any previous streaming entry before starting new.
        Self::finalize_streaming_in(sb, agent_id);

        let id = sb.next_id();
        sb.push_entry(ActivityEntry {
            id,
            kind: EntryKind::Thinking(ThinkingEntry {
                agent_id: agent_id.to_string(),
                text,
                is_streaming: true,
            }),
            collapsed: false,
        });
        sb.streaming_entry.insert(agent_id.to_string(), id);
    }

    pub(super) fn apply_tool_call(
        &mut self,
        agent_id: &str,
        tool_call_id: String,
        title: String,
        status: ToolCallStatus,
    ) {
        self.badge_background_tab(agent_id);
        let tab_idx = self.ensure_tab(agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        let id = sb.next_id();
        sb.push_entry(ActivityEntry {
            id,
            kind: EntryKind::ToolCall(ToolCallEntry {
                agent_id: agent_id.to_string(),
                tool_call_id,
                title: title.clone(),
                status,
            }),
            collapsed: false,
        });
        // Tool calls break the streaming cursor — next message chunk starts fresh.
        sb.streaming_entry.remove(agent_id);

        self.state.obs_log.push(ObsEvent {
            agent_id: agent_id.to_string(),
            kind: ObsEventKind::ToolCall { title },
            timestamp: Instant::now(),
        });
    }

    pub(super) fn apply_tool_call_update(
        &mut self,
        agent_id: &str,
        tool_call_id: String,
        new_title: Option<String>,
        new_status: Option<ToolCallStatus>,
    ) {
        let tab_idx = self.ensure_tab(agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        // Find the tool call entry by ID and update it.
        for (idx, entry) in sb.entries.iter_mut().enumerate().rev() {
            if let EntryKind::ToolCall(tc) = &mut entry.kind
                && tc.agent_id == agent_id
                && tc.tool_call_id == tool_call_id
            {
                if let Some(t) = &new_title {
                    tc.title = t.clone();
                }
                if let Some(s) = new_status {
                    tc.status = s;
                    // Auto-collapse completed/failed tool calls.
                    if matches!(s, ToolCallStatus::Completed | ToolCallStatus::Failed) {
                        entry.collapsed = true;
                    }
                }
                sb.invalidate_entry(idx);
                return;
            }
        }

        // If not found, create from update (fallback).
        self.apply_tool_call(
            agent_id,
            tool_call_id,
            new_title.unwrap_or_default(),
            new_status.unwrap_or(ToolCallStatus::InProgress),
        );
    }

    /// Mark the current streaming entry for an agent as no longer streaming.
    pub(super) fn finalize_streaming_in(sb: &mut ScrollbackState, agent_id: &str) {
        if let Some(&old_id) = sb.streaming_entry.get(agent_id)
            && let Some(idx) = sb.index_of(old_id)
        {
            match &mut sb.entries[idx].kind {
                EntryKind::AgentResponse(resp) => resp.is_streaming = false,
                EntryKind::Thinking(th) => th.is_streaming = false,
                _ => {}
            }
            sb.invalidate_entry(idx);
        }
    }
}
