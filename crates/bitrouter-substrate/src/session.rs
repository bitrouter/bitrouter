//! Per-session identity + status for one ACP session.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Spawning,
    Running,
    WaitingPermission,
    Idle,
    Errored,
    Exited,
}

/// Three-tier identity: `record_id` is the stable, manager-facing id;
/// `acp_session_id` is the ACP wire id from upstream `session/new`;
/// `agent_session_id` is the provider-native id from response `_meta.agentSessionId`
/// (optional, never synthesized).
#[derive(Debug, Clone)]
pub struct SessionState {
    pub record_id: String,
    pub agent_id: String,
    pub status: SessionStatus,
    pub acp_session_id: Option<String>,
    pub agent_session_id: Option<String>,
}

impl SessionState {
    pub fn new(record_id: String, agent_id: String) -> Self {
        Self {
            record_id,
            agent_id,
            status: SessionStatus::Spawning,
            acp_session_id: None,
            agent_session_id: None,
        }
    }

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
        assert_eq!(s.status, SessionStatus::Spawning);
        assert!(s.acp_session_id.is_none() && s.agent_session_id.is_none());
        s.set_acp_session_id("u1".into());
        s.set_agent_session_id("prov-9".into());
        s.status = SessionStatus::Running;
        assert_eq!(s.acp_session_id.as_deref(), Some("u1"));
        assert_eq!(s.agent_session_id.as_deref(), Some("prov-9"));
    }
}
