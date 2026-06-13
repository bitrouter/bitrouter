//! Mapping between BitRouter's canonical IR and the Chainlink wire envelope.

use bitrouter_sdk::language_model::{Content, FinishReason, GenerateResult, Prompt, Role, Usage};

use crate::wire::{InferenceRequest, InferenceResponse, WireUsage};

/// Concatenate the text of a message's content blocks (non-text parts ignored
/// for the MVP — Chainlink takes a single prompt string).
fn message_text(content: &[Content]) -> String {
    let mut out = String::new();
    for part in content {
        if let Content::Text { text, .. } = part {
            out.push_str(text);
        }
    }
    out
}

/// Flatten a canonical [`Prompt`] into a Chainlink [`InferenceRequest`].
///
/// `model` is the upstream service id (Chainlink model). `system` maps to
/// `system_prompt`; the remaining messages render as a labeled transcript, which
/// collapses to bare text for the common single-user-message case.
pub fn prompt_to_request(model: &str, prompt: &Prompt) -> InferenceRequest {
    let non_system: Vec<&bitrouter_sdk::language_model::Message> = prompt
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .collect();

    let body = if non_system.len() == 1 && non_system[0].role == Role::User {
        message_text(&non_system[0].content)
    } else {
        let mut transcript = String::new();
        for m in &non_system {
            let label = match m.role {
                Role::User | Role::Tool => "User",
                Role::Assistant => "Assistant",
                Role::System => continue,
            };
            transcript.push_str(label);
            transcript.push_str(": ");
            transcript.push_str(&message_text(&m.content));
            transcript.push('\n');
        }
        transcript.trim_end().to_string()
    };

    // System: prefer the out-of-band `system` field; fall back to any system
    // messages in the list.
    let system_prompt = prompt.system.clone().or_else(|| {
        let joined: String = prompt
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| message_text(&m.content))
            .collect::<Vec<_>>()
            .join("\n");
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    });

    InferenceRequest {
        model: model.to_string(),
        prompt: body,
        system_prompt,
    }
}

/// Map a Chainlink wire usage to canonical [`Usage`].
fn wire_usage_to_canonical(u: &WireUsage) -> Usage {
    Usage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    }
}

/// Build a canonical [`GenerateResult`] from a completed Chainlink response.
/// Caller must have already checked `status == Completed`.
pub fn completed_to_result(resp: &InferenceResponse) -> GenerateResult {
    let text = resp.output.clone().unwrap_or_default();
    GenerateResult {
        content: vec![Content::Text {
            text,
            provider_metadata: Default::default(),
        }],
        usage: resp.usage.as_ref().map(wire_usage_to_canonical),
        finish_reason: Some(FinishReason::Stop),
        response_id: Some(resp.id.clone()),
        stop_details: None,
        provider_metadata: Default::default(),
    }
}

/// Attach Chainlink attestation metadata (inference id, any digests) to the
/// result's `provider_metadata["chainlink"]`.
pub fn stash_attestation(result: &mut GenerateResult, resp: &InferenceResponse) {
    let resources: Vec<serde_json::Value> = resp
        .resource_summaries
        .iter()
        .map(|r| {
            serde_json::json!({
                "filename": r.filename,
                "digest": r.digest,
                "response_digest": r.response_digest,
            })
        })
        .collect();
    result.provider_metadata.insert(
        "chainlink".to_string(),
        serde_json::json!({ "inference_id": resp.id, "resources": resources }),
    );
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use bitrouter_sdk::language_model::Message;

    fn prompt_with(messages: Vec<Message>, system: Option<&str>) -> Prompt {
        Prompt {
            model: "gemma4".into(),
            system: system.map(|s| s.to_string()),
            system_provider_metadata: Default::default(),
            messages,
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    #[test]
    fn single_user_message_flattens_to_bare_text() {
        let p = prompt_with(
            vec![Message::text(Role::User, "Summarize the report.")],
            None,
        );
        let req = prompt_to_request("gemma4", &p);
        assert_eq!(req.prompt, "Summarize the report.");
        assert_eq!(req.system_prompt, None);
        assert_eq!(req.model, "gemma4");
    }

    #[test]
    fn system_field_maps_to_system_prompt() {
        let p = prompt_with(
            vec![Message::text(Role::User, "hi")],
            Some("You are terse."),
        );
        let req = prompt_to_request("gemma4", &p);
        assert_eq!(req.system_prompt.as_deref(), Some("You are terse."));
    }

    #[test]
    fn multi_turn_renders_labeled_transcript() {
        let p = prompt_with(
            vec![
                Message::text(Role::User, "hello"),
                Message::text(Role::Assistant, "hi there"),
                Message::text(Role::User, "thanks"),
            ],
            None,
        );
        let req = prompt_to_request("gemma4", &p);
        assert_eq!(req.prompt, "User: hello\nAssistant: hi there\nUser: thanks");
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;
    use crate::wire::Status;

    fn completed(output: &str) -> InferenceResponse {
        InferenceResponse {
            id: "abc".into(),
            status: Status::Completed,
            model: Some("gemma4".into()),
            output: Some(output.into()),
            usage: Some(WireUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
            }),
            error: None,
            resource_summaries: Vec::new(),
        }
    }

    #[test]
    fn completed_maps_output_to_text_content() {
        let r = completed_to_result(&completed("hello world"));
        match r.content.as_slice() {
            [Content::Text { text, .. }] => assert_eq!(text, "hello world"),
            other => panic!("expected one text block, got {other:?}"),
        }
        assert_eq!(r.finish_reason, Some(FinishReason::Stop));
        assert_eq!(r.response_id.as_deref(), Some("abc"));
    }

    #[test]
    fn completed_maps_usage() {
        let r = completed_to_result(&completed("x"));
        let u = r.usage.expect("usage");
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 5);
        assert_eq!(u.total(), 15);
    }

    #[test]
    fn stash_attestation_records_inference_id_and_digests() {
        let mut resp = completed("x");
        resp.resource_summaries = vec![crate::wire::ResourceSummary {
            filename: Some("report.pdf".into()),
            digest: Some("sha-abc".into()),
            response_digest: Some("sha-def".into()),
        }];
        let mut result = completed_to_result(&resp);
        stash_attestation(&mut result, &resp);
        let att = result
            .provider_metadata
            .get("chainlink")
            .expect("chainlink ns");
        assert_eq!(att["inference_id"], "abc");
        assert_eq!(att["resources"][0]["digest"], "sha-abc");
    }
}
