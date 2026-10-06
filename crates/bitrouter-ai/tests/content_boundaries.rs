//! Initial argument, JSON, ordering and nested-attribute admission.

use bitrouter_ai::conversion::{
    ConversionEffect, ConversionLocation, ConversionReport, ConversionStage,
};
use bitrouter_ai::error::ModelError;
use bitrouter_ai::protocol::{
    InboundAdapter, OutboundAdapter, chat_completions::ChatCompletionsAdapter,
    generate_content::GenerateContentAdapter, messages::MessagesAdapter,
    responses::ResponsesAdapter,
};
use bitrouter_ai::types::{
    ApiProtocol, Content, Message, Prompt, ProviderMetadata, Role, ToolResultOutput,
};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
fn prompt() -> bitrouter_ai::error::Result<Prompt> {
    ChatCompletionsAdapter
        .parse_request(json!({"model":"fixture","messages":[{"role":"user","content":"keep"}]}))
}
fn adapters() -> [(ApiProtocol, Box<dyn OutboundAdapter>); 4] {
    [
        (
            ApiProtocol::ChatCompletions,
            Box::new(ChatCompletionsAdapter),
        ),
        (ApiProtocol::Responses, Box::new(ResponsesAdapter)),
        (ApiProtocol::Messages, Box::new(MessagesAdapter)),
        (
            ApiProtocol::GenerateContent,
            Box::new(GenerateContentAdapter),
        ),
    ]
}
fn report(
    result: bitrouter_ai::error::Result<Value>,
) -> Result<ConversionReport, Box<dyn std::error::Error + Send + Sync>> {
    let Err(ModelError::Incompatible { report }) = result else {
        return Err("loss was silently admitted or lost its typed report".into());
    };
    assert!(!report.issues.is_empty());
    assert!(!format!("{report:?}").contains("secret"));
    assert!(!serde_json::to_string(&report)?.contains("secret"));
    Ok(report)
}
fn ingress(
    protocol: ApiProtocol,
    body: Value,
) -> Result<ConversionReport, Box<dyn std::error::Error + Send + Sync>> {
    let adapter = bitrouter_ai::protocol::inbound_adapter_for(&protocol).ok_or("no adapter")?;
    let original = body.clone();
    let result = adapter.parse_request(body.clone()).map(|_| Value::Null);
    let report = report(result)?;
    assert_eq!(body, original);
    for issue in &report.issues {
        assert_eq!(issue.stage, ConversionStage::RequestIngress);
    }
    Ok(report)
}
fn tool_result(output: ToolResultOutput) -> bitrouter_ai::error::Result<Prompt> {
    let mut source = prompt()?;
    source.messages = vec![Message {
        role: Role::Tool,
        content: vec![Content::ToolResult {
            call_id: "c".into(),
            tool_name: Some("f".into()),
            output,
            dynamic: false,
            provider_metadata: ProviderMetadata::new(),
        }],
    }];
    Ok(source)
}
fn call(arguments: &str) -> Content {
    Content::ToolCall {
        id: "c".into(),
        name: "f".into(),
        arguments: arguments.into(),
        provider_executed: false,
        dynamic: false,
        provider_metadata: ProviderMetadata::new(),
    }
}
fn text(value: &str) -> Content {
    Content::Text {
        text: value.into(),
        provider_metadata: ProviderMetadata::new(),
    }
}

#[test]
fn invalid_argument_strings_cannot_be_replaced_by_an_empty_object() -> TestResult {
    let mut source = prompt()?;
    source.messages = vec![Message {
        role: Role::Assistant,
        content: vec![call("argument-secret")],
    }];
    let original = source.clone();
    for (protocol, adapter) in adapters() {
        if matches!(
            protocol,
            ApiProtocol::Messages | ApiProtocol::GenerateContent
        ) {
            let report = report(adapter.render_request(&source))?;
            assert_eq!(report.issues[0].effect, ConversionEffect::TaskSemantics);
        } else {
            assert!(
                adapter
                    .render_request(&source)?
                    .to_string()
                    .contains("argument-secret")
            );
        }
    }
    source.messages[0].content = vec![call("{\"x\":42}")];
    for (_, adapter) in adapters() {
        adapter.render_request(&source)?;
    }
    assert_eq!(original.messages[0].content, vec![call("argument-secret")]);
    Ok(())
}

#[test]
fn json_encoding_preserves_values_and_native_gemini_objects() -> TestResult {
    let value = json!({"key-secret":[42,{"nested":true}]});
    let source = tool_result(ToolResultOutput::Json {
        value: value.clone(),
    })?;
    let original = source.clone();
    for (protocol, adapter) in adapters() {
        if protocol == ApiProtocol::GenerateContent {
            assert_eq!(
                adapter.render_request(&source)?["contents"][0]["parts"][0]["functionResponse"]["response"],
                value
            );
        } else {
            let body = adapter.render_request(&source)?;
            let encoded = match protocol {
                ApiProtocol::ChatCompletions => &body["messages"][0]["content"],
                ApiProtocol::Responses => &body["input"][0]["output"],
                _ => &body["messages"][0]["content"][0]["content"],
            };
            assert_eq!(
                serde_json::from_str::<Value>(encoded.as_str().ok_or("missing JSON encoding")?)?,
                value
            );
        }
    }
    assert_eq!(source, original);
    Ok(())
}

#[test]
fn scalar_json_and_text_to_object_wrappers_are_unclassified() -> TestResult {
    for output in [
        ToolResultOutput::Json {
            value: json!(["value-secret", 42]),
        },
        ToolResultOutput::Json { value: json!(42) },
        ToolResultOutput::Json { value: Value::Null },
        ToolResultOutput::Text {
            value: "value-secret".into(),
        },
    ] {
        let source = tool_result(output)?;
        let report = report(GenerateContentAdapter.render_request(&source))?;
        assert_eq!(report.issues[0].effect, ConversionEffect::Unknown);
    }
    Ok(())
}

#[test]
fn text_only_tool_arrays_keep_their_part_boundaries() -> TestResult {
    use bitrouter_ai::types::ToolResultContentPart;
    let source = tool_result(ToolResultOutput::Content {
        value: vec![
            ToolResultContentPart::Text {
                text: "before".into(),
            },
            ToolResultContentPart::Text {
                text: "after".into(),
            },
        ],
    })?;
    for (protocol, adapter) in adapters() {
        if protocol == ApiProtocol::GenerateContent {
            report(adapter.render_request(&source))?;
            continue;
        }
        let body = adapter.render_request(&source)?;
        let parsed = bitrouter_ai::protocol::inbound_adapter_for(&protocol)
            .ok_or("no adapter")?
            .parse_request(body)?;
        let output = parsed
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| {
                if let Content::ToolResult { output, .. } = content {
                    Some(output)
                } else {
                    None
                }
            })
            .ok_or("missing tool result")?;
        let Content::ToolResult {
            output: original, ..
        } = &source.messages[0].content[0]
        else {
            return Err("bad fixture".into());
        };
        assert_eq!(output, original);
    }
    Ok(())
}

#[test]
fn chat_text_parts_remain_separate_and_reasoning_parts_cannot_be_concatenated() -> TestResult {
    let mut source = prompt()?;
    source.messages = vec![Message {
        role: Role::Assistant,
        content: vec![text("before"), text("after")],
    }];
    let original = source.clone();
    let body = ChatCompletionsAdapter.render_request(&source)?;
    assert_eq!(
        body["messages"][0]["content"],
        json!([{"type":"text","text":"before"},{"type":"text","text":"after"}])
    );
    assert_eq!(
        ChatCompletionsAdapter.parse_request(body)?.messages,
        original.messages
    );
    source.messages[0].content = vec![
        Content::Reasoning {
            text: "before".into(),
            native: None,
            provider_metadata: ProviderMetadata::new(),
        },
        Content::Reasoning {
            text: "after".into(),
            native: None,
            provider_metadata: ProviderMetadata::new(),
        },
    ];
    report(ChatCompletionsAdapter.render_request(&source))?;
    Ok(())
}

#[test]
fn chat_cannot_move_post_call_text_or_late_reasoning_into_an_earlier_slot() -> TestResult {
    let mut source = prompt()?;
    source.messages = vec![Message {
        role: Role::Assistant,
        content: vec![text("before"), call("{}"), text("after")],
    }];
    let report = report(ChatCompletionsAdapter.render_request(&source))?;
    assert_eq!(
        report.issues[0].location,
        ConversionLocation::MessageContent {
            message: 0,
            block: 2
        }
    );
    let body = ResponsesAdapter.render_request(&source)?;
    assert_eq!(body["input"][0]["content"][0]["text"], "before");
    assert_eq!(body["input"][1]["type"], "function_call");
    assert_eq!(body["input"][2]["content"][0]["text"], "after");
    source.messages[0].content = vec![
        text("before"),
        Content::Reasoning {
            text: "late".into(),
            native: None,
            provider_metadata: ProviderMetadata::new(),
        },
    ];
    let report =
        bitrouter_ai::conversion::request_admission(&source, &ApiProtocol::ChatCompletions);
    assert!(!report.issues.is_empty());
    Ok(())
}

#[test]
fn ignored_tool_result_attributes_are_refused_at_original_wire_positions() -> TestResult {
    for (protocol, body, location) in [
        (
            ApiProtocol::ChatCompletions,
            json!({"model":"fixture","messages":[{"role":"tool","tool_call_id":"c","content":[{"type":"file","file":{"file_data":"data:application/pdf;base64,payload-secret","filename":"name-secret"}}]}]}),
            ConversionLocation::MessageContent {
                message: 0,
                block: 0,
            },
        ),
        (
            ApiProtocol::ChatCompletions,
            json!({"model":"fixture","messages":[{"role":"tool","tool_call_id":"c","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/secret.png","detail":"high"}}]}]}),
            ConversionLocation::MessageContent {
                message: 0,
                block: 0,
            },
        ),
        (
            ApiProtocol::Responses,
            json!({"model":"fixture","input":[{"type":"function_call_output","call_id":"c","output":[{"type":"input_file","file_data":"data:application/pdf;base64,payload-secret","filename":"name-secret"}]}]}),
            ConversionLocation::InputToolResultContent { item: 0, block: 0 },
        ),
        (
            ApiProtocol::Responses,
            json!({"model":"fixture","input":[{"type":"function_call_output","call_id":"c","output":[{"type":"input_image","file_id":"file-secret","detail":"high"}]}]}),
            ConversionLocation::InputToolResultContent { item: 0, block: 0 },
        ),
    ] {
        let report = ingress(protocol, body)?;
        assert_eq!(report.issues[0].location, location);
    }
    Ok(())
}

#[test]
fn responses_argument_values_and_ambiguous_tool_media_cannot_be_erased() -> TestResult {
    for body in [
        json!({"model":"fixture","input":[{"type":"function_call","call_id":"c","name":"f","arguments":{"value":"secret"}}]}),
        json!({"model":"fixture","input":[{"type":"custom_tool_call","call_id":"c","name":"f","input":{"value":"secret"}}]}),
        json!({"model":"fixture","input":[{"type":"function_call_output","call_id":"c","output":[{"type":"input_file","file_id":"file-secret","file_data":"data:application/pdf;base64,payload-secret"}]}]}),
    ] {
        ingress(ApiProtocol::Responses, body)?;
    }
    Ok(())
}

#[test]
fn ignored_document_and_function_response_attributes_are_refused() -> TestResult {
    ingress(
        ApiProtocol::Messages,
        json!({"model":"fixture","max_tokens":8,"messages":[{"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"payload-secret"},"context":"context-secret","title":"title-secret"}]}]}),
    )?;
    ingress(
        ApiProtocol::GenerateContent,
        json!({"contents":[{"role":"user","parts":[{"functionResponse":{"id":"c","name":"f","response":{"ok":true},"parts":[{"inlineData":{"mimeType":"image/png","data":"payload-secret"}}]}}]}]}),
    )?;
    ingress(
        ApiProtocol::GenerateContent,
        json!({"contents":[{"role":"user","parts":[{"functionResponse":{"id":"c","name":"f","response":{"ok":true},"willContinue":true}}]}]}),
    )?;
    Ok(())
}

#[test]
fn messages_cannot_partition_tool_results_ahead_of_original_text() -> TestResult {
    let body = json!({"model":"fixture","max_tokens":8,"messages":[{"role":"user","content":[{"type":"text","text":"before"},{"type":"tool_result","tool_use_id":"c","content":"value-secret"},{"type":"text","text":"after"}]}]});
    let report = ingress(ApiProtocol::Messages, body)?;
    assert_eq!(
        report.issues[0].location,
        ConversionLocation::MessageContent {
            message: 0,
            block: 1
        }
    );
    let native = MessagesAdapter.parse_request(json!({"model":"fixture","max_tokens":8,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":"kept"},{"type":"text","text":"after"}]}]}))?;
    assert_eq!(native.messages[0].role, Role::Tool);
    assert_eq!(native.messages[1].content, vec![text("after")]);
    Ok(())
}

#[test]
fn multiple_error_text_parts_cannot_collapse_during_messages_ingress() -> TestResult {
    let report = ingress(
        ApiProtocol::Messages,
        json!({"model":"fixture","max_tokens":8,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","is_error":true,"content":[{"type":"text","text":"before"},{"type":"text","text":"after-secret"}]}]}]}),
    )?;
    assert_eq!(
        report.issues[0].location,
        ConversionLocation::ToolResultContent {
            message: 0,
            block: 0,
            part: 1
        }
    );
    Ok(())
}

#[test]
fn native_messages_mcp_json_errors_keep_the_body_and_error_flag() -> TestResult {
    let value = json!([{"type":"text","text":"error-secret"}]);
    let source =
        MessagesAdapter.parse_request(json!({"model":"fixture","max_tokens":8,"messages":[{
        "role":"assistant","content":[
            {"type":"mcp_tool_use","id":"c","name":"f","input":{},"server_name":"server-secret"},
            {"type":"mcp_tool_result","tool_use_id":"c","content":value,"is_error":true}
        ]}]}))?;
    let original = source.clone();
    let body = MessagesAdapter.render_request(&source)?;
    assert_eq!(body["messages"][0]["content"][1]["content"], value);
    assert_eq!(body["messages"][0]["content"][1]["is_error"], true);
    assert_eq!(source, original);
    Ok(())
}
