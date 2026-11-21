use super::*;
use crate::gateway::model::conversion::ReportAdmission;
use serde_json::{Value, json};

#[test]
fn responses_output_preserves_empty_encrypted_and_stored_items_in_order() -> anyhow::Result<()> {
    let adapter = ResponsesAdapter;
    let output = json!([
        {"type":"reasoning", "id":"rs_a", "summary":[
            {"type":"summary_text", "text":"first "},
            {"type":"summary_text", "text":"summary"}
        ], "encrypted_content":"opaque-a", "status":"completed"},
        {"type":"function_call", "call_id":"call_a", "name":"inspect", "arguments":"{}"},
        {"type":"reasoning", "id":"rs_b", "summary":[], "encrypted_content":"opaque-b"},
        {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]},
        {"type":"reasoning", "id":"rs_c", "summary":[], "encrypted_content":null, "status":null}
    ]);
    let result =
        adapter.parse_response(json!({"id":"resp_a", "status":"completed", "output":output}))?;
    assert_eq!(result.content.len(), 5);
    assert!(
        matches!(&result.content[0], Content::Reasoning { text, .. } if text == "first summary")
    );
    assert!(matches!(&result.content[2], Content::Reasoning { text, .. } if text.is_empty()));
    assert!(assistant_turn_commitment(&result.content).is_some());
    assert!(terminal_assistant_turn_commitment(&json!({"output":output})).is_none());
    let mut prompt = adapter.parse_request(json!({"model":"served", "input":"task"}))?;
    prompt.messages.push(Message {
        role: Role::Assistant,
        content: result.content.clone(),
    });
    assert!(adapter.render_request(&prompt).is_err());
    let rendered = adapter.render_response(&result, &prompt, "request")?;
    assert_eq!(rendered["output"], output);
    let public: bitrouter_ai::protocol::responses::ResponsesResponse =
        serde_json::from_value(rendered)?;
    assert_eq!(serde_json::to_value(public)?["output"], output);
    let inbound = adapter.parse_request(json!({"model":"served", "input":output}))?;
    assert!(adapter.render_request(&inbound).is_err());
    Ok(())
}

#[test]
fn responses_reasoning_projection_rejects_malformed_or_unsynchronized_items() -> Result<()> {
    let adapter = ResponsesAdapter;
    let valid = json!({"type":"reasoning", "id":"rs_a", "summary":[{"type":"summary_text", "text":"checked"}], "encrypted_content":"opaque"});
    for (key, value) in [
        ("type", json!("message")),
        ("id", Value::Null),
        ("summary", json!([{"type":"summary_text", "text":17}])),
        (
            "summary",
            json!([{"type":"summary_text", "text":"checked", "hidden":"unchecked"}]),
        ),
        ("encrypted_content", json!({"unexpected":"object"})),
        ("status", json!("unknown")),
        (
            "unchecked_field",
            json!([{"type":"reasoning_text", "text":"unchecked"}]),
        ),
    ] {
        let mut item = valid.clone();
        item[key] = value;
        assert_eq!(
            validate_reasoning_history(&Content::Reasoning {
                text: "checked".into(),
                native: Some(NativeReasoning::Responses(item)),
                provider_metadata: Default::default()
            }),
            Err("responses_reasoning_item_invalid"),
            "{key}"
        );
    }
    let mut prompt = adapter.parse_request(json!({"model":"served", "input":[valid]}))?;
    let Content::Reasoning { text, .. } = &mut prompt.messages[0].content[0] else {
        return Err(ModelError::invalid_request("fixture lost reasoning"));
    };
    *text = "changed".into();
    assert_eq!(
        validate_reasoning_projection(&prompt.messages[0].content[0]),
        Err("responses_reasoning_item_invalid")
    );
    if let Content::Reasoning { text, .. } = &mut prompt.messages[0].content[0] {
        *text = "checked".into();
    }
    prompt.messages[0].role = Role::User;
    assert!(adapter.admission(&prompt).require_admitted().is_err());
    assert!(adapter.render_request(&prompt).is_err());
    Ok(())
}

#[test]
fn responses_input_keeps_messages_between_tool_calls() -> Result<()> {
    let adapter = ResponsesAdapter;
    let mut prompt = adapter.parse_request(json!({"model":"served", "input":"task"}))?;
    let result = adapter.parse_response(json!({"output":[
        {"type":"function_call", "call_id":"call_a", "name":"inspect", "arguments":"{}"},
        {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"middle"}]},
        {"type":"function_call", "call_id":"call_b", "name":"inspect", "arguments":"{}"},
        {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"last"}]}
    ]}))?;
    prompt.messages = vec![Message {
        role: Role::Assistant,
        content: result.content,
    }];
    let rendered = adapter.render_request(&prompt)?;
    assert_eq!(rendered["input"][0]["type"], "function_call");
    assert_eq!(rendered["input"][0]["call_id"], "call_a");
    assert_eq!(rendered["input"][1]["content"][0]["text"], "middle");
    assert_eq!(rendered["input"][2]["type"], "function_call");
    assert_eq!(rendered["input"][2]["call_id"], "call_b");
    assert_eq!(rendered["input"][3]["content"][0]["text"], "last");
    Ok(())
}

#[test]
fn codex_message_phase_survives_stateless_replay() -> Result<()> {
    let adapter = ResponsesAdapter;
    let output = json!([
        {"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"checking"}]},
        {"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"done"}]}
    ]);
    let body = json!({"id":"resp_phase","status":"completed","output":output});
    assert!(output_replayable(&body));
    assert!(terminal_assistant_turn_commitment(&body).is_none());
    let result = adapter.parse_response(body)?;
    assert!(assistant_turn_commitment(&result.content).is_none());
    let mut prompt = adapter.parse_request(json!({"model":"served","input":"task"}))?;
    prompt.messages.push(Message {
        role: Role::Assistant,
        content: result.content.clone(),
    });
    for protocol in [
        ApiProtocol::Messages,
        ApiProtocol::ChatCompletions,
        ApiProtocol::Custom("fixture".into()),
    ] {
        assert!(
            crate::gateway::model::conversion::request_admission(&prompt, &protocol)
                .require_admitted()
                .is_err()
        );
    }
    let mut wrong_role = prompt.clone();
    wrong_role.messages[1].role = Role::User;
    assert!(adapter.admission(&wrong_role).require_admitted().is_err());
    assert_eq!(
        adapter.render_request(&prompt)?["input"]
            .as_array()
            .map(|items| &items[1..]),
        output.as_array().map(Vec::as_slice)
    );
    assert_eq!(
        adapter.render_response(&result, &prompt, "request")?["output"],
        output
    );
    let inbound = adapter.parse_request(json!({"model":"served","input":output}))?;
    assert_eq!(adapter.render_request(&inbound)?["input"], output);
    Ok(())
}

#[test]
fn empty_reasoning_output_is_preserved_without_granting_replay_authority() -> Result<()> {
    for content in [Value::Null, json!([])] {
        let item = json!({"type":"reasoning", "id":"rs_empty", "summary":[], "content":content, "encrypted_content":"opaque"});
        let response = json!({"output":[item.clone()]});
        assert!(output_replayable(&response));
        let result = ResponsesAdapter.parse_response(response)?;
        let mut prompt =
            ResponsesAdapter.parse_request(json!({"model":"served", "input":"task"}))?;
        prompt.messages.push(Message {
            role: Role::Assistant,
            content: result.content.clone(),
        });
        assert_eq!(
            ResponsesAdapter.render_response(&result, &prompt, "request")?["output"][0],
            item
        );
        assert!(ResponsesAdapter.render_request(&prompt).is_err());
    }
    Ok(())
}

#[test]
fn retained_reasoning_never_grants_plain_projection_authority() -> Result<()> {
    let adapter = ResponsesAdapter;
    let item = json!({"type":"reasoning", "id":"rs_private", "summary":[{"type":"summary_text","text":"summary"}], "content":[{"type":"reasoning_text","text":"reasoning"}], "encrypted_content":"opaque"});
    for metadata_only in [false, true] {
        let mut prompt = adapter.parse_request(json!({"model":"served", "input":"task"}))?;
        let retained = NativeReasoning::Responses(item.clone());
        let mut metadata = ProviderMetadata::new();
        if metadata_only {
            set_provider_metadata(&mut metadata, "openai", "reasoningItem", item.clone());
        }
        prompt.messages.push(Message {
            role: Role::Assistant,
            content: vec![Content::Reasoning {
                text: retained.visible_text(),
                native: (!metadata_only).then_some(retained),
                provider_metadata: metadata,
            }],
        });
        let original = prompt.clone();
        assert!(adapter.render_request(&prompt).is_err());
        let report = adapter.admission(&prompt);
        if !metadata_only {
            assert!(report.issues.iter().any(|issue| {
                issue.reason
                    == crate::gateway::conversion_report::ConversionReason::NativeReasoningAuthorityUnproven
            }));
        }
        assert!(report.require_admitted().is_err());
        assert_eq!(prompt, original);
        if let Content::Reasoning {
            provider_metadata, ..
        } = &mut prompt.messages[1].content[0]
        {
            set_provider_metadata(
                provider_metadata,
                "anthropic",
                "redactedThinking",
                json!(true),
            );
        }
        assert!(validate_reasoning_projection(&prompt.messages[1].content[0]).is_err());
        assert!(adapter.render_request(&prompt).is_err());
    }
    let mut prompt = adapter.parse_request(json!({"model":"served", "input":[item]}))?;
    if let Content::Reasoning {
        provider_metadata, ..
    } = &mut prompt.messages[0].content[0]
    {
        set_provider_metadata(
            provider_metadata,
            "openai",
            "reasoningItem",
            json!({"type":"reasoning", "id":"conflict", "summary":[]}),
        );
    }
    assert!(validate_reasoning_projection(&prompt.messages[0].content[0]).is_err());
    assert!(adapter.render_request(&prompt).is_err());
    Ok(())
}
