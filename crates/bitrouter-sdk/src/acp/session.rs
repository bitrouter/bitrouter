//! Per-session identity for one ACP session.

/// Three-tier identity: `record_id` is the stable, manager-facing id — the
/// id the down-facing endpoint answers `session/new` with, minted locally and
/// unchanged for the life of the session;
/// `acp_session_id` is the ACP wire id from upstream `session/new`;
/// `agent_session_id` is the provider-native id from response `_meta.agentSessionId`
/// (optional, never synthesized).
#[derive(Debug, Clone)]
pub struct SessionState {
    /// Stable manager-facing session id, minted at launch.
    pub record_id: String,
    /// The configured agent id this session is pinned to (D8).
    pub agent_id: String,
    /// ACP wire session id from the upstream `session/new`; `None` until the
    /// session is opened.
    pub acp_session_id: Option<String>,
    /// Provider-native session id from `_meta.agentSessionId`, when the
    /// upstream exposes one.
    pub agent_session_id: Option<String>,
}

impl SessionState {
    /// Fresh identity for a session that has not opened upstream yet.
    pub fn new(record_id: String, agent_id: String) -> Self {
        Self {
            record_id,
            agent_id,
            acp_session_id: None,
            agent_session_id: None,
        }
    }

    /// Record the ACP wire id returned by the upstream `session/new`.
    pub fn set_acp_session_id(&mut self, id: String) {
        self.acp_session_id = Some(id);
    }

    /// Set only when the upstream exposes `_meta.agentSessionId`. Never synthesize.
    pub fn set_agent_session_id(&mut self, id: String) {
        self.agent_session_id = Some(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_defaults_then_sets() {
        let mut s = SessionState::new("rec-1".into(), "claude".into());
        assert!(s.acp_session_id.is_none() && s.agent_session_id.is_none());
        s.set_acp_session_id("u1".into());
        s.set_agent_session_id("prov-9".into());
        assert_eq!(s.acp_session_id.as_deref(), Some("u1"));
        assert_eq!(s.agent_session_id.as_deref(), Some("prov-9"));
    }
}
