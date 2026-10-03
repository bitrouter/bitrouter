use super::*;
use serde_json::{Value, json};

#[test]
fn responses_reasoning_replay_preserves_empty_encrypted_and_stored_items_in_order() -> Result<()> {
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
    assert!(assistant_turn_commitment(&result.content).is_none());
    assert!(terminal_assistant_turn_commitment(&json!({"output":output})).is_none());
    let mut prompt = adapter.parse_request(json!({"model":"served", "input":"task"}))?;
    prompt.messages.push(Message {
        role: Role::Assistant,
        content: result.content.clone(),
    });
    adapter
        .validate_managed_prompt(&prompt)
        .map_err(BitrouterError::bad_request)?;
    let request = adapter.render_request(&prompt)?;
    assert_eq!(
        request["input"].as_array().map(|items| &items[1..]),
        output.as_array().map(Vec::as_slice)
    );
    let rendered = adapter.render_response(&result, &prompt, "request")?;
    assert_eq!(rendered["output"], output);
    let inbound = adapter.parse_request(json!({"model":"served", "input":output}))?;
    adapter
        .validate_managed_prompt(&inbound)
        .map_err(BitrouterError::bad_request)?;
    assert_eq!(adapter.render_request(&inbound)?["input"], output);
    Ok(())
}

#[test]
fn responses_reasoning_managed_replay_rejects_malformed_or_unsynchronized_items() -> Result<()> {
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
            "content",
            json!([{"type":"reasoning_text", "text":"unchecked"}]),
        ),
    ] {
        let mut item = valid.clone();
        item[key] = value;
        assert_eq!(
            validate_reasoning_history(&parse_reasoning_item(&item)),
            Err("responses_reasoning_item_invalid"),
            "{key}"
        );
    }
    let mut prompt = adapter.parse_request(json!({"model":"served", "input":[valid]}))?;
    let Content::Reasoning { text, .. } = &mut prompt.messages[0].content[0] else {
        return Err(BitrouterError::internal("fixture lost reasoning"));
    };
    *text = "changed".into();
    assert_eq!(
        adapter.validate_managed_prompt(&prompt),
        Err("responses_reasoning_item_invalid")
    );
    if let Content::Reasoning { text, .. } = &mut prompt.messages[0].content[0] {
        *text = "checked".into();
    }
    prompt.messages[0].role = Role::User;
    assert_eq!(
        adapter.validate_managed_prompt(&prompt),
        Err("responses_reasoning_role_invalid")
    );
    Ok(())
}

#[test]
fn responses_input_does_not_move_messages_across_tools_or_reasoning() -> Result<()> {
    let adapter = ResponsesAdapter;
    let mut prompt = adapter.parse_request(json!({"model":"served", "input":"task"}))?;
    let result = adapter.parse_response(json!({"output":[
        {"type":"function_call", "call_id":"call_a", "name":"inspect", "arguments":"{}"},
        {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"middle"}]},
        {"type":"reasoning", "id":"rs_a", "summary":[], "encrypted_content":"opaque"},
        {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"last"}]}
    ]}))?;
    prompt.messages = vec![Message {
        role: Role::Assistant,
        content: result.content,
    }];
    let rendered = adapter.render_request(&prompt)?;
    assert_eq!(rendered["input"][0]["type"], "function_call");
    assert_eq!(rendered["input"][1]["content"][0]["text"], "middle");
    assert_eq!(rendered["input"][2]["type"], "reasoning");
    assert_eq!(rendered["input"][3]["content"][0]["text"], "last");
    Ok(())
}
