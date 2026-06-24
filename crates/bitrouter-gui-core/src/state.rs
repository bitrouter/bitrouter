use crate::protocol::{Event, Route, Session, SessionId, SessionStatus, SessionUpdateKind, ToolStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItem {
    Message { text: String },
    Thought { text: String },
    ToolCall { title: String, status: ToolStatus, diff: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission { pub request_id: String, pub summary: String, pub diff: Option<String> }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub session: Session,
    pub transcript: Vec<TranscriptItem>,
    pub pending: Option<Permission>,
    pub cost_micro_usd: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub last_route: Option<Route>,
    pub failovers: u32,
    pub latencies_ms: Vec<u32>,
}

impl SessionView {
    fn new(session: Session) -> Self {
        Self { session, transcript: Vec::new(), pending: None, cost_micro_usd: 0,
            tokens_in: 0, tokens_out: 0, last_route: None, failovers: 0, latencies_ms: Vec::new() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub sessions: Vec<SessionView>,
    pub focus: Option<SessionId>,
    pub selection: Vec<SessionId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hud {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub failovers: u32,
    pub p50_ms: Option<u32>,
    pub last_route: Option<Route>,
}

impl State {
    pub fn session(&self, id: &str) -> Option<&SessionView> {
        self.sessions.iter().find(|v| v.session.id.0 == id)
    }
    fn session_mut(&mut self, id: &SessionId) -> Option<&mut SessionView> {
        self.sessions.iter_mut().find(|v| &v.session.id == id)
    }
    pub fn session_cost_micro_usd(&self) -> u64 {
        self.sessions.iter().map(|v| v.cost_micro_usd).sum()
    }
    pub fn hud(&self) -> Hud {
        let mut lat: Vec<u32> = self.sessions.iter().flat_map(|v| v.latencies_ms.iter().copied()).collect();
        lat.sort_unstable();
        let p50 = if lat.is_empty() { None } else { Some(lat[lat.len() / 2]) };
        let route = self.focus.as_ref()
            .and_then(|id| self.sessions.iter().find(|v| &v.session.id == id))
            .and_then(|v| v.last_route.clone());
        Hud {
            tokens_in: self.sessions.iter().map(|v| v.tokens_in).sum(),
            tokens_out: self.sessions.iter().map(|v| v.tokens_out).sum(),
            failovers: self.sessions.iter().map(|v| v.failovers).sum(),
            p50_ms: p50,
            last_route: route,
        }
    }
}

pub fn reduce(state: &mut State, event: Event) {
    match event {
        Event::AgentSpawned { session } => {
            if state.focus.is_none() { state.focus = Some(session.id.clone()); }
            if state.session(&session.id.0).is_none() { state.sessions.push(SessionView::new(session)); }
        }
        Event::RequestCompleted { session, prompt_tokens, completion_tokens, cost_micro_usd, latency_ms, failed_over, .. } => {
            if let Some(v) = state.session_mut(&session) {
                v.cost_micro_usd += cost_micro_usd;
                v.tokens_in += prompt_tokens;
                v.tokens_out += completion_tokens;
                v.latencies_ms.push(latency_ms);
                if failed_over { v.failovers += 1; }
            }
        }
        Event::RoutingDecided { session, route } => {
            if let Some(v) = state.session_mut(&session) { v.last_route = Some(route); }
        }
        Event::AgentExited { session, .. } => {
            if let Some(v) = state.session_mut(&session) { v.session.status = SessionStatus::Exited; }
        }
        Event::SessionUpdate { session, update } => {
            if let Some(v) = state.session_mut(&session) {
                v.transcript.push(match update {
                    SessionUpdateKind::Message { text } => TranscriptItem::Message { text },
                    SessionUpdateKind::Thought { text } => TranscriptItem::Thought { text },
                    SessionUpdateKind::ToolCall { title, status, diff } => TranscriptItem::ToolCall { title, status, diff },
                });
            }
        }
        Event::PermissionRequested { session, request_id, summary, diff } => {
            if let Some(v) = state.session_mut(&session) {
                v.session.status = SessionStatus::WaitingPermission;
                v.pending = Some(Permission { request_id, summary, diff });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::*;

    fn sess(id: &str) -> Session {
        Session { id: SessionId(id.into()), name: id.into(), tab: TabId("t".into()),
            harness: "claude-code".into(), model: "claude".into(),
            status: SessionStatus::Running, render_mode: RenderMode::Terminal }
    }

    #[test]
    fn spawn_then_complete_accumulates() -> anyhow::Result<()> {
        let mut st = State::default();
        reduce(&mut st, Event::AgentSpawned { session: sess("s1") });
        assert_eq!(st.sessions.len(), 1);
        assert_eq!(st.focus, Some(SessionId("s1".into())));
        reduce(&mut st, Event::RequestCompleted { session: SessionId("s1".into()),
            model: "claude".into(), prompt_tokens: 10, completion_tokens: 5,
            cost_micro_usd: 4_000, latency_ms: 800, failed_over: true });
        let v = st.session("s1").ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!(v.cost_micro_usd, 4_000);
        assert_eq!(v.failovers, 1);
        assert_eq!(st.session_cost_micro_usd(), 4_000);
        Ok(())
    }

    #[test]
    fn hud_reports_p50() -> anyhow::Result<()> {
        let mut st = State::default();
        reduce(&mut st, Event::AgentSpawned { session: sess("s1") });
        for ms in [400u32, 800, 1200] {
            reduce(&mut st, Event::RequestCompleted { session: SessionId("s1".into()),
                model: "m".into(), prompt_tokens: 0, completion_tokens: 0,
                cost_micro_usd: 0, latency_ms: ms, failed_over: false });
        }
        assert_eq!(st.hud().p50_ms, Some(800));
        Ok(())
    }
}
