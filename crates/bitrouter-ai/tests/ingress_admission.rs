//! Ingress must refuse unclassified data before a lossy canonical Prompt exists.
use bitrouter_ai::conversion::{
    ConversionDisposition, ConversionEffect, ConversionLocation, ConversionReason,
    ConversionReport, ConversionStage,
};
use bitrouter_ai::error::ModelError;
use bitrouter_ai::protocol::inbound_adapter_for;
use bitrouter_ai::types::ApiProtocol;
use serde_json::{Value, json};
type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
fn refused(
    protocol: ApiProtocol,
    body: Value,
) -> Result<ConversionReport, Box<dyn std::error::Error + Send + Sync>> {
    let adapter = inbound_adapter_for(&protocol).ok_or("missing adapter")?;
    let error = adapter
        .parse_request(body)
        .err()
        .ok_or("ingress silently accepted discarded data")?;
    let ModelError::Incompatible { report } = error else {
        return Err("ingress lost its structured report".into());
    };
    assert!(!report.issues.is_empty());
    assert!(!format!("{report:?}").contains("secret"));
    assert!(!serde_json::to_string(&report)?.contains("secret"));
    for issue in &report.issues {
        assert_eq!(issue.stage, ConversionStage::RequestIngress);
    }
    Ok(report)
}
#[test]
fn unknown_blocks_on_all_four_wires_have_original_locations_and_no_payload() -> TestResult {
    let cases = [
        (
            ApiProtocol::ChatCompletions,
            json!({"model":"fixture","messages":[{"role":"user","content":[{"type":"text","text":"keep"},{"type":"future-secret","payload":"opaque-secret"}]}]}),
            ConversionLocation::MessageContent {
                message: 0,
                block: 1,
            },
        ),
        (
            ApiProtocol::Responses,
            json!({"model":"fixture","input":[{"role":"user","content":[{"type":"input_text","text":"keep"},{"type":"future-secret","payload":"opaque-secret"}]}]}),
            ConversionLocation::InputContent { item: 0, block: 1 },
        ),
        (
            ApiProtocol::Messages,
            json!({"model":"fixture","max_tokens":8,"messages":[{"role":"user","content":[{"type":"text","text":"keep"},{"type":"future-secret","payload":"opaque-secret"}]}]}),
            ConversionLocation::MessageContent {
                message: 0,
                block: 1,
            },
        ),
        (
            ApiProtocol::GenerateContent,
            json!({"contents":[{"role":"user","parts":[{"text":"keep"},{"future-secret":"opaque-secret"}]}]}),
            ConversionLocation::MessageContent {
                message: 0,
                block: 1,
            },
        ),
    ];
    for (protocol, body, location) in cases {
        let report = refused(protocol, body)?;
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].location, location);
        assert_eq!(report.issues[0].effect, ConversionEffect::Unknown);
        assert_eq!(
            report.issues[0].disposition,
            ConversionDisposition::RejectRequest
        );
    }
    Ok(())
}
#[test]
fn responses_collects_unknown_items_missing_roles_and_content_in_source_order() -> TestResult {
    let report = refused(
        ApiProtocol::Responses,
        json!({"model":"fixture","input":[{"type":"future-secret","id":"id-secret"},{"type":"message","content":"would disappear"},{"role":"user","content":[{"type":"input_text","text":"keep"},{"type":"future-secret"}]}]}),
    )?;
    assert_eq!(
        report
            .issues
            .iter()
            .map(|issue| issue.location)
            .collect::<Vec<_>>(),
        vec![
            ConversionLocation::InputItem { item: 0 },
            ConversionLocation::InputItem { item: 1 },
            ConversionLocation::InputContent { item: 2, block: 1 }
        ]
    );
    assert_eq!(
        report.issues[0].reason,
        ConversionReason::UnclassifiedInputItem
    );
    Ok(())
}
#[test]
fn nested_tool_result_arrays_cannot_drop_unknown_or_error_media_members() -> TestResult {
    let cases = [
        (
            ApiProtocol::ChatCompletions,
            json!({"model":"fixture","messages":[{"role":"tool","tool_call_id":"c","content":[{"type":"text","text":"kept"},{"type":"future-secret","text":"lost-secret"}]}]}),
            ConversionLocation::MessageContent {
                message: 0,
                block: 1,
            },
        ),
        (
            ApiProtocol::Responses,
            json!({"model":"fixture","input":[{"type":"function_call_output","call_id":"c","output":[{"type":"input_text","text":"keep"},{"type":"future-secret","data":"lost-secret"}]}]}),
            ConversionLocation::InputToolResultContent { item: 0, block: 1 },
        ),
        (
            ApiProtocol::Messages,
            json!({"model":"fixture","max_tokens":8,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":[{"type":"text","text":"keep"},{"type":"future-secret","data":"lost-secret"}]}]}]}),
            ConversionLocation::ToolResultContent {
                message: 0,
                block: 0,
                part: 1,
            },
        ),
        (
            ApiProtocol::Messages,
            json!({"model":"fixture","max_tokens":8,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","is_error":true,"content":[{"type":"text","text":"keep"},{"type":"image","source":{"type":"url","url":"https://example.test/image"}}]}]}]}),
            ConversionLocation::ToolResultContent {
                message: 0,
                block: 0,
                part: 1,
            },
        ),
    ];
    for (protocol, body, location) in cases {
        let report = refused(protocol, body)?;
        assert_eq!(report.issues[0].location, location);
    }
    Ok(())
}
#[test]
fn system_text_slots_cannot_silently_flatten_media() -> TestResult {
    let cases = [
        (
            ApiProtocol::ChatCompletions,
            json!({"model":"fixture","messages":[{"role":"system","content":[{"type":"text","text":"rules"},{"type":"image_url","image_url":{"url":"https://example.test/image"}}]}]}),
            ConversionLocation::MessageContent {
                message: 0,
                block: 1,
            },
        ),
        (
            ApiProtocol::Messages,
            json!({"model":"fixture","max_tokens":8,"system":[{"type":"text","text":"rules"},{"type":"image","source":{"type":"url","url":"https://example.test/image"}}],"messages":[{"role":"user","content":"hi"}]}),
            ConversionLocation::SystemContent { block: 1 },
        ),
        (
            ApiProtocol::GenerateContent,
            json!({"systemInstruction":{"parts":[{"text":"rules"},{"inlineData":{"mimeType":"image/png","data":"AA=="}}]},"contents":[{"role":"user","parts":[{"text":"hi"}]}]}),
            ConversionLocation::SystemContent { block: 1 },
        ),
    ];
    for (protocol, body, location) in cases {
        let report = refused(protocol, body)?;
        assert_eq!(report.issues[0].location, location);
        assert_eq!(report.issues[0].effect, ConversionEffect::TaskSemantics);
    }
    Ok(())
}
#[test]
fn unsigned_thinking_is_rejected_instead_of_erased() -> TestResult {
    let report = refused(
        ApiProtocol::Messages,
        json!({"model":"fixture","max_tokens":8,"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"lost-secret"},{"type":"text","text":"answer"}]}]}),
    )?;
    assert_eq!(
        report.issues[0].reason,
        ConversionReason::ReasoningContinuityMissing
    );
    assert_eq!(
        report.issues[0].disposition,
        ConversionDisposition::RejectReplay
    );
    Ok(())
}
#[test]
fn gemini_ambiguous_payload_does_not_choose_one_and_discard_another() -> TestResult {
    let report = refused(
        ApiProtocol::GenerateContent,
        json!({"contents":[{"role":"user","parts":[{"text":"lost-secret","functionResponse":{"name":"f","response":{"ok":true}}}]}]}),
    )?;
    assert_eq!(report.issues[0].effect, ConversionEffect::TaskSemantics);
    Ok(())
}

#[test]
fn unsupported_complete_content_values_are_not_lowered_to_empty_history() -> TestResult {
    let cases = [
        (
            ApiProtocol::ChatCompletions,
            json!({"model":"fixture","messages":[{"role":"user","content":{"future":"opaque-secret"}}]}),
            ConversionLocation::MessageContentValue { message: 0 },
        ),
        (
            ApiProtocol::Responses,
            json!({"model":"fixture","input":[{"role":"user","content":{"future":"opaque-secret"}}]}),
            ConversionLocation::InputContentValue { item: 0 },
        ),
        (
            ApiProtocol::Messages,
            json!({"model":"fixture","messages":[{"role":"user","content":{"future":"opaque-secret"}}]}),
            ConversionLocation::MessageContentValue { message: 0 },
        ),
        (
            ApiProtocol::Messages,
            json!({"model":"fixture","system":{"future":"opaque-secret"},"messages":[{"role":"user","content":"hello"}]}),
            ConversionLocation::SystemContentValue,
        ),
    ];
    for (protocol, body, location) in cases {
        let report = refused(protocol, body)?;
        assert_eq!(report.issues[0].location, location);
    }
    Ok(())
}

#[test]
fn known_media_json_continuity_and_declarations_remain_supported() -> TestResult {
    let cases = [
        (
            ApiProtocol::ChatCompletions,
            json!({"model":"fixture","messages":[{"role":"developer","content":[{"type":"text","text":"rules"}]},{"role":"user","content":[{"type":"text","text":""},{"type":"image_url","image_url":{"url":"https://example.test/image"}}]},{"role":"tool","tool_call_id":"c","content":{"opaque":"retain"}}]}),
        ),
        (
            ApiProtocol::Responses,
            json!({"model":"fixture","input":[{"type":"additional_tools","tools":[{"type":"function","name":"f","parameters":{"type":"object"}}]},{"role":"user","content":[{"type":"input_text","text":""},{"type":"input_image","image_url":"https://example.test/image"}]},{"type":"function_call_output","call_id":"c","output":[{"type":"input_text","text":"keep"},{"type":"input_file","file_id":"file-reference"}]}]}),
        ),
        (
            ApiProtocol::Messages,
            json!({"model":"fixture","system":[{"text":"implicit system text","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"retained","signature":"sig"},{"type":"tool_use","id":"c","name":"f","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":[{"type":"text","text":"kept"},{"type":"image","source":{"type":"url","url":"https://example.test/image"}}]},{"type":"mcp_tool_result","tool_use_id":"mcp","content":[{"type":"future","opaque":"retain"}]}]}]}),
        ),
        (
            ApiProtocol::GenerateContent,
            json!({"systemInstruction":{"parts":[{"text":"rules"}]},"contents":[{"role":"model","parts":[{"functionCall":{"id":"c","name":"f","args":{}},"thoughtSignature":"sig"}]},{"role":"user","parts":[{"functionResponse":{"id":"c","name":"f","response":{"opaque":"retain"}}},{"inlineData":{"mimeType":"image/png","data":"AA=="}}]}]}),
        ),
    ];
    for (protocol, body) in cases {
        let adapter = inbound_adapter_for(&protocol).ok_or("missing adapter")?;
        let prompt = adapter.parse_request(body)?;
        assert!(!prompt.messages.is_empty());
        // Accepted opaque JSON/media remain present in canonical semantics.
        let encoded = serde_json::to_value(&prompt)?.to_string();
        if protocol == ApiProtocol::Responses {
            assert!(encoded.contains("file-reference"));
            assert_eq!(prompt.tools.len(), 1);
        } else {
            assert!(encoded.contains("retain"));
        }
    }
    Ok(())
}

#[test]
fn refusal_reports_all_unknown_blocks_without_discarding_later_diagnostics() -> TestResult {
    let report = refused(
        ApiProtocol::ChatCompletions,
        json!({"model":"fixture","messages":[{"role":"user","content":[{"type":"future-secret"},{"type":"text","text":"keep"},{"type":"future-secret"}]},{"role":"assistant","content":[{"type":"future-secret"}]}]}),
    )?;
    assert_eq!(
        report
            .issues
            .iter()
            .map(|issue| issue.location)
            .collect::<Vec<_>>(),
        vec![
            ConversionLocation::MessageContent {
                message: 0,
                block: 0
            },
            ConversionLocation::MessageContent {
                message: 0,
                block: 2
            },
            ConversionLocation::MessageContent {
                message: 1,
                block: 0
            }
        ]
    );
    Ok(())
}
