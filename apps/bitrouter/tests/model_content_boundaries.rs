//! Initial argument, JSON, ordering and nested-attribute admission.

#[path = "model_support/conversion.rs"]
mod conversion;
use conversion::{adapters, prompt, refusal, require_refusal};

use bitrouter::gateway::model::protocol::chat_completions::ChatCompletionsAdapter;
use bitrouter::gateway::model::protocol::messages::MessagesAdapter;
use bitrouter::gateway::model::protocol::responses::ResponsesAdapter;
use bitrouter::gateway::model::protocol::{InboundAdapter, OutboundAdapter};
use bitrouter_ai::conversion::{
    ConversionEffect, ConversionLocation, ConversionReport, ConversionStage,
};
use bitrouter_ai::types::{
    ApiProtocol, Content, Message, Prompt, ProviderMetadata, Role, ToolResultOutput,
};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn assert_contract_round_trip<T: serde::de::DeserializeOwned + serde::Serialize>(
    body: Value,
    adapter: &dyn InboundAdapter,
) -> TestResult {
    let expected = adapter.parse_request(body.clone())?;
    let request: T = serde_json::from_value(body)?;
    let serialized = serde_json::to_value(request)?;
    let actual = adapter.parse_request(serialized)?;
    assert_eq!(
        serde_json::to_value(actual)?,
        serde_json::to_value(expected)?
    );
    Ok(())
}

#[test]
fn public_request_serialization_preserves_gateway_ingress_semantics() -> TestResult {
    use bitrouter_ai::protocol::chat_completions::ChatRequest;
    use bitrouter_ai::protocol::messages::MessagesRequest;
    use bitrouter_ai::protocol::responses::ResponsesRequest;

    assert_contract_round_trip::<ChatRequest>(
        json!({
            "model":"fixture", "stream":true,
            "messages":[
                {"role":"user", "content":"keep"},
                {"role":"assistant", "tool_calls":[{
                    "id":"call-1", "type":"function",
                    "function":{"name":"f", "arguments":"{\"x\":1}"}
                }]},
                {"role":"tool", "tool_call_id":"call-1", "content":"done"}
            ],
            "tools":[{"type":"function", "function":{
                "name":"f", "parameters":{"type":"object"}, "strict":true
            }}],
            "stream_options":{"include_usage":true, "vendor_option":42},
            "response_format":{"type":"json_schema", "json_schema":{
                "name":"result", "schema":{"type":"object"}, "strict":true
            }},
            "tool_choice":"auto", "vendor_request":{"key":"value"}
        }),
        &ChatCompletionsAdapter,
    )?;
    assert_contract_round_trip::<MessagesRequest>(
        json!({
            "model":"fixture", "max_tokens":64,
            "system":[{"type":"text", "text":"system"}],
            "messages":[{"role":"user", "content":[{"type":"text", "text":"keep"}]}],
            "tools":[{"name":"f", "input_schema":{"type":"object"}, "defer_loading":true}],
            "output_config":{"format":{"type":"json_schema", "schema":{"type":"object"}}, "effort":"high"},
            "metadata":{"user_id":"fixture"}
        }),
        &MessagesAdapter,
    )?;
    assert_contract_round_trip::<ResponsesRequest>(
        json!({
            "model":"fixture", "input":[{"role":"user", "content":"keep"}],
            "tools":[{"type":"function", "name":"f", "parameters":{"type":"object"}, "strict":true}],
            "reasoning":{"effort":"high", "summary":"auto"},
            "text":{"format":{"type":"json_schema", "name":"result", "schema":{"type":"object"}, "strict":true}, "verbosity":"low"},
            "store":false, "parallel_tool_calls":false, "metadata":{"key":"value"}
        }),
        &ResponsesAdapter,
    )?;
    Ok(())
}

#[test]
fn typed_request_round_trip_cannot_erase_a_replay_refusal() -> TestResult {
    use bitrouter::gateway::model::error::ModelError;
    use bitrouter_ai::protocol::chat_completions::ChatRequest;

    let body = json!({
        "model":"fixture",
        "messages":[{
            "role":"assistant", "content":"keep",
            "extra_content":{"google":{"thought_signature":"opaque-replay-secret"}}
        }]
    });
    let request: ChatRequest = serde_json::from_value(body.clone())?;
    assert!(!format!("{request:?}").contains("opaque-replay-secret"));
    let encoded = serde_json::to_value(request)?;
    assert_eq!(
        encoded["messages"][0]["extra_content"],
        body["messages"][0]["extra_content"]
    );
    let Err(ModelError::Incompatible { report: expected }) =
        ChatCompletionsAdapter.parse_request(body)
    else {
        return Err("fixture must be refused before typed serialization".into());
    };
    let Err(ModelError::Incompatible { report: actual }) =
        ChatCompletionsAdapter.parse_request(encoded)
    else {
        return Err("typed serialization erased replay admission evidence".into());
    };
    assert_eq!(actual, expected);
    Ok(())
}

fn ingress(
    protocol: ApiProtocol,
    body: Value,
) -> Result<ConversionReport, Box<dyn std::error::Error + Send + Sync>> {
    let adapter =
        bitrouter::gateway::model::protocol::inbound_adapter_for(&protocol).ok_or("no adapter")?;
    require_refusal(adapter.parse_request(body), ConversionStage::RequestIngress)
}
fn tool_result(output: ToolResultOutput) -> bitrouter::gateway::model::error::Result<Prompt> {
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
fn json_encoding_preserves_values_on_remaining_wires() -> TestResult {
    let value = json!({"key-secret":[42,{"nested":true}]});
    let source = tool_result(ToolResultOutput::Json {
        value: value.clone(),
    })?;
    for (protocol, adapter) in adapters() {
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
    Ok(())
}

#[test]
fn invalid_argument_strings_cannot_be_replaced_by_an_empty_object() -> TestResult {
    let mut source = prompt()?;
    source.messages = vec![Message {
        role: Role::Assistant,
        content: vec![call("argument-secret")],
    }];
    for (protocol, adapter) in adapters() {
        if matches!(protocol, ApiProtocol::Messages) {
            let report = refusal(adapter.as_ref(), &source)?;
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
    Ok(())
}

#[test]
fn chat_text_parts_remain_separate_and_reasoning_parts_cannot_be_concatenated() -> TestResult {
    let mut source = prompt()?;
    source.messages = vec![Message {
        role: Role::Assistant,
        content: vec![text("before"), text("after")],
    }];
    let body = ChatCompletionsAdapter.render_request(&source)?;
    assert_eq!(
        body["messages"][0]["content"],
        json!([{"type":"text","text":"before"},{"type":"text","text":"after"}])
    );
    assert_eq!(
        ChatCompletionsAdapter.parse_request(body)?.messages,
        source.messages
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
    refusal(&ChatCompletionsAdapter, &source)?;
    Ok(())
}

#[test]
fn chat_cannot_move_post_call_text_or_late_reasoning_into_an_earlier_slot() -> TestResult {
    let mut source = prompt()?;
    source.messages = vec![Message {
        role: Role::Assistant,
        content: vec![text("before"), call("{}"), text("after")],
    }];
    let report = refusal(&ChatCompletionsAdapter, &source)?;
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
    let report = bitrouter::gateway::model::conversion::request_admission(
        &source,
        &ApiProtocol::ChatCompletions,
    );
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
    let body = MessagesAdapter.render_request(&source)?;
    assert_eq!(body["messages"][0]["content"][1]["content"], value);
    assert_eq!(body["messages"][0]["content"][1]["is_error"], true);
    Ok(())
}
