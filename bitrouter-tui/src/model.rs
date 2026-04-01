/// An agent harness that can be connected via ACP.
#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub status: AgentStatus,
}

/// Connection status of an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running,
    Error(String),
}

/// One conversation session with an agent.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub agent_id: String,
    pub messages: Vec<Message>,
}

impl Session {
    pub(crate) fn new(id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            agent_id: agent_id.into(),
            messages: Vec::new(),
        }
    }
}

/// A single message in a session.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
}

/// Who sent the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Agent,
    System,
}

/// A content block within a message. Aligns with ACP content block types.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    ToolCall {
        tool_name: String,
        status: ToolCallStatus,
        summary: String,
    },
}

/// Status of a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallStatus {
    Running,
    Done,
    Failed,
}

/// Create mock agents for Phase 1.
pub(crate) fn mock_agents() -> Vec<Agent> {
    vec![
        Agent {
            name: "claude-code".into(),
            status: AgentStatus::Idle,
        },
        Agent {
            name: "opencode".into(),
            status: AgentStatus::Running,
        },
        Agent {
            name: "openclaw".into(),
            status: AgentStatus::Error("disconnected".into()),
        },
    ]
}

/// Create a mock session with sample messages for Phase 1.
pub(crate) fn mock_session(id: &str, agent_id: &str) -> Session {
    let mut session = Session::new(id, agent_id);
    session.messages.push(Message {
        role: Role::System,
        blocks: vec![ContentBlock::Text("Session started.".into())],
    });
    session.messages.push(Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text(
            "Refactor the auth module into smaller files.".into(),
        )],
    });
    session.messages.push(Message {
        role: Role::Agent,
        blocks: vec![
            ContentBlock::Text("I'll start by reading the current module.".into()),
            ContentBlock::ToolCall {
                tool_name: "read_file".into(),
                status: ToolCallStatus::Done,
                summary: "src/auth/mod.rs (342 lines)".into(),
            },
            ContentBlock::ToolCall {
                tool_name: "write_file".into(),
                status: ToolCallStatus::Running,
                summary: "src/auth/session.rs".into(),
            },
            ContentBlock::Text(
                "Here's my plan: split into auth.rs, session.rs, and middleware.rs.".into(),
            ),
            ContentBlock::ToolCall {
                tool_name: "lint".into(),
                status: ToolCallStatus::Failed,
                summary: "2 errors found".into(),
            },
        ],
    });
    session
}
