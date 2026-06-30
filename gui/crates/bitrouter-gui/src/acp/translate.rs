//! Pure mapping from ACP schema types to `bitrouter_gui_core` protocol types.
//! No I/O — unit-tested without spawning a process.

use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    SelectedPermissionOutcome, SessionUpdate, ToolCallContent, ToolCallStatus,
};
use bitrouter_gui_core::protocol::{PermissionOutcome, SessionUpdateKind, ToolStatus};

/// Map one ACP `SessionUpdate` to a core `SessionUpdateKind`. Variants the GUI
/// does not render in v1 (`UserMessageChunk`, `Plan`, `UsageUpdate`, …) → `None`.
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
        _ => None,
    }
}

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

/// Choose the ACP permission option whose `kind` matches the GUI outcome,
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
    let chosen = options.iter().find(|o| o.kind == want).or_else(|| options.first());
    match chosen {
        Some(o) => RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(o.option_id.clone()),
        ),
        None => RequestPermissionOutcome::Cancelled,
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
            Some(format!("{}\n[old]\n{}\n[new]\n{}", d.path.display(), old, d.new_text))
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentChunk, Diff, MessageId, PermissionOptionId, TextContent, ToolCall, ToolCallId,
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
    fn ignored_variants_return_none() {
        assert_eq!(translate(SessionUpdate::UserMessageChunk(chunk("u", None))), None);
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
}
