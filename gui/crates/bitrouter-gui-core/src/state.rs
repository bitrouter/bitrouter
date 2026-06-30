use crate::protocol::{
    Event, Route, Session, SessionId, SessionStatus, SessionUpdateKind, ToolStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItem {
    Message {
        /// Coalescing key from ACP `ContentChunk.message_id`; `None` for
        /// non-streamed (mock) messages.
        message_id: Option<String>,
        text: String,
    },
    Thought {
        message_id: Option<String>,
        text: String,
    },
    ToolCall {
        id: String,
        title: String,
        status: ToolStatus,
        diff: Option<String>,
    },
    /// A prompt the user typed, echoed locally on send. Never produced by
    /// `reduce` from a feed `Event` — only by `AppModel::append_user_message`.
    UserPrompt {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission {
    pub request_id: String,
    pub summary: String,
    pub diff: Option<String>,
}

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
        Self {
            session,
            transcript: Vec::new(),
            pending: None,
            cost_micro_usd: 0,
            tokens_in: 0,
            tokens_out: 0,
            last_route: None,
            failovers: 0,
            latencies_ms: Vec::new(),
        }
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
        let mut lat: Vec<u32> = self
            .sessions
            .iter()
            .flat_map(|v| v.latencies_ms.iter().copied())
            .collect();
        lat.sort_unstable();
        let p50 = if lat.is_empty() {
            None
        } else {
            Some(lat[lat.len() / 2])
        };
        let route = self
            .focus
            .as_ref()
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
            if state.focus.is_none() {
                state.focus = Some(session.id.clone());
            }
            if state.session(&session.id.0).is_none() {
                state.sessions.push(SessionView::new(session));
            }
        }
        Event::RequestCompleted {
            session,
            prompt_tokens,
            completion_tokens,
            cost_micro_usd,
            latency_ms,
            failed_over,
            ..
        } => {
            if let Some(v) = state.session_mut(&session) {
                v.cost_micro_usd = v.cost_micro_usd.saturating_add(cost_micro_usd);
                v.tokens_in = v.tokens_in.saturating_add(prompt_tokens);
                v.tokens_out = v.tokens_out.saturating_add(completion_tokens);
                v.latencies_ms.push(latency_ms);
                if failed_over {
                    v.failovers = v.failovers.saturating_add(1);
                }
            }
        }
        Event::RoutingDecided { session, route } => {
            if let Some(v) = state.session_mut(&session) {
                v.last_route = Some(route);
            }
        }
        Event::AgentExited { session, .. } => {
            if let Some(v) = state.session_mut(&session) {
                v.session.status = SessionStatus::Exited;
            }
        }
        Event::SessionUpdate { session, update } => {
            if let Some(v) = state.session_mut(&session) {
                match update {
                    SessionUpdateKind::Message { text } => {
                        v.transcript.push(TranscriptItem::Message { message_id: None, text });
                    }
                    SessionUpdateKind::Thought { text } => {
                        v.transcript.push(TranscriptItem::Thought { message_id: None, text });
                    }
                    // Coalesce onto the trailing same-kind bubble when message_id matches.
                    // Two `None` ids match by design: an agent that streams chunks without
                    // message_ids still produces one bubble per turn — a new bubble starts only
                    // when message_id changes or a non-Message item (tool call, thought) breaks
                    // the run.
                    SessionUpdateKind::MessageChunk { message_id, text } => {
                        match v.transcript.last_mut() {
                            Some(TranscriptItem::Message { message_id: last, text: body })
                                if *last == message_id => body.push_str(&text),
                            _ => v.transcript.push(TranscriptItem::Message { message_id, text }),
                        }
                    }
                    // Same coalescing logic as MessageChunk — see comment above.
                    SessionUpdateKind::ThoughtChunk { message_id, text } => {
                        match v.transcript.last_mut() {
                            Some(TranscriptItem::Thought { message_id: last, text: body })
                                if *last == message_id => body.push_str(&text),
                            _ => v.transcript.push(TranscriptItem::Thought { message_id, text }),
                        }
                    }
                    SessionUpdateKind::ToolCall { id, title, status, diff } => {
                        v.transcript.push(TranscriptItem::ToolCall { id, title, status, diff });
                    }
                    SessionUpdateKind::ToolCallUpdate { id, status, title, diff } => {
                        if let Some(TranscriptItem::ToolCall {
                            title: t, status: s, diff: d, ..
                        }) = v.transcript.iter_mut().rev().find(
                            |it| matches!(it, TranscriptItem::ToolCall { id: tid, .. } if *tid == id),
                        ) {
                            if let Some(ns) = status { *s = ns; }
                            if let Some(nt) = title { *t = nt; }
                            if let Some(nd) = diff { *d = Some(nd); }
                        }
                        // Unknown id: no-op (mirrors unknown-session discipline).
                    }
                }
            }
        }
        Event::PermissionRequested {
            session,
            request_id,
            summary,
            diff,
        } => {
            if let Some(v) = state.session_mut(&session) {
                v.session.status = SessionStatus::WaitingPermission;
                v.pending = Some(Permission {
                    request_id,
                    summary,
                    diff,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::*;

    fn sess(id: &str) -> Session {
        Session {
            id: SessionId(id.into()),
            name: id.into(),
            tab: TabId("t".into()),
            harness: "claude-code".into(),
            model: "claude".into(),
            status: SessionStatus::Running,
            render_mode: RenderMode::Terminal,
        }
    }

    #[test]
    fn spawn_then_complete_accumulates() -> anyhow::Result<()> {
        let mut st = State::default();
        reduce(
            &mut st,
            Event::AgentSpawned {
                session: sess("s1"),
            },
        );
        assert_eq!(st.sessions.len(), 1);
        assert_eq!(st.focus, Some(SessionId("s1".into())));
        reduce(
            &mut st,
            Event::RequestCompleted {
                session: SessionId("s1".into()),
                model: "claude".into(),
                prompt_tokens: 10,
                completion_tokens: 5,
                cost_micro_usd: 4_000,
                latency_ms: 800,
                failed_over: true,
            },
        );
        let v = st.session("s1").ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!(v.cost_micro_usd, 4_000);
        assert_eq!(v.failovers, 1);
        assert_eq!(st.session_cost_micro_usd(), 4_000);
        Ok(())
    }

    #[test]
    fn hud_reports_p50() -> anyhow::Result<()> {
        let mut st = State::default();
        reduce(
            &mut st,
            Event::AgentSpawned {
                session: sess("s1"),
            },
        );
        for ms in [400u32, 800, 1200] {
            reduce(
                &mut st,
                Event::RequestCompleted {
                    session: SessionId("s1".into()),
                    model: "m".into(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cost_micro_usd: 0,
                    latency_ms: ms,
                    failed_over: false,
                },
            );
        }
        assert_eq!(st.hud().p50_ms, Some(800));
        Ok(())
    }

    #[test]
    fn dedup_spawn_and_ignore_unknown_session() -> anyhow::Result<()> {
        let mut st = State::default();
        reduce(
            &mut st,
            Event::AgentSpawned {
                session: sess("s1"),
            },
        );
        // A duplicate spawn must not add a second session or move focus.
        reduce(
            &mut st,
            Event::AgentSpawned {
                session: sess("s1"),
            },
        );
        assert_eq!(st.sessions.len(), 1);
        assert_eq!(st.focus, Some(SessionId("s1".into())));

        // An event for an unknown session is a no-op, not a panic.
        reduce(
            &mut st,
            Event::RequestCompleted {
                session: SessionId("ghost".into()),
                model: "m".into(),
                prompt_tokens: 1,
                completion_tokens: 1,
                cost_micro_usd: 1,
                latency_ms: 1,
                failed_over: true,
            },
        );
        assert_eq!(st.sessions.len(), 1);
        assert_eq!(st.session_cost_micro_usd(), 0);
        Ok(())
    }

    #[test]
    fn message_chunks_coalesce_by_message_id() -> anyhow::Result<()> {
        let mut st = State::default();
        reduce(&mut st, Event::AgentSpawned { session: sess("s1") });
        for part in ["Hel", "lo ", "world"] {
            reduce(&mut st, Event::SessionUpdate {
                session: SessionId("s1".into()),
                update: SessionUpdateKind::MessageChunk {
                    message_id: Some("m1".into()), text: part.into(),
                },
            });
        }
        let v = st.session("s1").ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!(v.transcript.len(), 1);
        assert!(matches!(&v.transcript[0],
            TranscriptItem::Message { text, .. } if text == "Hello world"));
        Ok(())
    }

    #[test]
    fn new_message_id_starts_new_bubble() -> anyhow::Result<()> {
        let mut st = State::default();
        reduce(&mut st, Event::AgentSpawned { session: sess("s1") });
        for (mid, t) in [("m1", "a"), ("m2", "b")] {
            reduce(&mut st, Event::SessionUpdate {
                session: SessionId("s1".into()),
                update: SessionUpdateKind::MessageChunk {
                    message_id: Some(mid.into()), text: t.into(),
                },
            });
        }
        let v = st.session("s1").ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!(v.transcript.len(), 2);
        Ok(())
    }

    #[test]
    fn tool_call_update_mutates_by_id() -> anyhow::Result<()> {
        let mut st = State::default();
        reduce(&mut st, Event::AgentSpawned { session: sess("s1") });
        reduce(&mut st, Event::SessionUpdate {
            session: SessionId("s1".into()),
            update: SessionUpdateKind::ToolCall {
                id: "t1".into(), title: "WRITE x".into(),
                status: ToolStatus::Pending, diff: None,
            },
        });
        reduce(&mut st, Event::SessionUpdate {
            session: SessionId("s1".into()),
            update: SessionUpdateKind::ToolCallUpdate {
                id: "t1".into(), status: Some(ToolStatus::Ok),
                title: None, diff: Some("d".into()),
            },
        });
        let v = st.session("s1").ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!(v.transcript.len(), 1);
        assert!(matches!(&v.transcript[0],
            TranscriptItem::ToolCall { status: ToolStatus::Ok, diff: Some(d), .. } if d == "d"));
        Ok(())
    }

    #[test]
    fn none_id_chunks_coalesce_into_one_bubble() -> anyhow::Result<()> {
        let mut st = State::default();
        reduce(&mut st, Event::AgentSpawned { session: sess("s1") });
        for part in ["a", "b", "c"] {
            reduce(&mut st, Event::SessionUpdate {
                session: SessionId("s1".into()),
                update: SessionUpdateKind::MessageChunk { message_id: None, text: part.into() },
            });
        }
        let v = st.session("s1").ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!(v.transcript.len(), 1);
        assert!(matches!(&v.transcript[0],
            TranscriptItem::Message { text, .. } if text == "abc"));
        Ok(())
    }

    #[test]
    fn tool_call_chunk_break_starts_new_message_bubble() -> anyhow::Result<()> {
        // A tool call between two None-id message chunks must break coalescing.
        let mut st = State::default();
        reduce(&mut st, Event::AgentSpawned { session: sess("s1") });
        reduce(&mut st, Event::SessionUpdate {
            session: SessionId("s1".into()),
            update: SessionUpdateKind::MessageChunk { message_id: None, text: "before".into() },
        });
        reduce(&mut st, Event::SessionUpdate {
            session: SessionId("s1".into()),
            update: SessionUpdateKind::ToolCall {
                id: "t1".into(), title: "x".into(), status: ToolStatus::Pending, diff: None,
            },
        });
        reduce(&mut st, Event::SessionUpdate {
            session: SessionId("s1".into()),
            update: SessionUpdateKind::MessageChunk { message_id: None, text: "after".into() },
        });
        let v = st.session("s1").ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!(v.transcript.len(), 3); // message, tool call, message
        Ok(())
    }

    #[test]
    fn tool_call_update_unknown_id_is_noop() -> anyhow::Result<()> {
        let mut st = State::default();
        reduce(&mut st, Event::AgentSpawned { session: sess("s1") });
        reduce(&mut st, Event::SessionUpdate {
            session: SessionId("s1".into()),
            update: SessionUpdateKind::ToolCallUpdate {
                id: "ghost".into(), status: Some(ToolStatus::Ok), title: None, diff: None,
            },
        });
        let v = st.session("s1").ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert!(v.transcript.is_empty());
        Ok(())
    }
}
