//! Human-readable projection of native tool facts; no execution authority.
use bitrouter_orchestrator::store::EffectStatus;
use bitrouter_sdk::language_model::{Content, Message, ToolResultOutput};
use bitrouter_tui::native_agent::{NativeEntryKind, NativeState, ToolStatus};

pub(super) fn tool_title(name: &str, arguments: &str) -> String {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok();
    let keys: &[&str] = match name {
        "shell" => &["command"],
        "grep" | "glob" => &["pattern", "path"],
        "read" | "write" | "edit" => &["path"],
        _ => &[
            "description",
            "task",
            "prompt",
            "query",
            "url",
            "path",
            "agent_id",
        ],
    };
    let detail = keys
        .iter()
        .filter_map(|key| value.as_ref()?.get(key)?.as_str())
        .collect::<Vec<_>>()
        .join(" · ");
    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name} {detail}")
    }
}

pub(super) fn tool_result(
    state: &NativeState,
    item_id: &str,
    message: &Message,
    effect: EffectStatus,
) -> (String, NativeEntryKind) {
    let result = message.content.iter().find_map(|part| match part {
        Content::ToolResult {
            tool_name, output, ..
        } => Some((tool_name, output)),
        _ => None,
    });
    let title = state
        .entries
        .iter()
        .find(|entry| entry.id == item_id)
        .map(|entry| entry.text.clone())
        .unwrap_or_else(|| {
            result
                .and_then(|(name, _)| name.clone())
                .unwrap_or_else(|| "Tool".into())
        });
    let (mut status, detail) = match result.map(|(_, output)| output) {
        Some(ToolResultOutput::ExecutionDenied { reason }) => {
            (ToolStatus::Denied, reason.clone().unwrap_or_default())
        }
        Some(ToolResultOutput::ErrorText { value }) => (ToolStatus::Failed, value.clone()),
        Some(ToolResultOutput::ErrorJson { value }) => (
            ToolStatus::Failed,
            value
                .get("message")
                .or_else(|| value.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("Tool returned an error")
                .into(),
        ),
        Some(ToolResultOutput::Json { value })
            if value.get("timed_out").and_then(|v| v.as_bool()) == Some(true) =>
        {
            (ToolStatus::Failed, "Timed out".into())
        }
        Some(ToolResultOutput::Json { value })
            if value
                .get("exit_status")
                .is_some_and(|code| code.as_i64() != Some(0)) =>
        {
            (
                ToolStatus::Failed,
                value
                    .get("exit_status")
                    .and_then(|code| code.as_i64())
                    .map(|code| format!("Exit {code}"))
                    .unwrap_or_else(|| "Terminated by signal".into()),
            )
        }
        Some(_) => (ToolStatus::Succeeded, String::new()),
        None => (ToolStatus::Unknown, "Result unavailable".into()),
    };
    if effect == EffectStatus::Unknown {
        status = ToolStatus::Unknown;
    } else if effect == EffectStatus::NotExecuted && status == ToolStatus::Succeeded {
        status = ToolStatus::Denied;
    }
    (title, NativeEntryKind::Tool { status, detail })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::language_model::Role;
    use serde_json::json;

    #[test]
    fn tool_results_keep_the_call_title_and_report_actual_outcomes() {
        let mut state = NativeState::default();
        let title = tool_title("shell", r#"{"command":"cargo test","timeout":30}"#);
        state.upsert(
            "call".into(),
            title.clone(),
            NativeEntryKind::Tool {
                status: ToolStatus::Running,
                detail: String::new(),
            },
        );
        let cases = [
            (
                ToolResultOutput::Json {
                    value: json!({"exit_status": 0, "stdout":"lots of output"}),
                },
                EffectStatus::Completed,
                ToolStatus::Succeeded,
            ),
            (
                ToolResultOutput::Json {
                    value: json!({"exit_status": 1}),
                },
                EffectStatus::Completed,
                ToolStatus::Failed,
            ),
            (
                ToolResultOutput::ErrorText {
                    value: "file unavailable".into(),
                },
                EffectStatus::NotExecuted,
                ToolStatus::Failed,
            ),
            (
                ToolResultOutput::ExecutionDenied {
                    reason: Some("user denied".into()),
                },
                EffectStatus::NotExecuted,
                ToolStatus::Denied,
            ),
            (
                ToolResultOutput::Json {
                    value: json!({"timed_out":true}),
                },
                EffectStatus::Unknown,
                ToolStatus::Unknown,
            ),
        ];
        for (output, effect, expected) in cases {
            let message = Message {
                role: Role::Tool,
                content: vec![Content::ToolResult {
                    call_id: "provider-id".into(),
                    tool_name: Some("shell".into()),
                    dynamic: false,
                    output,
                    provider_metadata: Default::default(),
                }],
            };
            let (text, kind) = tool_result(&state, "call", &message, effect);
            assert_eq!(text, "shell cargo test");
            assert!(matches!(&kind, NativeEntryKind::Tool { status, .. } if *status == expected));
            state.upsert("call".into(), text, kind);
            assert_eq!(state.entries.len(), 1);
        }
    }

    #[test]
    fn edit_summaries_show_the_target_without_dumping_file_contents() {
        assert_eq!(
            tool_title(
                "edit",
                r#"{"path":"src/main.rs","edits":[{"oldText":"before","newText":"after"}]}"#
            ),
            "edit src/main.rs"
        );
        assert_eq!(
            tool_title("grep", r#"{"pattern":"TODO","path":"src"}"#),
            "grep TODO · src"
        );
        assert_eq!(tool_title("mcp.custom", "{}"), "mcp.custom");
    }
}
