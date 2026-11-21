use super::*;
use crate::gateway::{
    error::Result,
    pipeline::{context::PipelineContext, types::PipelineRequest},
};
use bitrouter_ai::types::{FinishReason, GenerateResult, Message, Role, Usage};

#[tokio::test]
async fn model_routing_cannot_project_private_history_or_rewrite_public_content() -> Result<()> {
    for rewrite_public in [false, true] {
        let mut ctx = context();
        let mut prompt = ctx.require_generation_prompt()?.clone();
        prompt.messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                Content::Reasoning {
                    text: "private summary".into(),
                    native: Some(bitrouter_ai::types::NativeReasoning::Responses(
                        serde_json::json!({"type":"reasoning","summary":[],"encrypted_content":"sealed"}),
                    )),
                    provider_metadata: Default::default(),
                },
                Content::Text {
                    text: "public answer".into(),
                    provider_metadata: Default::default(),
                },
            ],
        }];
        ctx.apply_routing_prompt(prompt.clone())?;
        let projected = vec![Message::text(
            Role::Assistant,
            if rewrite_public {
                "forged public answer"
            } else {
                "public answer"
            },
        )];
        assert!(ctx.replace_managed_messages(projected).is_err());
        assert_eq!(ctx.require_generation_prompt()?, &prompt);
    }
    Ok(())
}
fn context() -> PipelineContext {
    PipelineContext::new(PipelineRequest {
        extensions: http::Extensions::new(),
        request_id: "request".into(),
        original_model: "served".into(),
        model: "served".into(),
        caller: CallerContext::local(),
        headers: Default::default(),
        inbound_protocol: None,
        input: crate::gateway::pipeline::types::PipelineInput::Generation(Box::new(Prompt {
            model: "served".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "task")],
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        })),
    })
}

fn result() -> GenerateResult {
    GenerateResult {
        content: vec![Content::Reasoning {
            native: None,
            text: "thought".into(),
            provider_metadata: [
                (
                    "anthropic".into(),
                    serde_json::json!({"signature":"fixture-signature"}),
                ),
                (
                    ORIGIN_NAMESPACE.into(),
                    serde_json::json!({ORIGIN_FIELD:"untrusted"}),
                ),
            ]
            .into(),
        }],
        usage: Some(Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
            ..Default::default()
        }),
        finish_reason: Some(FinishReason::Stop),
        response_id: Some("fixture-response".into()),
        stop_details: None,
        provider_metadata: Default::default(),
    }
}

#[test]
fn private_message_commitment_ignores_all_markers_but_binds_all_other_content()
-> std::result::Result<(), PrivateContextFailure> {
    let mut parts = result().content;
    parts.push(parts[0].clone());
    let original = message_commitment(&parts)?;
    clear_origins(&mut parts);
    assert_eq!(message_commitment(&parts)?, original);
    metadata_mut(&mut parts[1]).insert(
        ORIGIN_NAMESPACE.into(),
        serde_json::json!({ORIGIN_FIELD:"another"}),
    );
    assert_eq!(message_commitment(&parts)?, original);
    parts.push(Content::Text {
        text: "altered assistant".into(),
        provider_metadata: Default::default(),
    });
    assert_ne!(message_commitment(&parts)?, original);
    Ok(())
}
