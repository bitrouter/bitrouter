//! Request attributes, constraints and status cannot silently disappear.

#[path = "support/conversion.rs"]
mod conversion;
use conversion::{adapters, prompt, refusal};

use bitrouter_ai::conversion::{ConversionEffect, ConversionLocation};
use bitrouter_ai::protocol::{
    InboundAdapter, OutboundAdapter, generate_content::GenerateContentAdapter,
    responses::ResponsesAdapter,
};
use bitrouter_ai::types::{
    ApiProtocol, Content, DataContent, Message, Prompt, ProviderMetadata, ResponseFormat, Role,
    Tool, ToolResultOutput,
};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn function(parameters: Value, strict: Option<bool>) -> Tool {
    Tool::Function {
        name: "f".into(),
        description: None,
        parameters,
        strict,
        provider_metadata: ProviderMetadata::new(),
    }
}

#[test]
fn explicit_function_strict_flags_need_a_faithful_target_slot() -> TestResult {
    for strict in [true, false] {
        let mut source = prompt()?;
        source.tools = vec![function(json!({"type":"object"}), Some(strict))];
        for (protocol, adapter) in adapters() {
            if matches!(
                protocol,
                ApiProtocol::Messages | ApiProtocol::GenerateContent
            ) {
                let report = refusal(adapter.as_ref(), &source)?;
                assert_eq!(
                    report.issues[0].location,
                    ConversionLocation::ToolDefinition { tool: 0 }
                );
            } else {
                let body = adapter.render_request(&source)?;
                let flag = if protocol == ApiProtocol::Responses {
                    &body["tools"][0]["strict"]
                } else {
                    &body["tools"][0]["function"]["strict"]
                };
                assert_eq!(flag, strict);
            }
        }
    }
    Ok(())
}

#[test]
fn gemini_schema_cleanup_cannot_delete_constraints_or_collapse_distinct_types() -> TestResult {
    for parameters in [
        json!({"type":"object","additionalProperties":false,"properties":{"constraint-secret":{"type":"string"}}}),
        json!({"type":"object","properties":{"x":{"type":"array","items":{"type":"number","exclusiveMinimum":0}}}}),
        json!({"anyOf":[{"type":"object","properties":{"x":{"$ref":"#/$defs/secret"}}}],"$defs":{"secret":{"type":"string"}}}),
        json!({"type":["string","integer","null"]}),
        json!({"type":["null"]}),
        json!({"type":["string","null"],"nullable":false}),
    ] {
        let mut source = prompt()?;
        source.tools = vec![function(parameters.clone(), None)];
        let report = refusal(&GenerateContentAdapter, &source)?;
        assert_eq!(report.issues[0].effect, ConversionEffect::Unknown);
        for (protocol, adapter) in adapters() {
            if protocol == ApiProtocol::GenerateContent {
                continue;
            }
            let body = adapter.render_request(&source)?;
            let rendered = match protocol {
                ApiProtocol::ChatCompletions => &body["tools"][0]["function"]["parameters"],
                ApiProtocol::Responses => &body["tools"][0]["parameters"],
                _ => &body["tools"][0]["input_schema"],
            };
            assert_eq!(rendered, &parameters);
        }
    }
    Ok(())
}

#[test]
fn gemini_equivalent_nullable_and_single_type_unions_remain_eligible() -> TestResult {
    let mut source = prompt()?;
    source.tools = vec![function(
        json!({
            "type":"object", "required":["x"],
            "properties":{"x":{"type":["null","integer"],"minimum":0},
                "y":{"type":"array","items":{"type":["string"]}},
                "z":{"anyOf":[{"type":["boolean","null"]},{"type":"string"}]}}
        }),
        None,
    )];
    let body = GenerateContentAdapter.render_request(&source)?;
    let parameters = &body["tools"][0]["functionDeclarations"][0]["parameters"];
    assert_eq!(parameters["properties"]["x"]["type"], "integer");
    assert_eq!(parameters["properties"]["x"]["nullable"], true);
    assert_eq!(parameters["properties"]["x"]["minimum"], 0);
    assert_eq!(parameters["properties"]["y"]["items"]["type"], "string");
    assert_eq!(parameters["properties"]["z"]["anyOf"][0]["nullable"], true);
    Ok(())
}

#[test]
fn structured_output_metadata_cannot_disappear_on_other_wires() -> TestResult {
    for (name, description, strict) in [
        (Some("name-secret".into()), None, None),
        (None, Some("description-secret".into()), None),
        (None, None, Some(true)),
        (None, None, Some(false)),
    ] {
        let mut source = prompt()?;
        source.response_format = Some(ResponseFormat::JsonSchema {
            name,
            description,
            strict,
            schema: json!({"type":"object","properties":{"x":{"type":"string"}}}),
        });
        for (protocol, adapter) in adapters() {
            if matches!(
                protocol,
                ApiProtocol::Messages | ApiProtocol::GenerateContent
            ) {
                let report = refusal(adapter.as_ref(), &source)?;
                assert_eq!(
                    report.issues[0].location,
                    ConversionLocation::ResponseFormat
                );
            } else {
                adapter.render_request(&source)?;
            }
        }
    }
    let mut source = prompt()?;
    source.response_format = Some(ResponseFormat::JsonSchema {
        name: None,
        description: None,
        strict: None,
        schema: json!({"type":"object"}),
    });
    for (_, adapter) in adapters() {
        adapter.render_request(&source)?;
    }
    Ok(())
}

#[test]
fn file_names_are_preserved_or_reported_as_unclassified_omissions() -> TestResult {
    for media_type in ["application/pdf", "image/png", "audio/wav"] {
        let mut source = prompt()?;
        source.messages[0].content = vec![Content::File {
            media_type: media_type.into(),
            data: DataContent::Base64 {
                data: "payload-secret".into(),
            },
            filename: Some("name-secret".into()),
            provider_metadata: ProviderMetadata::new(),
        }];
        for (protocol, adapter) in adapters() {
            let keeps_name = (protocol == ApiProtocol::Responses
                && !media_type.starts_with("image/"))
                || (protocol == ApiProtocol::ChatCompletions && media_type == "application/pdf");
            if keeps_name {
                assert!(
                    adapter
                        .render_request(&source)?
                        .to_string()
                        .contains("name-secret")
                );
            } else {
                let report = refusal(adapter.as_ref(), &source)?;
                assert_eq!(report.issues[0].effect, ConversionEffect::Unknown);
            }
        }
    }
    Ok(())
}

fn result_prompt(output: ToolResultOutput) -> bitrouter_ai::error::Result<Prompt> {
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

#[test]
fn error_status_cannot_become_an_ordinary_success_result() -> TestResult {
    for output in [
        ToolResultOutput::ErrorText {
            value: "error-secret".into(),
        },
        ToolResultOutput::ErrorJson {
            value: json!({"error-secret":42}),
        },
    ] {
        let source = result_prompt(output)?;
        for (protocol, adapter) in adapters() {
            if protocol == ApiProtocol::Messages {
                assert_eq!(
                    adapter.render_request(&source)?["messages"][0]["content"][0]["is_error"],
                    true
                );
            } else {
                let report = refusal(adapter.as_ref(), &source)?;
                assert_eq!(report.issues[0].effect, ConversionEffect::TaskSemantics);
                assert_eq!(
                    report.issues[0].location,
                    ConversionLocation::MessageContent {
                        message: 0,
                        block: 0
                    }
                );
            }
        }
    }
    Ok(())
}

#[test]
fn denial_status_and_suppressed_denial_reasons_need_a_native_slot() -> TestResult {
    for reason in [None, Some("denial-secret".into())] {
        let source = result_prompt(ToolResultOutput::ExecutionDenied { reason })?;
        for (_, adapter) in adapters() {
            refusal(adapter.as_ref(), &source)?;
        }
    }
    let mut source = ResponsesAdapter.parse_request(json!({"model":"fixture","input":[{
        "type":"mcp_approval_response","approval_request_id":"approval-secret","approve":false
    }]}))?;
    let body = ResponsesAdapter.render_request(&source)?;
    assert_eq!(body["input"].as_array().ok_or("missing input")?.len(), 1);
    for content in source
        .messages
        .iter_mut()
        .flat_map(|message| &mut message.content)
    {
        if let Content::ToolResult {
            output: ToolResultOutput::ExecutionDenied { reason },
            ..
        } = content
        {
            *reason = Some("denial-secret".into());
        }
    }
    refusal(&ResponsesAdapter, &source)?;
    Ok(())
}
