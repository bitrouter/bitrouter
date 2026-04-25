use crate::model::{ActivityEntry, ContentBlock, EntryKind, SystemNotice};

use super::App;

impl App {
    /// Push a system message to a specific session.
    pub(super) fn push_system_msg_to_session(&mut self, session_idx: usize, text: &str) {
        if let Some(session) = self.state.session_store.active.get_mut(session_idx) {
            let id = session.scrollback.next_id();
            session.scrollback.push_entry(ActivityEntry {
                id,
                kind: EntryKind::System(SystemNotice {
                    text: text.to_string(),
                }),
                collapsed: false,
            });
        }
    }

    /// Push a system message to the active session (no-op if no sessions).
    pub(super) fn push_system_msg(&mut self, text: &str) {
        let idx = self.state.active_session;
        self.push_system_msg_to_session(idx, text);
    }
}

/// Permission response choice (single key).
pub(super) enum PermissionChoice {
    Yes,
    No,
    Always,
}

/// Check if an entry's text content contains the query string.
pub(super) fn entry_contains_text(kind: &EntryKind, query: &str) -> bool {
    match kind {
        EntryKind::UserPrompt(p) => p.text.to_lowercase().contains(query),
        EntryKind::AgentResponse(r) => r.blocks.iter().any(|b| match b {
            ContentBlock::Text(t) => t.to_lowercase().contains(query),
            ContentBlock::Other(d) => d.to_lowercase().contains(query),
        }),
        EntryKind::ToolCall(tc) => tc.title.to_lowercase().contains(query),
        EntryKind::Thinking(th) => th.text.to_lowercase().contains(query),
        EntryKind::Permission(p) => p.request.title.to_lowercase().contains(query),
        EntryKind::System(s) => s.text.to_lowercase().contains(query),
        EntryKind::Separator(s) => s.label.to_lowercase().contains(query),
    }
}

/// Check if an agent requires a binary download before it can be launched.
///
/// Returns true only when the agent has no npx/uvx distribution —
/// i.e., the only fallback is a binary archive download.
pub(super) fn needs_binary_install(config: &bitrouter_config::AgentConfig) -> bool {
    use bitrouter_config::Distribution;

    for dist in &config.distribution {
        match dist {
            Distribution::Npx { .. } | Distribution::Uvx { .. } => return false,
            Distribution::Binary { .. } => {}
        }
    }
    config
        .distribution
        .iter()
        .any(|d| matches!(d, Distribution::Binary { .. }))
}
