//! Authenticate a visible continuation prefix only with complete causal history.

use bitrouter_ai::types::{Content, Prompt, Role};

fn has_complete_visible_causal_history(prompt: &Prompt) -> bool {
    let mut unmatched_calls = Vec::<String>::new();
    let mut completed_assistant_message = None;
    let mut invalid_result = false;
    for (message_index, message) in prompt.messages.iter().enumerate() {
        for content in &message.content {
            match content {
                Content::Text { text, .. }
                    if message.role == Role::Assistant && !text.trim().is_empty() =>
                {
                    completed_assistant_message = Some(message_index);
                }
                Content::ToolCall { id, .. } if message.role == Role::Assistant => {
                    unmatched_calls.push(id.clone());
                }
                Content::ToolCall { .. } => invalid_result = true,
                Content::ToolResult { call_id, .. } => {
                    if message.role != Role::Tool {
                        invalid_result = true;
                        continue;
                    }
                    let Some(position) = unmatched_calls.iter().rposition(|id| id == call_id)
                    else {
                        invalid_result = true;
                        continue;
                    };
                    unmatched_calls.remove(position);
                    completed_assistant_message = Some(message_index);
                }
                _ => {}
            }
        }
    }
    prompt
        .messages
        .iter()
        .find(|message| message.role != Role::System)
        .map(|message| message.role)
        == Some(Role::User)
        && !invalid_result
        && unmatched_calls.is_empty()
        && completed_assistant_message.is_some_and(|assistant_index| {
            prompt.messages[assistant_index.saturating_add(1)..]
                .iter()
                .any(|message| message.role == Role::User)
        })
}

pub(super) fn authenticated_visible_causal_prefix(
    prompt: &Prompt,
    expected: &crate::gateway::model::protocol::responses::CausalPrefixCommitment,
) -> Option<usize> {
    if !has_complete_visible_causal_history(prompt) {
        return None;
    }
    crate::gateway::model::protocol::responses::find_unique_causal_prefix(
        &prompt.messages,
        expected,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_ai::types::{Message, ToolResultOutput};

    #[test]
    fn complete_history_requires_a_user_root_paired_tools_and_a_new_user_turn() -> anyhow::Result<()>
    {
        let mut prompt: Prompt = serde_json::from_value(
            serde_json::json!({"model":"fixture","messages":[],"stream":false}),
        )?;
        let user = Message::text(Role::User, "Inspect the source");
        let assistant = Message::text(Role::Assistant, "Here is the answer");
        let followup = Message::text(Role::User, "Continue");
        let call = Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall {
                id: "read".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
                provider_executed: false,
                dynamic: false,
                provider_metadata: Default::default(),
            }],
        };
        let result = Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                call_id: "read".into(),
                tool_name: None,
                output: ToolResultOutput::Text {
                    value: "source".into(),
                },
                dynamic: false,
                provider_metadata: Default::default(),
            }],
        };
        for messages in [
            vec![user.clone(), assistant.clone(), followup.clone()],
            vec![user.clone(), call.clone(), result.clone(), followup.clone()],
        ] {
            prompt.messages = messages;
            assert!(has_complete_visible_causal_history(&prompt));
            prompt
                .messages
                .insert(0, Message::text(Role::System, "instructions"));
            assert!(has_complete_visible_causal_history(&prompt));
        }
        for messages in [
            vec![followup.clone()],
            vec![assistant.clone(), followup.clone()],
            vec![user.clone(), assistant],
            vec![user.clone(), call, followup.clone()],
            vec![user, result, followup],
        ] {
            prompt.messages = messages;
            assert!(!has_complete_visible_causal_history(&prompt));
        }
        Ok(())
    }
}
