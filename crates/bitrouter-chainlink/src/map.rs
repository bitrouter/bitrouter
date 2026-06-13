//! Mapping between BitRouter's canonical IR and the Chainlink wire envelope.

use bitrouter_sdk::language_model::{
    Content, DataContent, FinishReason, GenerateResult, Prompt, Role, Usage,
};

use crate::wire::{InferenceRequest, InferenceResponse, Resource, WireUsage};

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

    let resources: Vec<Resource> = prompt
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|part| match part {
            Content::File {
                media_type,
                data: DataContent::Base64 { data },
                filename,
                ..
            } => Some(Resource {
                filename: filename.clone().unwrap_or_else(|| "resource".to_string()),
                content_type: media_type.clone(),
                content_base64: data.clone(),
            }),
            // URL-based files are not forwarded in the MVP (Chainlink takes inline bytes).
            _ => None,
        })
        .collect();

    InferenceRequest {
        model: model.to_string(),
        prompt: body,
        system_prompt,
        resources,
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

/// Attach Chainlink's **neutral** per-inference evidence (inference id + the
/// unsigned resource digests) to `provider_metadata["chainlink"]`. This is data,
/// not a verdict — it makes no attestation claim. A caller verifies it on demand
/// via `bitrouter verify`.
pub fn stash_evidence(result: &mut GenerateResult, resp: &InferenceResponse) {
    let resources: Vec<serde_json::Value> = resp
        .resources
        .iter()
        .map(|r| {
            serde_json::json!({
                "digest": r.digest,
                "request_digest": r.request_digest,
                "response_digest": r.response_digest,
                "filename_digest": r.filename_digest,
                "filename_blinding": r.filename_blinding,
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

    #[test]
    fn file_parts_map_to_resources() {
        use bitrouter_sdk::language_model::{Content, DataContent, Message};
        let p = Prompt {
            model: "gemma4".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    Content::Text {
                        text: "review this".into(),
                        provider_metadata: Default::default(),
                    },
                    Content::File {
                        media_type: "text/plain".into(),
                        data: DataContent::Base64 {
                            data: "aGk=".into(),
                        },
                        filename: Some("doc.txt".into()),
                        provider_metadata: Default::default(),
                    },
                ],
            }],
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let req = prompt_to_request("gemma4", &p);
        assert_eq!(req.resources.len(), 1);
        assert_eq!(req.resources[0].filename, "doc.txt");
        assert_eq!(req.resources[0].content_base64, "aGk=");
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
            resources: Vec::new(),
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
    fn stash_evidence_records_inference_id_and_digests_without_claiming_attested() {
        let mut resp = completed("x");
        resp.resources = vec![crate::wire::ResourceDigest {
            digest: Some("sha-abc".into()),
            request_digest: Some("sha-req".into()),
            response_digest: Some("sha-resp".into()),
            filename_digest: None,
            filename_blinding: None,
        }];
        let mut result = completed_to_result(&resp);
        stash_evidence(&mut result, &resp);
        let ev = result
            .provider_metadata
            .get("chainlink")
            .expect("chainlink ns");
        assert_eq!(ev["inference_id"], "abc");
        assert_eq!(ev["resources"][0]["digest"], "sha-abc");
        assert!(ev.get("attested").is_none(), "must not claim attested");
    }
}
