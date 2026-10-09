use super::*;
use crate::language_model::native::NativeProtocolValidation;
use bitrouter_ai::protocol::OutboundDispatch;
use bitrouter_ai::types::{DataContent, ToolResultContentPart, ToolResultOutput};
use serde_json::json;

fn tool_result(output: ToolResultOutput) -> Content {
    Content::ToolResult {
        call_id: "call_1".into(),
        tool_name: Some("read".into()),
        output,
        dynamic: false,
        provider_metadata: Default::default(),
    }
}

fn validate(protocol: ApiProtocol, prompt: &Prompt) -> std::result::Result<(), &'static str> {
    let dispatch = OutboundDispatch::builtin();
    let (adapter, _) = dispatch
        .lookup(&protocol)
        .ok_or("fixture adapter missing")?;
    adapter.validate_managed_prompt(prompt)
}

#[test]
fn managed_protocol_checks_nested_tool_media_and_shared_capabilities() {
    let mut prompt = prompt();
    for (part, capability, responses, chat, messages) in [
        (
            ToolResultContentPart::Text {
                text: "required".into(),
            },
            None,
            true,
            true,
            true,
        ),
        (
            ToolResultContentPart::Media {
                media_type: "image/png".into(),
                data: DataContent::Base64 {
                    data: "aW1hZ2U=".into(),
                },
            },
            Some(Capability::ImageInput),
            true,
            true,
            true,
        ),
        (
            ToolResultContentPart::Media {
                media_type: "application/pdf".into(),
                data: DataContent::Url {
                    url: "https://example.invalid/required.pdf".into(),
                },
            },
            Some(Capability::FileInput),
            true,
            true,
            false,
        ),
        (
            ToolResultContentPart::FileId {
                media_type: None,
                id: "file-required".into(),
            },
            Some(Capability::FileInput),
            true,
            false,
            false,
        ),
    ] {
        let mut message = Message::text(Role::Tool, "");
        message.content = vec![tool_result(ToolResultOutput::Content { value: vec![part] })];
        prompt.messages = vec![message];
        for (protocol, allowed) in [
            (ApiProtocol::Responses, responses),
            (ApiProtocol::ChatCompletions, chat),
            (ApiProtocol::Messages, messages),
        ] {
            assert_eq!(
                validate(protocol.clone(), &prompt).is_ok(),
                allowed,
                "{protocol:?}"
            );
        }
        if let Some(capability) = capability {
            assert!(prompt.required_capabilities().contains(&capability));
        }
    }
}

#[test]
fn managed_protocol_rejects_lost_history_but_preserves_native_custom_calls() -> Result<()> {
    let mut prompt = prompt();
    let mut message = Message::text(Role::Tool, "required artifact");
    message.content.push(tool_result(ToolResultOutput::Text {
        value: "result".into(),
    }));
    prompt.messages = vec![message];
    for protocol in [ApiProtocol::Messages, ApiProtocol::ChatCompletions] {
        assert_eq!(
            validate(protocol, &prompt),
            Err("tool_message_content_would_be_dropped")
        );
    }
    let mut message = Message::text(Role::Assistant, "");
    message.content = vec![serde_json::from_value(json!({"type":"tool_call","id":"call_1","name":"shell","arguments":"ls -la","provider_metadata":{"openai":{"type":"custom_tool_call"}}})).map_err(|e|BitrouterError::internal(e.to_string()))?];
    prompt.messages = vec![message];
    assert_eq!(validate(ApiProtocol::Responses, &prompt), Ok(()));
    assert_eq!(
        validate(ApiProtocol::Messages, &prompt),
        Err("custom_tool_call_would_change_kind")
    );
    let dispatch = OutboundDispatch::builtin();
    let (adapter, _) = dispatch
        .lookup(&ApiProtocol::Responses)
        .ok_or_else(|| BitrouterError::internal("fixture adapter"))?;
    let body = adapter.render_request(&prompt)?;
    assert_eq!(body["input"][0]["type"], "custom_tool_call");
    assert_eq!(body["input"][0]["input"], "ls -la");
    prompt.messages[0].content = vec![Content::Reasoning {
        native: None,
        text: "required prior reasoning".into(),
        provider_metadata: Default::default(),
    }];
    for protocol in [ApiProtocol::Responses, ApiProtocol::Messages] {
        assert_eq!(
            validate(protocol, &prompt),
            Err("reasoning_history_would_be_dropped")
        );
    }
    Ok(())
}

#[test]
fn managed_protocol_checks_preflight_controls_without_input_counting() -> Result<()> {
    let executor = executor::HttpExecutor::with_defaults()?;
    let target = {
        let mut target = target("fixture");
        target.api_protocol = ApiProtocol::Responses;
        target
    };
    for (field, value, reason) in [
        (
            "truncation",
            json!("auto"),
            "automatic_truncation_forbidden",
        ),
        (
            "conversation",
            json!("conv_private"),
            "unmanaged_provider_context_forbidden",
        ),
        (
            "previous_response_id",
            json!("resp_private"),
            "provider_continuation_unbound",
        ),
        (
            "multi_agent",
            json!({"enabled":true}),
            "upstream_agent_scheduler_forbidden",
        ),
    ] {
        let mut req = request();
        req.input
            .generation_prompt_mut()
            .ok_or_else(|| BitrouterError::internal("generation fixture"))?
            .params
            .extra
            .insert(field.into(), value);
        let ctx = PipelineContext::new(req);
        assert_eq!(
            executor.native_protocol_validation(&target, ctx.require_generation_prompt()?, &ctx),
            NativeProtocolValidation::Rejected {
                reason: reason.into()
            }
        );
    }
    let ctx = PipelineContext::new(request());
    assert_eq!(
        executor.native_protocol_validation(&target, ctx.require_generation_prompt()?, &ctx),
        NativeProtocolValidation::Compatible
    );
    Ok(())
}

#[tokio::test]
async fn managed_control_cannot_admit_an_incompatible_http_route() -> Result<()> {
    let mut req = request();
    req.input
        .generation_prompt_mut()
        .ok_or_else(|| BitrouterError::internal("generation fixture"))?
        .params
        .extra
        .insert("truncation".into(), json!("auto"));
    let mut route = target("fixture");
    route.api_protocol = ApiProtocol::Responses;
    let table = Arc::new(StaticRoutingTable::new());
    table.insert("test-model", vec![route]);
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(table)
        .executor(Arc::new(executor::HttpExecutor::with_defaults()?));
    let error = Arc::new(builder.build()?)
        .execute_native_controlled(req, Arc::new(AdmittedRoutes(vec![0])))
        .await
        .err()
        .ok_or_else(|| BitrouterError::internal("incompatible route admitted"))?;
    assert!(
        error
            .to_string()
            .contains("admitted route failed managed protocol validation")
    );
    Ok(())
}

#[test]
fn managed_protocol_rejects_render_errors_and_lost_tool_identities() -> Result<()> {
    let executor = executor::HttpExecutor::with_defaults()?;
    let mut req = request();
    req.input
        .generation_prompt_mut()
        .ok_or_else(|| BitrouterError::internal("generation fixture"))?
        .params
        .store = Some(true);
    let ctx = PipelineContext::new(req);
    let mut route = target("fixture");
    route.api_protocol = ApiProtocol::Messages;
    assert_eq!(
        executor.native_protocol_validation(&route, ctx.require_generation_prompt()?, &ctx),
        NativeProtocolValidation::Rejected {
            reason: "protocol_render_failed".into()
        }
    );
    route.api_protocol = ApiProtocol::Responses;
    assert_eq!(
        executor.native_protocol_validation(&route, ctx.require_generation_prompt()?, &ctx),
        NativeProtocolValidation::Compatible
    );
    let mut prompt = prompt();
    for (content, reason) in [
        (
            json!({"type":"tool_call","id":"call_1","name":"read","arguments":"{}","provider_metadata":{"openai":{"namespace":"workspace"}}}),
            "tool_namespace_would_be_dropped",
        ),
        (
            json!({"type":"tool_result","call_id":"call_1","output":{"type":"text","value":"ok"},"provider_metadata":{"openai":{"type":"custom_tool_call_output"}}}),
            "custom_tool_result_would_change_kind",
        ),
    ] {
        let content: Content =
            serde_json::from_value(content).map_err(|e| BitrouterError::internal(e.to_string()))?;
        let mut message = Message::text(
            if matches!(&content, Content::ToolResult { .. }) {
                Role::Tool
            } else {
                Role::Assistant
            },
            "",
        );
        message.content = vec![content];
        prompt.messages = vec![message];
        assert_eq!(validate(ApiProtocol::Responses, &prompt), Ok(()));
        for protocol in [ApiProtocol::ChatCompletions, ApiProtocol::Messages] {
            assert_eq!(validate(protocol, &prompt), Err(reason));
        }
    }
    let mut content = tool_result(ToolResultOutput::ExecutionDenied {
        reason: Some("denied".into()),
    });
    if let Content::ToolResult {
        provider_metadata, ..
    } = &mut content
    {
        bitrouter_ai::types::set_provider_metadata(
            provider_metadata,
            "openai",
            "approvalId",
            json!("approval-private"),
        );
    }
    prompt.messages[0].content = vec![content];
    assert_eq!(
        validate(ApiProtocol::Responses, &prompt),
        Err("provider_approval_requires_native_replay")
    );
    Ok(())
}
