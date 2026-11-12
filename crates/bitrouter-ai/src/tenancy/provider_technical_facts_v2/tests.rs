//! Value-only contract controls, never authentication or provider authority.
use super::*;
use serde_json::json;
type TestResult = Result<(), Box<dyn std::error::Error>>;
fn value() -> serde_json::Value {
    json!({"schema_version":2,"receipt_format":1,"installation_id":"store","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","execution_revision":"revision","operations":[{"attempt_ids":["attempt-a","attempt-b"],"selected_attempt_id":"attempt-b","streamed":false,"upstream_duration_ms":7,"first_semantic":null,"generation_duration_ms":null,"partial":false}],"pipeline_scope":{"profile":"core_pipeline_finalized_v1","context_request_id":"different-own-context","request_duration_ms":321,"streamed":true,"stream":{"profile":"core_pipeline_merged_stream_v1","source_attempt_id":"attempt-a","terminal_attempt_id":"attempt-b","upstream_duration_ms":250,"first_semantic":{"ttft_ms":123,"kind":"reasoning"},"generation_duration_ms":127,"partial":false}},"hop_observations":[{"profile":"core_json_hop_callbacks_v1","attempt_ids":["attempt-a","attempt-b"],"closing_attempt_id":"attempt-b","duration_ms":9,"end":"failed","partial":false}]})
}
fn receipt() -> Result<ExecutionReceipt, Box<dyn std::error::Error>> {
    let usage = json!({"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"provenance":"unknown","complete":false});
    Ok(serde_json::from_value(
        json!({"installation_id":"store","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","correlation_id":"correlation","execution_revision":"revision","state":"incomplete","outcome":"failed","attempts":[{"attempt_id":"attempt-a","target":null,"outcome":"failed","usage":usage,"native_usage":[]},{"attempt_id":"attempt-b","target":null,"outcome":"failed","usage":usage,"native_usage":[]}],"usage":usage,"recorded_at":"2026-01-01T00:00:00Z"}),
    )?)
}
#[test]
fn own_context_stream_and_hop_values_never_replace_original_operation_or_receipt() -> TestResult {
    let facts: ProviderTechnicalFactsV2 = serde_json::from_value(value())?;
    let original = receipt()?;
    let bytes = serde_json::to_vec(&original)?;
    facts.validate_against(&original)?;
    let scope = facts.pipeline_scope.as_ref().ok_or("scope")?;
    assert_ne!(scope.context_request_id, original.correlation_id.as_str());
    assert_ne!(scope.context_request_id, original.execution_id.as_str());
    assert_eq!(scope.request_duration_ms, 321);
    assert!(scope.streamed);
    let stream = scope.stream.as_ref().ok_or("stream")?;
    assert_ne!(stream.source_attempt_id, stream.terminal_attempt_id);
    assert_eq!(stream.upstream_duration_ms, Some(250));
    assert_eq!(stream.first_semantic.as_ref().ok_or("first")?.ttft_ms, 123);
    assert_eq!(stream.generation_duration_ms, Some(127));
    let view = facts.operation_view().ok_or("operation view")?;
    view.validate_against(&original)?;
    assert_eq!(view.schema_version, 1);
    assert_eq!(view.operations[0].upstream_duration_ms, Some(7));
    assert_eq!(facts.hop_observations[0].duration_ms, Some(9));
    assert_eq!(serde_json::to_vec(&original)?, bytes);
    Ok(())
}
#[test]
fn missing_scope_zero_single_semantic_and_source_prefix_remain_distinct() -> TestResult {
    let mut input = value();
    input["pipeline_scope"] = json!(null);
    let absent: ProviderTechnicalFactsV2 = serde_json::from_value(input)?;
    absent.validate()?;
    assert!(absent.pipeline_scope.is_none());
    let mut input = value();
    input["pipeline_scope"]["request_duration_ms"] = json!(0);
    input["pipeline_scope"]["stream"]["upstream_duration_ms"] = json!(0);
    input["pipeline_scope"]["stream"]["first_semantic"]["ttft_ms"] = json!(0);
    input["pipeline_scope"]["stream"]["generation_duration_ms"] = json!(0);
    let zero: ProviderTechnicalFactsV2 = serde_json::from_value(input.clone())?;
    zero.validate()?;
    assert_eq!(
        zero.pipeline_scope
            .as_ref()
            .ok_or("scope")?
            .request_duration_ms,
        0
    );
    input["pipeline_scope"]["stream"]["source_attempt_id"] = json!(null);
    let missing: ProviderTechnicalFactsV2 = serde_json::from_value(input.clone())?;
    assert!(missing.validate().is_err());
    input["pipeline_scope"]["stream"]["partial"] = json!(true);
    let partial: ProviderTechnicalFactsV2 = serde_json::from_value(input)?;
    partial.validate()?;
    let mut prefix = value();
    prefix["pipeline_scope"]["stream"]["upstream_duration_ms"] = json!(null);
    prefix["pipeline_scope"]["stream"]["partial"] = json!(true);
    let prefix: ProviderTechnicalFactsV2 = serde_json::from_value(prefix)?;
    prefix.validate()?;
    let stream = prefix
        .pipeline_scope
        .as_ref()
        .ok_or("scope")?
        .stream
        .as_ref()
        .ok_or("partial stream")?;
    assert!(stream.upstream_duration_ms.is_none() && stream.first_semantic.is_some());
    let mut json_scope = value();
    json_scope["pipeline_scope"]["streamed"] = json!(false);
    assert!(
        serde_json::from_value::<ProviderTechnicalFactsV2>(json_scope)?
            .validate()
            .is_err()
    );
    Ok(())
}
#[test]
fn closed_objects_exact_numbers_and_original_arm_membership_refuse_forgery() -> TestResult {
    for key in ["pipeline_scope", "hop_observations"] {
        let mut input = value();
        input.as_object_mut().ok_or("object")?.remove(key);
        assert!(serde_json::from_value::<ProviderTechnicalFactsV2>(input).is_err());
    }
    for (pointer, bad) in [
        (
            "/pipeline_scope",
            json!(["core_pipeline_finalized_v1", "id", 0, false, null]),
        ),
        (
            "/pipeline_scope/stream",
            json!([
                "core_pipeline_merged_stream_v1",
                null,
                null,
                0,
                null,
                null,
                true
            ]),
        ),
        (
            "/hop_observations/0",
            json!([
                "core_json_hop_callbacks_v1",
                ["attempt-a"],
                "attempt-a",
                0,
                "completed",
                false
            ]),
        ),
    ] {
        let mut input = value();
        *input.pointer_mut(pointer).ok_or("pointer")? = bad;
        assert!(serde_json::from_value::<ProviderTechnicalFactsV2>(input).is_err());
    }
    let raw = serde_json::to_string(&value())?;
    for token in [
        "0.5",
        "-1",
        "18446744073709551616",
        "18446744073709551615.1",
        "true",
        "\"0\"",
    ] {
        let changed = raw.replace(
            "\"request_duration_ms\":321",
            &format!("\"request_duration_ms\":{token}"),
        );
        assert_ne!(changed, raw);
        assert!(serde_json::from_str::<ProviderTechnicalFactsV2>(&changed).is_err());
    }
    // Exact JSON integers are values, not a lexical spelling: the original
    // RawValue decoder intentionally accepts integral decimals/exponents.
    for (token, expected) in [
        ("0.0", 0),
        ("321.0", 321),
        ("3.21e2", 321),
        ("18446744073709551615.0", u64::MAX),
    ] {
        let changed = raw.replace(
            "\"request_duration_ms\":321",
            &format!("\"request_duration_ms\":{token}"),
        );
        assert_ne!(changed, raw);
        let decoded: ProviderTechnicalFactsV2 = serde_json::from_str(&changed)?;
        decoded.validate_against(&receipt()?)?;
        assert_eq!(
            decoded
                .pipeline_scope
                .as_ref()
                .ok_or("scope")?
                .request_duration_ms,
            expected
        );
    }
    let duplicate = raw.replacen(
        "\"schema_version\":2",
        "\"schema_version\":2,\"schema_version\":2",
        1,
    );
    assert!(serde_json::from_str::<ProviderTechnicalFactsV2>(&duplicate).is_err());
    let original = receipt()?;
    for pointer in [
        "/pipeline_scope/stream/source_attempt_id",
        "/pipeline_scope/stream/terminal_attempt_id",
        "/hop_observations/0/closing_attempt_id",
    ] {
        let mut input = value();
        *input.pointer_mut(pointer).ok_or("pointer")? = json!("foreign");
        assert!(
            serde_json::from_value::<ProviderTechnicalFactsV2>(input)?
                .validate_against(&original)
                .is_err()
        );
    }
    let mut reversed = value();
    reversed["pipeline_scope"]["stream"]["source_attempt_id"] = json!("attempt-b");
    reversed["pipeline_scope"]["stream"]["terminal_attempt_id"] = json!("attempt-a");
    assert!(
        serde_json::from_value::<ProviderTechnicalFactsV2>(reversed)?
            .validate_against(&original)
            .is_err()
    );
    let mut unobserved = value();
    unobserved["hop_observations"][0]["end"] = json!("unobserved");
    unobserved["hop_observations"][0]["partial"] = json!(true);
    assert!(
        serde_json::from_value::<ProviderTechnicalFactsV2>(unobserved.clone())?
            .validate()
            .is_err()
    );
    unobserved["hop_observations"][0]["duration_ms"] = json!(null);
    serde_json::from_value::<ProviderTechnicalFactsV2>(unobserved)?.validate_against(&original)?;
    let mut input = value();
    input["hop_observations"][0]["attempt_ids"] = json!(["attempt-b", "attempt-a"]);
    input["hop_observations"][0]["closing_attempt_id"] = json!("attempt-a");
    assert!(
        serde_json::from_value::<ProviderTechnicalFactsV2>(input)?
            .validate_against(&original)
            .is_err()
    );
    Ok(())
}
