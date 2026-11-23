//! Pure mapping from ACP schema types to BRO's headless NDJSON events.
//! No I/O — unit-tested without spawning a process.

use agent_client_protocol_schema::v1::{
    AvailableCommandInput, ContentBlock, PlanEntryPriority, PlanEntryStatus, SessionUpdate,
    ToolCallContent, ToolCallStatus,
};

/// Tool execution status in the headless output.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Accepted by the agent, not started yet.
    Pending,
    /// Currently executing.
    Running,
    /// Finished successfully.
    Ok,
    /// Finished with an error — also the mapping for any status this ACP
    /// version does not know, so a future state is never read as not-started.
    Failed,
}

/// Cumulative session cost as reported by the upstream's `UsageUpdate`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct UsageCost {
    /// Money spent so far this session, in [`Self::currency`].
    pub amount: f64,
    /// ISO-4217 currency code the upstream reported (e.g. `"USD"`).
    pub currency: String,
}

/// One task in the agent's execution plan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PlanTask {
    /// Human-readable description of what this task aims to accomplish.
    pub content: String,
    /// `pending` / `in_progress` / `completed`, or the raw wire value for a
    /// status this ACP version does not know.
    pub status: String,
    /// `high` / `medium` / `low`, or the raw wire value for an unknown priority.
    pub priority: String,
}

/// One command the agent advertises as runnable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AgentCommand {
    /// Command name, e.g. `create_plan`.
    pub name: String,
    /// Human-readable description of what the command does.
    pub description: String,
    /// What to type after the name, when the command takes an argument.
    ///
    /// ACP carries this inside `input`; it is flattened here so an NDJSON
    /// consumer sees it without having to know the input union's shape.
    /// `None` when the command takes nothing, or when a future input variant
    /// this build does not know about carries no hint.
    pub hint: Option<String>,
}

/// One session configuration option and the value it currently holds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConfigOption {
    /// Stable identifier for the option.
    pub id: String,
    /// Human-readable label.
    pub name: String,
    /// Optional longer description for display.
    pub description: Option<String>,
}

/// A headless BRO event produced from one ACP `SessionUpdate`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionUpdateKind {
    /// A chunk of the agent's streamed answer.
    MessageChunk {
        /// The message this chunk belongs to, when the agent tags chunks.
        message_id: Option<String>,
        /// The chunk's text (non-text content blocks render empty).
        text: String,
    },
    /// A chunk of the agent's streamed reasoning.
    ThoughtChunk {
        /// The message this chunk belongs to, when the agent tags chunks.
        message_id: Option<String>,
        /// The chunk's text (non-text content blocks render empty).
        text: String,
    },
    /// The agent started a tool call.
    ToolCall {
        /// The ACP tool-call id.
        id: String,
        /// Human-readable title of what the tool is doing.
        title: String,
        /// Execution status at the time of the update.
        status: ToolStatus,
        /// The call's first diff, rendered readable, when it carries one.
        diff: Option<String>,
    },
    /// An update to an in-flight tool call; every field but the id is optional
    /// because ACP sends only what changed.
    ToolCallUpdate {
        /// The ACP tool-call id being updated.
        id: String,
        /// New execution status, when it changed.
        status: Option<ToolStatus>,
        /// New title, when it changed.
        title: Option<String>,
        /// The call's first diff, rendered readable, when it carries one.
        diff: Option<String>,
    },
    /// The agent's execution plan, replaced wholesale on every update — ACP
    /// sends the complete entry list each time, not a delta.
    ///
    /// This is `SessionUpdate::Plan`, ACP **v1**'s spelling. The v2 vocabulary
    /// splits it into incremental `PlanUpdate` / `PlanRemoved` messages, which
    /// v1 exposes only behind the `unstable_plan_operations` feature. Targeting
    /// v1 means the whole-plan form is the one that exists.
    Plan {
        /// Every task in the plan, in the order the agent listed them.
        tasks: Vec<PlanTask>,
    },
    /// The set of commands the agent can run has been established or changed.
    AvailableCommands {
        /// The full command set, replacing any previous one.
        commands: Vec<AgentCommand>,
    },
    /// The session's configuration options and their current values changed.
    ConfigOptions {
        /// The full option set, replacing any previous one.
        options: Vec<ConfigOption>,
    },
    /// The session switched mode (ACP v1's session-state signal; v2 calls the
    /// equivalent notion `StateUpdate`).
    ModeChanged {
        /// Id of the mode now in force.
        mode_id: String,
    },
    /// Session metadata changed. Both fields are `None` when this particular
    /// update did not carry them — ACP distinguishes "absent" from "cleared",
    /// and this type does not, because a renderer treats them alike.
    SessionInfo {
        /// New human-readable session title, when the update carried one.
        title: Option<String>,
        /// ISO-8601 timestamp of last activity, when the update carried one.
        updated_at: Option<String>,
    },
    /// Context-window occupancy (+ optional cumulative cost) from the
    /// upstream's `UsageUpdate`. ACP's stable usage signal reports tokens *in
    /// context* (`used`/`size`), not per-turn input/output deltas.
    Usage {
        /// Tokens currently in context.
        used: u64,
        /// Total context-window size in tokens.
        size: u64,
        /// Cumulative session cost, when the upstream reports one.
        cost: Option<UsageCost>,
    },
}

/// Map a stable ACP v1 session update into the headless event format.
pub fn translate(update: SessionUpdate) -> Option<SessionUpdateKind> {
    match update {
        SessionUpdate::AgentMessageChunk(c) => Some(SessionUpdateKind::MessageChunk {
            message_id: c.message_id.map(|m| m.0.to_string()),
            text: block_text(&c.content),
        }),
        SessionUpdate::AgentThoughtChunk(c) => Some(SessionUpdateKind::ThoughtChunk {
            message_id: c.message_id.map(|m| m.0.to_string()),
            text: block_text(&c.content),
        }),
        SessionUpdate::ToolCall(tc) => Some(SessionUpdateKind::ToolCall {
            id: tc.tool_call_id.0.to_string(),
            title: tc.title,
            status: map_status(tc.status),
            diff: render_diff(&tc.content),
        }),
        SessionUpdate::ToolCallUpdate(u) => Some(SessionUpdateKind::ToolCallUpdate {
            id: u.tool_call_id.0.to_string(),
            status: u.fields.status.map(map_status),
            title: u.fields.title,
            diff: u.fields.content.as_deref().and_then(render_diff),
        }),
        SessionUpdate::UsageUpdate(u) => Some(SessionUpdateKind::Usage {
            used: u.used,
            size: u.size,
            cost: u.cost.map(|c| UsageCost {
                amount: c.amount,
                currency: c.currency,
            }),
        }),
        SessionUpdate::Plan(p) => Some(SessionUpdateKind::Plan {
            tasks: p
                .entries
                .into_iter()
                .map(|e| PlanTask {
                    content: e.content,
                    status: plan_status(&e.status).to_string(),
                    priority: plan_priority(&e.priority).to_string(),
                })
                .collect(),
        }),
        SessionUpdate::AvailableCommandsUpdate(u) => Some(SessionUpdateKind::AvailableCommands {
            commands: u
                .available_commands
                .into_iter()
                .map(|c| AgentCommand {
                    name: c.name,
                    description: c.description,
                    // `AvailableCommandInput` is `#[non_exhaustive]`: a variant
                    // this build does not know yields no hint rather than
                    // failing to compile against a newer schema.
                    hint: c.input.and_then(|input| match input {
                        AvailableCommandInput::Unstructured(unstructured) => {
                            Some(unstructured.hint)
                        }
                        _ => None,
                    }),
                })
                .collect(),
        }),
        SessionUpdate::ConfigOptionUpdate(u) => Some(SessionUpdateKind::ConfigOptions {
            options: u
                .config_options
                .into_iter()
                .map(|o| ConfigOption {
                    id: o.id.0.to_string(),
                    name: o.name,
                    description: o.description,
                })
                .collect(),
        }),
        SessionUpdate::CurrentModeUpdate(u) => Some(SessionUpdateKind::ModeChanged {
            mode_id: u.current_mode_id.0.to_string(),
        }),
        SessionUpdate::SessionInfoUpdate(u) => Some(SessionUpdateKind::SessionInfo {
            title: maybe_string(u.title),
            updated_at: maybe_string(u.updated_at),
        }),
        // The manager's own prompt echoed back.
        SessionUpdate::UserMessageChunk(_) => None,
        // Future update kinds have no corresponding headless event yet.
        _ => None,
    }
}

/// Collapse ACP's three-state optional (absent / explicit null / value) to two.
///
/// The distinction is real on the wire — "leave it alone" versus "clear it" —
/// but a renderer showing a title treats both as "no title to show", and this
/// stream feeds renderers.
fn maybe_string(field: agent_client_protocol_schema::MaybeUndefined<String>) -> Option<String> {
    match field {
        agent_client_protocol_schema::MaybeUndefined::Value(v) => Some(v),
        _ => None,
    }
}

/// Wire spelling of a plan entry's status, passing an unknown future value
/// through rather than collapsing it onto a status that means something else.
fn plan_status(status: &PlanEntryStatus) -> &str {
    match status {
        PlanEntryStatus::Pending => "pending",
        PlanEntryStatus::InProgress => "in_progress",
        PlanEntryStatus::Completed => "completed",
        _ => "unknown",
    }
}

/// Wire spelling of a plan entry's priority, with the same unknown-value rule
/// as [`plan_status`].
fn plan_priority(priority: &PlanEntryPriority) -> &str {
    match priority {
        PlanEntryPriority::High => "high",
        PlanEntryPriority::Medium => "medium",
        PlanEntryPriority::Low => "low",
        _ => "unknown",
    }
}

/// Map an ACP `ToolCallStatus` to [`ToolStatus`].
pub fn map_status(s: ToolCallStatus) -> ToolStatus {
    match s {
        ToolCallStatus::Pending => ToolStatus::Pending,
        ToolCallStatus::InProgress => ToolStatus::Running,
        ToolCallStatus::Completed => ToolStatus::Ok,
        ToolCallStatus::Failed => ToolStatus::Failed,
        // Unknown future status: surface as Failed rather than masking it as not-started.
        _ => ToolStatus::Failed,
    }
}

fn block_text(b: &ContentBlock) -> String {
    match b {
        ContentBlock::Text(t) => t.text.clone(),
        _ => String::new(),
    }
}

/// Render the first diff in a tool call's content as a readable string.
pub fn render_diff(content: &[ToolCallContent]) -> Option<String> {
    content.iter().find_map(|c| match c {
        ToolCallContent::Diff(d) => {
            let old = d.old_text.clone().unwrap_or_default();
            Some(format!(
                "{}\n[old]\n{}\n[new]\n{}",
                d.path.display(),
                old,
                d.new_text
            ))
        }
        _ => None,
    })
}
