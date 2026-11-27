//! Request attributes, constraints and status cannot silently disappear.

use super::conversion::{adapters, prompt, refusal};

use crate::gateway::conversion_report::{ConversionEffect, ConversionLocation, ConversionReason};
use crate::gateway::model::protocol::chat_completions::ChatCompletionsAdapter;
use crate::gateway::model::protocol::messages::MessagesAdapter;
use crate::gateway::model::protocol::responses::ResponsesAdapter;
use crate::gateway::model::protocol::{InboundAdapter, OutboundAdapter};
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
fn function_strict_preserves_required_enforcement_and_target_defaults() -> TestResult {
    for strict in [true, false] {
        let mut source = prompt()?;
        source.tools = vec![function(json!({"type":"object"}), Some(strict))];
        for (protocol, adapter) in adapters() {
            match protocol {
                ApiProtocol::Messages => {
                    let body = adapter.render_request(&source)?;
                    assert_eq!(
                        body["tools"][0].get("strict"),
                        strict.then(|| json!(true)).as_ref()
                    );
                }
                ApiProtocol::Responses => {
                    let body = adapter.render_request(&source)?;
                    assert_eq!(body["tools"][0]["strict"], strict);
                }
                _ => {
                    let body = adapter.render_request(&source)?;
                    assert_eq!(body["tools"][0]["function"]["strict"], strict);
                }
            }
        }
    }
    Ok(())
}

#[test]
fn strict_structured_output_maps_to_messages_and_back() -> TestResult {
    let schema = json!({"type":"object","properties":{"answer":{"type":"string"}},
        "required":["answer"],"additionalProperties":false});
    let description = "Report the final answer.";
    let input = json!({"model":"fixture","input":"hi","text":{"format":{
        "type":"json_schema","name":"report","description":description,"strict":true,"schema":schema
    }}});
    let mut source = ResponsesAdapter.parse_request(input.clone())?;
    let body = MessagesAdapter.render_request(&source)?;
    let mut expected = schema.clone();
    expected["description"] = json!(description);
    assert_eq!(
        body["output_config"]["format"],
        json!({"type":"json_schema","schema":expected})
    );

    let prompt = MessagesAdapter.parse_request(body)?;
    let responses = ResponsesAdapter.render_request(&prompt)?;
    let chat = ChatCompletionsAdapter.render_request(&prompt)?;
    for format in [
        &responses["text"]["format"],
        &chat["response_format"]["json_schema"],
    ] {
        assert_eq!(format["strict"], true);
        assert_eq!(format["schema"], expected);
    }

    if let Some(ResponseFormat::JsonSchema { schema, .. }) = &mut source.response_format {
        schema["description"] = json!("Conflicting guidance.");
    }
    let report = refusal(&MessagesAdapter, &source)?;
    assert_eq!(
        report.issues[0].reason,
        ConversionReason::ResponseFormatMetadataUnrepresentable
    );

    for strict in [Value::Null, json!(false)] {
        let mut input = input.clone();
        input["text"]["format"]["strict"] = strict;
        let report = refusal(&MessagesAdapter, &ResponsesAdapter.parse_request(input)?)?;
        assert_eq!(
            report.issues[0].reason,
            ConversionReason::ResponseFormatStrictUnrepresentable
        );
    }
    Ok(())
}

#[test]
fn messages_function_strict_flags_survive_openai_projection() -> TestResult {
    let input = json!({"model":"fixture","max_tokens":64,
        "messages":[{"role":"user","content":"hi"}],
        "tools":[
            {"type":"custom","name":"enforced","strict":true,"input_schema":{"type":"object"}},
            {"name":"relaxed","strict":false,"input_schema":{"type":"object"}},
            {"name":"default","input_schema":{"type":"object"}}
        ]
    });
    let source = MessagesAdapter.parse_request(input.clone())?;
    let native = MessagesAdapter.render_request(&source)?;
    assert_eq!(native["tools"][0]["type"], "custom");
    assert_eq!(
        native["tools"][0]["input_schema"],
        input["tools"][0]["input_schema"]
    );
    assert_eq!(native["tools"][0]["strict"], true);
    assert_eq!(native["tools"][1]["strict"], false);
    assert!(native["tools"][2].get("strict").is_none());

    let responses = ResponsesAdapter.render_request(&source)?;
    let chat = ChatCompletionsAdapter.render_request(&source)?;
    for (index, strict) in [true, false, false].into_iter().enumerate() {
        assert_eq!(responses["tools"][index]["strict"], strict);
    }
    assert_eq!(chat["tools"][0]["function"]["strict"], true);
    assert_eq!(chat["tools"][1]["function"]["strict"], false);
    assert!(chat["tools"][2]["function"].get("strict").is_none());
    Ok(())
}

#[test]
fn foreign_tool_allowlists_keep_only_selected_functions_and_mode() -> TestResult {
    let responses_body = json!({"model":"fixture","input":"hi",
        "tools":[
            {"type":"function","name":"first","parameters":{"type":"object"}},
            {"type":"function","name":"second","parameters":{"type":"object"}}
        ],
        "tool_choice":{"type":"allowed_tools","mode":"required",
            "tools":[{"type":"function","name":"second"}]}
    });
    let prompt = ResponsesAdapter.parse_request(responses_body.clone())?;
    let messages = MessagesAdapter.render_request(&prompt)?;
    assert_eq!(
        messages["tools"]
            .as_array()
            .ok_or("missing Messages tools")?
            .len(),
        1
    );
    assert_eq!(messages["tools"][0]["name"], "second");
    assert_eq!(messages["tool_choice"], json!({"type":"any"}));
    let chat = ChatCompletionsAdapter.render_request(&prompt)?;
    assert_eq!(
        chat["tools"].as_array().ok_or("missing Chat tools")?.len(),
        1
    );
    assert_eq!(chat["tools"][0]["function"]["name"], "second");
    assert_eq!(chat["tool_choice"], "required");
    let native = ResponsesAdapter.render_request(&prompt)?;
    assert_eq!(native["tool_choice"], responses_body["tool_choice"]);

    let chat = json!({"model":"fixture","messages":[{"role":"user","content":"hi"}],
        "tools":[
            {"type":"function","function":{"name":"first","parameters":{"type":"object"}}},
            {"type":"function","function":{"name":"second","parameters":{"type":"object"}}}
        ],
        "tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"auto",
            "tools":[{"type":"function","function":{"name":"second"}}]}}
    });
    let prompt = ChatCompletionsAdapter.parse_request(chat)?;
    let responses = ResponsesAdapter.render_request(&prompt)?;
    assert_eq!(
        responses["tools"]
            .as_array()
            .ok_or("missing Responses tools")?
            .len(),
        1
    );
    assert_eq!(responses["tools"][0]["name"], "second");
    assert_eq!(responses["tools"][0]["strict"], false);
    assert_eq!(responses["tool_choice"], "auto");
    Ok(())
}

#[test]
fn foreign_unrepresentable_tool_choices_are_refused() -> TestResult {
    for selector in [
        json!({"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"missing"}]}),
        json!({"type":"allowed_tools","mode":"required","tools":[{"type":"web_search"}]}),
        json!({"type":"allowed_tools","mode":"required","tools":[]}),
    ] {
        let source = ResponsesAdapter.parse_request(json!({
            "model":"fixture","input":"hi","tool_choice":selector,
            "tools":[{"type":"function","name":"f","parameters":{"type":"object"}}]
        }))?;
        let report = refusal(&MessagesAdapter, &source)?;
        assert_eq!(report.issues[0].location, ConversionLocation::ToolChoice);
        assert_eq!(
            report.issues[0].reason,
            ConversionReason::ToolChoiceUnrepresentable
        );
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

fn result_prompt(output: ToolResultOutput) -> crate::gateway::model::error::Result<Prompt> {
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
