//! Managed calls cannot use a compatibility conversion that discards required
//! input or controls. These checks live beside the shared serving adapters.

use serde_json::Value;

use crate::types::{
    ApiProtocol, Content, Prompt, Role, Tool, ToolResultContentPart, ToolResultOutput,
    provider_namespace,
};

pub(super) fn validate_prompt(protocol: &ApiProtocol, prompt: &Prompt) -> Result<(), &'static str> {
    // The request renderers below document their official wire schemas. This
    // stricter managed contract rejects their known lossy compatibility cases.
    // https://developers.openai.com/api/reference/cli/resources/responses/methods/create
    // https://platform.claude.com/docs/en/api/messages
    // https://ai.google.dev/api/generate-content
    let params = &prompt.params;
    let unsupported_sampling = match protocol {
        ApiProtocol::ChatCompletions => params.top_k.is_some(),
        ApiProtocol::Responses => {
            params.top_k.is_some()
                || params.seed.is_some()
                || !params.stop.is_empty()
                || params.presence_penalty.is_some()
                || params.frequency_penalty.is_some()
                || !params.response_modalities.is_empty()
        }
        ApiProtocol::Messages => {
            params.seed.is_some()
                || params.presence_penalty.is_some()
                || params.frequency_penalty.is_some()
                || !params.response_modalities.is_empty()
        }
        _ => return Err("managed_protocol_validation_unsupported"),
    };
    if unsupported_sampling {
        return Err("generation_control_would_be_dropped");
    }
    let namespace = match protocol {
        ApiProtocol::Responses => Some("openai."),
        ApiProtocol::Messages => Some("anthropic."),
        _ => None,
    };
    for tool in &prompt.tools {
        match tool {
            Tool::ProviderDefined { id, args, .. }
                if !namespace.is_some_and(|prefix| id.starts_with(prefix)) || !args.is_object() =>
            {
                return Err("provider_tool_has_no_managed_wire_form");
            }
            Tool::Function {
                strict: Some(true), ..
            } if matches!(protocol, ApiProtocol::Messages) => {
                return Err("strict_tool_constraint_would_be_dropped");
            }
            _ => {}
        }
    }
    for message in &prompt.messages {
        if *protocol == ApiProtocol::ChatCompletions
            && message.role == Role::Tool
            && (message.content.len() != 1
                || !matches!(message.content.first(), Some(Content::ToolResult { .. })))
        {
            return Err("tool_message_content_would_be_dropped");
        }
        if *protocol == ApiProtocol::Messages
            && message.role == Role::Tool
            && message
                .content
                .iter()
                .any(|content| !matches!(content, Content::ToolResult { .. }))
        {
            return Err("tool_message_content_would_be_dropped");
        }
        for content in &message.content {
            match content {
                Content::ToolCall {
                    arguments,
                    provider_executed,
                    dynamic,
                    provider_metadata,
                    ..
                } => {
                    if *protocol != ApiProtocol::Responses
                        && provider_namespace(provider_metadata, "openai")
                            .is_some_and(|fields| fields.contains_key("namespace"))
                    {
                        return Err("tool_namespace_would_be_dropped");
                    }
                    if *provider_executed || *dynamic {
                        return Err("provider_execution_history_requires_native_replay");
                    }
                    let custom = provider_namespace(provider_metadata, "openai")
                        .and_then(|fields| fields.get("type"))
                        .and_then(Value::as_str)
                        == Some("custom_tool_call");
                    if custom && *protocol != ApiProtocol::Responses {
                        return Err("custom_tool_call_would_change_kind");
                    }
                    if !custom
                        && !serde_json::from_str::<Value>(arguments)
                            .is_ok_and(|value| value.is_object())
                    {
                        return Err("tool_arguments_have_no_object_wire_form");
                    }
                }
                Content::ToolResult {
                    output,
                    dynamic,
                    provider_metadata,
                    ..
                } => {
                    if *protocol != ApiProtocol::Responses
                        && provider_namespace(provider_metadata, "openai")
                            .and_then(|fields| fields.get("type"))
                            .and_then(Value::as_str)
                            == Some("custom_tool_call_output")
                    {
                        return Err("custom_tool_result_would_change_kind");
                    }
                    if *protocol == ApiProtocol::Responses
                        && matches!(output, ToolResultOutput::ExecutionDenied { .. })
                        && provider_namespace(provider_metadata, "openai")
                            .is_some_and(|fields| fields.contains_key("approvalId"))
                    {
                        return Err("provider_approval_requires_native_replay");
                    }
                    if *dynamic {
                        return Err("provider_execution_history_requires_native_replay");
                    }
                    if matches!(
                        protocol,
                        ApiProtocol::ChatCompletions | ApiProtocol::Messages
                    ) && message.role != Role::Tool
                    {
                        return Err("tool_result_role_would_drop_output");
                    }
                    if let ToolResultOutput::Content { value } = output {
                        for part in value {
                            let supported = match (protocol, part) {
                                (_, ToolResultContentPart::Text { .. }) => true,
                                (ApiProtocol::Responses, _) => true,
                                (
                                    ApiProtocol::ChatCompletions,
                                    ToolResultContentPart::Media { .. },
                                ) => true,
                                (
                                    ApiProtocol::Messages,
                                    ToolResultContentPart::Media { media_type, .. },
                                ) => media_type.starts_with("image/"),
                                _ => false,
                            };
                            if !supported {
                                return Err("tool_result_content_would_be_dropped");
                            }
                        }
                    }
                }
                Content::Reasoning { .. } if *protocol == ApiProtocol::Responses => {
                    if message.role != Role::Assistant {
                        return Err("responses_reasoning_role_invalid");
                    }
                    super::responses::validate_reasoning_history(content)?;
                }
                Content::ToolApprovalRequest { .. } | Content::ToolApprovalResponse { .. } => {
                    return Err("provider_approval_requires_native_replay");
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub(super) fn validate_body(
    protocol: &ApiProtocol,
    expected: &Value,
    actual: &Value,
) -> Result<(), &'static str> {
    if !expected.is_object() || !actual.is_object() {
        return Err("managed_body_not_object");
    }
    // Automatic truncation can remove mandatory history. Provider compaction and
    // mutable conversations likewise lack the core's acknowledged context plan.
    // https://developers.openai.com/api/reference/cli/resources/responses/methods/create
    if actual
        .get("truncation")
        .is_some_and(|value| !value.is_null() && value != "disabled")
    {
        return Err("automatic_truncation_forbidden");
    }
    if ["conversation", "context_management", "cachedContent"]
        .iter()
        .any(|field| actual.get(*field).is_some_and(|value| !value.is_null()))
    {
        return Err("unmanaged_provider_context_forbidden");
    }
    if actual.get("previous_response_id") != expected.get("previous_response_id") {
        return Err("provider_continuation_changed");
    }
    if actual
        .get("multi_agent")
        .is_some_and(|value| !value.is_null())
    {
        return Err("upstream_agent_scheduler_forbidden");
    }
    if !matches!(
        protocol,
        ApiProtocol::ChatCompletions | ApiProtocol::Responses | ApiProtocol::Messages
    ) {
        return Err("managed_body_validation_unsupported");
    }
    // The baseline has already received the provider's pure normalization.
    // Compare every field: provider-specific extras can also carry input,
    // sampling or tool constraints. Header authentication cannot alter them.
    if expected != actual {
        return Err("managed_context_or_controls_changed");
    }
    Ok(())
}
