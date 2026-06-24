use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TabId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderMode {
    Terminal,
    Acp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Spawning,
    Running,
    WaitingPermission,
    Idle,
    Errored,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub tab: TabId, // cohort/project label, used for sidebar grouping
    pub harness: String,
    pub model: String,
    pub status: SessionStatus,
    pub render_mode: RenderMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum Target {
    Session { id: SessionId },
    Selection { ids: Vec<SessionId> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    AllowOnce,
    AllowAlways,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    SpawnAgent {
        agent_id: String,
        model: String,
        worktree: Option<String>,
        tab: TabId,
        prompt: Option<String>,
    },
    StopAgent {
        target: Target,
    },
    SendPrompt {
        target: Target,
        text: String,
    },
    ResolvePending {
        target: Target,
        request_id: Option<String>,
        outcome: PermissionOutcome,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    Running,
    Ok,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "update", rename_all = "snake_case")]
pub enum SessionUpdateKind {
    /// A complete (non-streamed) assistant message — used by the mock feed.
    Message { text: String },
    /// A complete (non-streamed) thought — used by the mock feed.
    Thought { text: String },
    /// A streamed assistant-message delta. Chunks sharing a `message_id`
    /// coalesce into one transcript bubble.
    MessageChunk {
        message_id: Option<String>,
        text: String,
    },
    /// A streamed thought delta.
    ThoughtChunk {
        message_id: Option<String>,
        text: String,
    },
    /// A new tool call. `id` keys later `ToolCallUpdate`s.
    ToolCall {
        id: String,
        title: String,
        status: ToolStatus,
        diff: Option<String>,
    },
    /// An update to an existing tool call, addressed by `id`. Absent fields
    /// leave the prior value unchanged.
    ToolCallUpdate {
        id: String,
        status: Option<ToolStatus>,
        title: Option<String>,
        diff: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    pub asked: String,
    pub routed: String,
    pub rule: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    AgentSpawned {
        session: Session,
    },
    SessionUpdate {
        session: SessionId,
        update: SessionUpdateKind,
    },
    PermissionRequested {
        session: SessionId,
        request_id: String,
        summary: String,
        diff: Option<String>,
    },
    RequestCompleted {
        session: SessionId,
        model: String,
        prompt_tokens: u64,
        completion_tokens: u64,
        cost_micro_usd: u64,
        latency_ms: u32,
        failed_over: bool,
    },
    RoutingDecided {
        session: SessionId,
        route: Route,
    },
    AgentExited {
        session: SessionId,
        code: i32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_json_round_trip() -> anyhow::Result<()> {
        let cmd = Command::SpawnAgent {
            agent_id: "claude-code-acp".into(),
            model: "claude-opus-4".into(),
            worktree: None,
            tab: TabId("auth-feature".into()),
            prompt: Some("fix the token refresh race".into()),
        };
        let back: Command = serde_json::from_str(&serde_json::to_string(&cmd)?)?;
        assert_eq!(cmd, back);
        Ok(())
    }

    #[test]
    fn streaming_update_kinds_round_trip() -> anyhow::Result<()> {
        let kinds = vec![
            SessionUpdateKind::MessageChunk { message_id: Some("m1".into()), text: "hel".into() },
            SessionUpdateKind::ThoughtChunk { message_id: None, text: "hmm".into() },
            SessionUpdateKind::ToolCall {
                id: "t1".into(), title: "WRITE x".into(),
                status: ToolStatus::Pending, diff: None,
            },
            SessionUpdateKind::ToolCallUpdate {
                id: "t1".into(), status: Some(ToolStatus::Ok),
                title: None, diff: Some("x\n+++ new\nv".into()),
            },
        ];
        for k in kinds {
            let back: SessionUpdateKind = serde_json::from_str(&serde_json::to_string(&k)?)?;
            assert_eq!(k, back);
        }
        Ok(())
    }

    #[test]
    fn event_json_round_trip() -> anyhow::Result<()> {
        let ev = Event::RequestCompleted {
            session: SessionId("s1".into()),
            model: "qwen".into(),
            prompt_tokens: 12_000,
            completion_tokens: 3_000,
            cost_micro_usd: 1_100_000,
            latency_ms: 900,
            failed_over: false,
        };
        let back: Event = serde_json::from_str(&serde_json::to_string(&ev)?)?;
        assert_eq!(ev, back);
        Ok(())
    }
}
