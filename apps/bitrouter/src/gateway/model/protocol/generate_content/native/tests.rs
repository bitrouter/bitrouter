use super::*;
use bitrouter_ai::tenancy::generate_content::OrderedGenerateContentRequestV1;
use serde_json::json;
type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
fn input() -> crate::gateway::error::Result<Input> {
    Input::new(
        OrderedGenerateContentRequestV1 {
            schema_version: 1,
            model: "native/model".into(),
            stream: false,
            request: [("contents".into(), json!([{"parts":[{"text":"native"}]}]))].into(),
        },
        None,
    )
}

#[test]
fn native_generate_content_json_retains_full_signed_media_and_non_executable_calls() -> TestResult {
    let input = input()?;
    let body = json!({"responseId":"original", "modelVersion":"native-version", "candidates":[
        {"index":7,"content":{"role":"model","parts":[
            {"text":"", "thoughtSignature":"signed-empty"},
            {"text":"native", "thoughtSignature":"signed-text"},
            {"inlineData":{"mimeType":"image/png","data":"AA=="}},
            {"functionCall":{"id":"native-id", "name":"tool", "args":{"$serde_json::private::Number":"opaque", "sibling":[1.25,null]}}}
        ]},"finishReason":"MAX_TOKENS"},
        {"index":2,"content":{"parts":[{"fileData":{"mimeType":"image/png","fileUri":"retained://native"}}]},"finishReason":"STOP"}
    ],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":3}});
    let raw = serde_json::to_string(&body)?;
    let retained = value(&raw)?;
    assert_eq!(retained, body);
    let response = qualify_json(&input, retained)?;
    assert_eq!(input.response_body(&response)?, body);
    let other = self::input()?;
    assert!(other.response_body(&response).is_err());
    let output = crate::gateway::pipeline::types::PipelineOutput::GenerateContent(response);
    assert!(output.generation().is_none());
    Ok(())
}

#[test]
fn native_generate_content_json_shape_terminal_and_token_checks_are_real() -> TestResult {
    let input = input()?;
    for body in [
        json!({"error":{"code":500}}),
        json!({"promptFeedback":{"blockReason":"SAFETY"}}),
        json!({"candidates":[]}),
        json!({"candidates":[{"finishReason":null}]}),
        json!({"candidates":[{"index":null,"finishReason":"STOP"}]}),
        json!({"candidates":[{"finishReason":"STOP","content":null}]}),
        json!({"candidates":[{"finishReason":"STOP","content":{"parts":[null]}}]}),
        json!({"candidates":[{"index":0,"finishReason":"STOP"},{"index":0,"finishReason":"STOP"}]}),
        json!({"candidates":[{"finishReason":"STOP"},{"index":1,"finishReason":"STOP"}]}),
    ] {
        assert!(qualify_json(&input, body).is_err());
    }
    assert!(
        value(r#"{"candidates":[{"finishReason":"STOP","finish\u0052eason":"MAX_TOKENS"}]}"#)
            .is_err()
    );
    assert!(
        value(&format!(
            "{{\"opaque\":{}{}}}",
            "[".repeat(128),
            "]".repeat(128)
        ))
        .is_err()
    );
    let body = json!({"error":null,"candidates":[{"finishReason":"STOP","content":{"parts":[]}}]});
    qualify_json(&input, body)?;
    Ok(())
}

#[test]
fn native_generate_content_lifecycle_preserves_open_siblings_and_neutral_tail() -> TestResult {
    let mut state = lifecycle::Lifecycle::new(65536);
    assert!(state.observe(
        &json!({"responseId":"original", "modelVersion":"version", "candidates":[
            {"index":4,"finishReason":"STOP"},{"index":9,"content":{"parts":[{"text":"open"}]}}
        ]})
    )?);
    assert!(state.complete().is_err());
    assert!(
        state.observe(&json!({"promptFeedback":{"safetyRatings":[]},"candidates":[
            {"index":9,"content":{"parts":[{"text":"still-open"}]}}
        ]}))?
    );
    state.observe(&json!({"candidates":[{"index":9,"finishReason":"STOP"}]}))?;
    state.observe(&json!({"error":null,"usageMetadata":{"candidatesTokenCount":2}}))?;
    assert_eq!(state.complete()?, "STOP");
    assert!(state.observe(&json!({"promptFeedback":{}})).is_err());
    assert!(
        state
            .observe(&json!({"candidates":[{"index":4,"finishReason":null}]}))
            .is_err()
    );
    assert!(
        state
            .observe(&json!({"candidates":[{"index":9,"content":{"parts":[{"text":"late"}]}}]}))
            .is_err()
    );
    assert!(state.observe(&json!({"responseId":"changed"})).is_err());
    Ok(())
}

#[test]
fn native_generate_content_candidate_identity_modes_and_budget_do_not_mix() -> TestResult {
    for (first, next) in [
        (
            json!({"candidates":[{"content":{"parts":[]}}]}),
            json!({"candidates":[{"index":0,"finishReason":"STOP"}]}),
        ),
        (
            json!({"candidates":[{"index":0}]}),
            json!({"candidates":[{"finishReason":"STOP"}]}),
        ),
        (
            json!({"candidates":[{"index":2147483647_u64}]}),
            json!({"candidates":[{"index":2147483648_u64,"finishReason":"STOP"}]}),
        ),
    ] {
        let mut state = lifecycle::Lifecycle::new(65536);
        state.observe(&first)?;
        assert!(state.observe(&next).is_err());
    }
    for field in ["responseId", "modelVersion"] {
        for invalid in [json!(null), json!(""), json!(42)] {
            let mut state = lifecycle::Lifecycle::new(65536);
            let mut body = json!({"candidates":[{"finishReason":"STOP"}]});
            body[field] = invalid;
            assert!(state.observe(&body).is_err());
        }
    }
    let mut state = lifecycle::Lifecycle::new(1);
    assert!(
        state
            .observe(&json!({"candidates":[{"index":0,"finishReason":"STOP"}]}))
            .is_err()
    );
    Ok(())
}
