//! Required canonical content cannot be silently omitted by a target projection.

#[path = "support/conversion.rs"]
mod conversion;
use conversion::{adapters, prompt, refusal};

use bitrouter_ai::conversion::{ConversionEffect, ConversionLocation};
use bitrouter_ai::protocol::{
    InboundAdapter, OutboundAdapter, messages::MessagesAdapter, responses::ResponsesAdapter,
};
use bitrouter_ai::types::{
    ApiProtocol, Content, Message, ProviderMetadata, Role, Tool, ToolResultContentPart,
    ToolResultOutput,
};
use serde_json::json;
type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
#[test]
fn uploaded_file_references_are_rejected_where_omitted_and_retained_on_responses() -> TestResult {
    let source=ResponsesAdapter.parse_request(json!({"model":"fixture","input":[{"type":"function_call_output","call_id":"c","output":[{"type":"input_text","text":"before"},{"type":"input_file","file_id":"file-secret"},{"type":"input_text","text":"after"}]}]}))?;
    for (protocol, adapter) in adapters() {
        if protocol == ApiProtocol::Responses {
            let body = adapter.render_request(&source)?;
            assert_eq!(body["input"][0]["output"][1]["file_id"], "file-secret");
        } else {
            let report = refusal(adapter.as_ref(), &source)?;
            assert_eq!(
                report.issues[0].location,
                ConversionLocation::ToolResultContent {
                    message: 0,
                    block: 0,
                    part: 1
                }
            );
        }
    }
    Ok(())
}
#[test]
fn tool_result_media_is_rejected_only_on_the_current_lossy_targets() -> TestResult {
    let mut source = prompt()?;
    source.messages = vec![Message {
        role: Role::Tool,
        content: vec![Content::ToolResult {
            call_id: "c".into(),
            tool_name: Some("f".into()),
            output: ToolResultOutput::Content {
                value: vec![ToolResultContentPart::Media {
                    media_type: "application/pdf".into(),
                    data: bitrouter_ai::types::DataContent::Base64 {
                        data: "payload-secret".into(),
                    },
                }],
            },
            dynamic: false,
            provider_metadata: ProviderMetadata::new(),
        }],
    }];
    for (protocol, adapter) in adapters() {
        if matches!(protocol, ApiProtocol::Messages) {
            refusal(adapter.as_ref(), &source)?;
        } else {
            assert!(
                adapter
                    .render_request(&source)?
                    .to_string()
                    .contains("payload-secret")
            );
        }
    }
    Ok(())
}
#[test]
fn provider_executed_calls_cannot_become_client_calls_or_disappear() -> TestResult {
    let mut source = prompt()?;
    source.messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::ToolCall {
            id: "call-secret".into(),
            name: "search".into(),
            arguments: "{}".into(),
            provider_executed: true,
            dynamic: false,
            provider_metadata: ProviderMetadata::new(),
        }],
    });
    for (protocol, adapter) in adapters() {
        if protocol == ApiProtocol::Messages {
            assert_eq!(
                adapter.render_request(&source)?["messages"][1]["content"][0]["type"],
                "server_tool_use"
            );
        } else {
            refusal(adapter.as_ref(), &source)?;
        }
    }
    Ok(())
}
#[test]
fn approval_responses_need_a_native_slot_and_cannot_lose_the_reason() -> TestResult {
    let mut source = prompt()?;
    source.messages.push(Message {
        role: Role::Tool,
        content: vec![Content::ToolApprovalResponse {
            approval_id: "approval-secret".into(),
            approved: false,
            reason: None,
            provider_metadata: ProviderMetadata::new(),
        }],
    });
    for (protocol, adapter) in adapters() {
        if protocol == ApiProtocol::Responses {
            assert_eq!(
                adapter.render_request(&source)?["input"][1]["approve"],
                false
            );
        } else {
            refusal(adapter.as_ref(), &source)?;
        }
    }
    if let Content::ToolApprovalResponse { reason, .. } = &mut source.messages[1].content[0] {
        *reason = Some("reason-secret".into());
    }
    refusal(&ResponsesAdapter, &source)?;
    Ok(())
}
#[test]
fn provider_defined_tools_are_not_dropped_or_forwarded_to_an_unclassified_wire() -> TestResult {
    let mut source = prompt()?;
    source.tools = vec![Tool::ProviderDefined {
        id: "openai.web_search".into(),
        name: "web_search".into(),
        args: json!({"user_location":{"type":"approximate","city":"arg-secret"}}),
        provider_metadata: ProviderMetadata::new(),
    }];
    for (protocol, adapter) in adapters() {
        if protocol == ApiProtocol::Responses {
            assert_eq!(
                adapter.render_request(&source)?["tools"][0]["type"],
                "web_search"
            );
        } else {
            let report = refusal(adapter.as_ref(), &source)?;
            assert_eq!(
                report.issues[0].location,
                ConversionLocation::ToolDefinition { tool: 0 }
            );
            if protocol != ApiProtocol::ChatCompletions {
                assert_eq!(report.issues[0].effect, ConversionEffect::Unknown);
            }
        }
    }
    Ok(())
}

#[test]
fn native_dynamic_mcp_history_is_kept_and_foreign_or_unrepresentable_history_is_refused()
-> TestResult {
    let source=MessagesAdapter.parse_response(json!({"id":"fixture","type":"message","role":"assistant","content":[{"type":"mcp_tool_use","id":"c","name":"f","server_name":"server-secret","input":{}},{"type":"mcp_tool_result","tool_use_id":"c","content":[{"type":"text","text":"result-secret"}]}]}))?;
    let mut request = prompt()?;
    request.messages = vec![Message {
        role: Role::Assistant,
        content: source.content,
    }];
    for (protocol, adapter) in adapters() {
        if protocol == ApiProtocol::Messages {
            let rendered = adapter.render_request(&request)?;
            assert_eq!(
                rendered["messages"][0]["content"][0]["type"],
                "mcp_tool_use"
            );
            assert_eq!(
                rendered["messages"][0]["content"][1]["type"],
                "mcp_tool_result"
            );
        } else {
            refusal(adapter.as_ref(), &request)?;
        }
    }
    if let Content::ToolResult { output, .. } = &mut request.messages[0].content[1] {
        *output = ToolResultOutput::Text {
            value: "unrepresentable-secret".into(),
        };
    }
    refusal(&MessagesAdapter, &request)?;
    Ok(())
}

#[test]
fn denial_suppression_requires_a_matching_denied_approval_response() -> TestResult {
    let mut source=ResponsesAdapter.parse_request(json!({"model":"fixture","input":[{"type":"mcp_approval_response","approval_request_id":"approval-secret","approve":false}]}))?;
    let rendered = ResponsesAdapter.render_request(&source)?;
    assert_eq!(
        rendered["input"].as_array().ok_or("missing input")?.len(),
        1
    );
    assert_eq!(rendered["input"][0]["approve"], false);
    source.messages[0].content.remove(0);
    let report = refusal(&ResponsesAdapter, &source)?;
    assert_eq!(
        report.issues[0].reason,
        bitrouter_ai::conversion::ConversionReason::ApprovalDenialUnpaired
    );
    Ok(())
}

#[test]
fn provider_tool_arguments_cannot_be_silently_replaced_by_an_empty_object() -> TestResult {
    let mut source = prompt()?;
    for (id, adapter) in [
        (
            "openai.web_search",
            &ResponsesAdapter as &dyn OutboundAdapter,
        ),
        (
            "anthropic.web_search",
            &MessagesAdapter as &dyn OutboundAdapter,
        ),
    ] {
        source.tools = vec![Tool::ProviderDefined {
            id: id.into(),
            name: "search".into(),
            args: json!("argument-secret"),
            provider_metadata: ProviderMetadata::new(),
        }];
        let report = refusal(adapter, &source)?;
        assert_eq!(
            report.issues[0].reason,
            bitrouter_ai::conversion::ConversionReason::ProviderToolArgumentsUnrepresentable
        );
    }
    Ok(())
}

#[test]
fn output_only_approval_requests_need_an_explicit_replay_representation() -> TestResult {
    let result=ResponsesAdapter.parse_response(json!({"output":[{"type":"mcp_approval_request","id":"approval-secret","server_label":"server-secret","name":"f","arguments":"{}"}]}))?;
    let mut source = prompt()?;
    source.messages = vec![Message {
        role: Role::Assistant,
        content: result.content,
    }];
    for (_, adapter) in adapters() {
        let report = refusal(adapter.as_ref(), &source)?;
        assert_eq!(
            report.issues[0].reason,
            bitrouter_ai::conversion::ConversionReason::ApprovalUnrepresentable
        );
    }
    Ok(())
}
