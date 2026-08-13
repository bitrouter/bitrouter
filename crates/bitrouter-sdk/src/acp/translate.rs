//! Pure mapping from ACP schema types to the gateway's own event types.
//! No I/O — unit-tested without spawning a process.

use agent_client_protocol_schema::v1::{
    ContentBlock, PermissionOption, PermissionOptionKind, PlanEntryPriority, PlanEntryStatus,
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate, ToolCallContent,
    ToolCallStatus,
};

/// Tool execution status, mirroring `bitrouter_gui_core::protocol::ToolStatus`.
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

/// Which permission option the user selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// Allow this one request.
    AllowOnce,
    /// Allow this and every later request of the same shape.
    AllowAlways,
    /// Reject this request.
    Deny,
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

/// A gateway-local event produced from one ACP `SessionUpdate`.
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

/// Map one ACP `SessionUpdate` to a `SessionUpdateKind`.
///
/// Every **stable ACP v1** update is now carried. The catch-all previously
/// swallowed `Plan`, `AvailableCommandsUpdate`, `ConfigOptionUpdate`,
/// `CurrentModeUpdate`, and `SessionInfoUpdate`, so a manager reading the
/// translated stream could not show a plan or a command palette at all.
///
/// Note this is only the *translated* stream, which the NDJSON output and the
/// in-process renderer consume. Managers speaking ACP to `down.rs` already get
/// every variant verbatim off `Session::raw_updates`, which is the fidelity-
/// preserving path — nothing here reverse-maps onto the wire.
///
/// `UserMessageChunk` stays dropped, and now says so by name rather than by
/// falling into a catch-all: it is the manager's own prompt echoed back, and
/// forwarding it would duplicate every user turn in a transcript.
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
        // The manager's own prompt, echoed back. See the note above.
        SessionUpdate::UserMessageChunk(_) => None,
        // A variant this ACP version does not know. Managers that need it read
        // `Session::raw_updates`, which never drops anything.
        _ => None,
    }
}

/// Collapse ACP's three-state optional (absent / explicit null / value) to two.
///
/// The distinction is real on the wire — "leave it alone" versus "clear it" —
/// but a renderer showing a title treats both as "no title to show", and this
/// stream feeds renderers. A manager that needs the distinction reads
/// `Session::raw_updates`.
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

/// Choose the ACP permission option whose `kind` matches the desired outcome,
/// falling back to the first option, then to `Cancelled` if none exist.
pub fn select_option(
    outcome: PermissionOutcome,
    options: &[PermissionOption],
) -> RequestPermissionOutcome {
    let want = match outcome {
        PermissionOutcome::AllowOnce => PermissionOptionKind::AllowOnce,
        PermissionOutcome::AllowAlways => PermissionOptionKind::AllowAlways,
        PermissionOutcome::Deny => PermissionOptionKind::RejectOnce,
    };
    let chosen = options
        .iter()
        .find(|o| o.kind == want)
        .or_else(|| options.first());
    match chosen {
        Some(o) => {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(o.option_id.clone()))
        }
        None => RequestPermissionOutcome::Cancelled,
    }
}

/// Validate a manager's `RequestPermissionOutcome` against the option set
/// originally offered, preserving the **exact** selection.
///
/// `Cancelled` passes through. A `Selected` whose `option_id` is one of the
/// offered options passes through **verbatim** — the manager's choice is never
/// collapsed to an option kind, so two options of the same kind (e.g. "allow
/// this command" vs "allow all npm commands", both `allow_once`) stay
/// distinguishable. A `Selected` carrying an id we never offered is replaced by
/// the safe default, [`select_option`]`(Deny)`.
pub fn sanitize_selection(
    outcome: RequestPermissionOutcome,
    options: &[PermissionOption],
) -> RequestPermissionOutcome {
    match &outcome {
        RequestPermissionOutcome::Cancelled => outcome,
        RequestPermissionOutcome::Selected(selected)
            if options.iter().any(|o| o.option_id == selected.option_id) =>
        {
            outcome
        }
        _ => select_option(PermissionOutcome::Deny, options),
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v1::{
        ContentChunk, Diff, MessageId, PermissionOptionId, SelectedPermissionOutcome, TextContent,
        ToolCall, ToolCallId,
    };

    fn chunk(text: &str, mid: Option<&str>) -> ContentChunk {
        let mut c = ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())));
        if let Some(m) = mid {
            c = c.message_id(MessageId::new(m));
        }
        c
    }

    #[test]
    fn agent_message_chunk_maps_to_message_chunk() {
        let got = translate(SessionUpdate::AgentMessageChunk(chunk("hi", Some("m1"))));
        assert_eq!(
            got,
            Some(SessionUpdateKind::MessageChunk {
                message_id: Some("m1".into()),
                text: "hi".into(),
            })
        );
    }

    #[test]
    fn tool_call_maps_with_status_and_diff() {
        let tc = ToolCall::new(ToolCallId::new("t1"), "WRITE x")
            .status(ToolCallStatus::InProgress)
            .content(vec![ToolCallContent::Diff(
                Diff::new("x.rs", "b").old_text("a".to_string()),
            )]);
        let got = translate(SessionUpdate::ToolCall(tc));
        assert!(matches!(
            got,
            Some(SessionUpdateKind::ToolCall {
                status: ToolStatus::Running,
                diff: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn usage_update_maps_with_cost() {
        use agent_client_protocol_schema::v1::{Cost, UsageUpdate};
        let got = translate(SessionUpdate::UsageUpdate(
            UsageUpdate::new(1500, 200_000).cost(Cost::new(0.25, "USD")),
        ));
        assert_eq!(
            got,
            Some(SessionUpdateKind::Usage {
                used: 1500,
                size: 200_000,
                cost: Some(UsageCost {
                    amount: 0.25,
                    currency: "USD".into(),
                }),
            })
        );
    }

    /// The five updates the gateway used to swallow. Each is asserted
    /// separately so a regression names the variant it lost.
    ///
    /// ACP v1 spellings: the plan arrives whole as `Plan` (v2 splits it into
    /// `PlanUpdate`/`PlanRemoved`), and session state arrives as
    /// `CurrentModeUpdate` (v2 calls it `StateUpdate`). Targeting v1 means
    /// these are the variants that exist.
    #[test]
    fn a_plan_survives_translation() {
        use agent_client_protocol_schema::v1::{Plan, PlanEntry};
        let got = translate(SessionUpdate::Plan(Plan::new(vec![
            PlanEntry::new(
                "read the spec",
                PlanEntryPriority::High,
                PlanEntryStatus::Completed,
            ),
            PlanEntry::new(
                "write the code",
                PlanEntryPriority::Low,
                PlanEntryStatus::InProgress,
            ),
        ])));
        assert_eq!(
            got,
            Some(SessionUpdateKind::Plan {
                tasks: vec![
                    PlanTask {
                        content: "read the spec".into(),
                        status: "completed".into(),
                        priority: "high".into(),
                    },
                    PlanTask {
                        content: "write the code".into(),
                        status: "in_progress".into(),
                        priority: "low".into(),
                    },
                ],
            })
        );
    }

    #[test]
    fn available_commands_survive_translation() {
        use agent_client_protocol_schema::v1::{AvailableCommand, AvailableCommandsUpdate};
        let got = translate(SessionUpdate::AvailableCommandsUpdate(
            AvailableCommandsUpdate::new(vec![AvailableCommand::new(
                "create_plan",
                "draft a plan for the task",
            )]),
        ));
        assert_eq!(
            got,
            Some(SessionUpdateKind::AvailableCommands {
                commands: vec![AgentCommand {
                    name: "create_plan".into(),
                    description: "draft a plan for the task".into(),
                }],
            })
        );
    }

    #[test]
    fn config_options_survive_translation() {
        use agent_client_protocol_schema::v1::{
            ConfigOptionUpdate, SessionConfigId, SessionConfigOption, SessionConfigValueId,
        };
        let got = translate(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
            vec![
                SessionConfigOption::select(
                    SessionConfigId::new("model"),
                    "Model",
                    SessionConfigValueId::new("sonnet"),
                    Vec::<agent_client_protocol_schema::v1::SessionConfigSelectOption>::new(),
                )
                .description("which model serves this session".to_string()),
            ],
        )));
        assert_eq!(
            got,
            Some(SessionUpdateKind::ConfigOptions {
                options: vec![ConfigOption {
                    id: "model".into(),
                    name: "Model".into(),
                    description: Some("which model serves this session".into()),
                }],
            })
        );
    }

    #[test]
    fn a_mode_change_survives_translation() {
        use agent_client_protocol_schema::v1::{CurrentModeUpdate, SessionModeId};
        let got = translate(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
            SessionModeId::new("plan"),
        )));
        assert_eq!(
            got,
            Some(SessionUpdateKind::ModeChanged {
                mode_id: "plan".into(),
            })
        );
    }

    #[test]
    fn session_info_survives_translation() {
        use agent_client_protocol_schema::v1::SessionInfoUpdate;
        let got = translate(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title("refactor the parser".to_string()),
        ));
        assert_eq!(
            got,
            Some(SessionUpdateKind::SessionInfo {
                title: Some("refactor the parser".into()),
                // Absent on this update — and absent is not the same wire
                // state as an explicit null, which also lands here.
                updated_at: None,
            })
        );
    }

    #[test]
    fn ignored_variants_return_none() {
        assert_eq!(
            translate(SessionUpdate::UserMessageChunk(chunk("u", None))),
            None
        );
    }

    #[test]
    fn status_mapping_is_total() {
        assert_eq!(map_status(ToolCallStatus::Pending), ToolStatus::Pending);
        assert_eq!(map_status(ToolCallStatus::InProgress), ToolStatus::Running);
        assert_eq!(map_status(ToolCallStatus::Completed), ToolStatus::Ok);
        assert_eq!(map_status(ToolCallStatus::Failed), ToolStatus::Failed);
    }

    fn opt(kind: PermissionOptionKind, id: &str) -> PermissionOption {
        PermissionOption::new(PermissionOptionId::new(id), id, kind)
    }

    #[test]
    fn select_option_matches_kind_then_falls_back() {
        let opts = vec![
            opt(PermissionOptionKind::AllowOnce, "a1"),
            opt(PermissionOptionKind::RejectOnce, "r1"),
        ];
        match select_option(PermissionOutcome::Deny, &opts) {
            RequestPermissionOutcome::Selected(s) => assert_eq!(&*s.option_id.0, "r1"),
            _ => panic!("expected Selected"),
        }
    }

    fn selected_id(outcome: &RequestPermissionOutcome) -> Option<String> {
        match outcome {
            RequestPermissionOutcome::Selected(s) => Some(s.option_id.0.to_string()),
            _ => None,
        }
    }

    #[test]
    fn sanitize_selection_preserves_exact_known_id() {
        // Two options of the SAME kind: the exact id must survive, proving the
        // selection is never collapsed to a kind.
        let opts = vec![
            opt(PermissionOptionKind::AllowOnce, "a1"),
            opt(PermissionOptionKind::AllowOnce, "a2"),
            opt(PermissionOptionKind::RejectOnce, "r1"),
        ];
        let sel = |id: &str| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                PermissionOptionId::new(id),
            ))
        };
        assert_eq!(
            selected_id(&sanitize_selection(sel("a2"), &opts)).as_deref(),
            Some("a2")
        );
        assert_eq!(
            selected_id(&sanitize_selection(sel("r1"), &opts)).as_deref(),
            Some("r1")
        );
    }

    #[test]
    fn sanitize_selection_unknown_id_falls_back_to_deny_option() {
        let opts = vec![
            opt(PermissionOptionKind::AllowOnce, "a1"),
            opt(PermissionOptionKind::RejectOnce, "r1"),
        ];
        let sel = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new("nope"),
        ));
        // Unknown id → the reject option, never the fabricated id.
        assert_eq!(
            selected_id(&sanitize_selection(sel, &opts)).as_deref(),
            Some("r1")
        );
    }

    #[test]
    fn sanitize_selection_cancelled_passes_through() {
        let opts = vec![opt(PermissionOptionKind::AllowOnce, "a1")];
        assert_eq!(
            sanitize_selection(RequestPermissionOutcome::Cancelled, &opts),
            RequestPermissionOutcome::Cancelled
        );
    }
}
