use bitrouter_ai::tenancy::{
    execution::ExecutionReceipt,
    usage_evidence::{ExecutionUsageEvidenceV1, UsageEvidenceCapabilitiesV1},
    validation::Validate,
};
use serde_json::{Value, json};
type TestResult = Result<(), Box<dyn std::error::Error>>;
fn document() -> Value {
    json!({"schema_version":1,"installation_id":"installation","tenant_id":"tenant","execution_id":"execution","capture":"captured","extractor_version":1,"estimation":"not_captured","estimate":null,"parent_execution_id":null,"child_execution_ids":[],"attempts":[{"attempt_id":"attempt","target":null,"outcome":"unknown","terminal_observed":false}],"frames":[{"sequence":0,"attempt_id":"attempt","protocol":"messages","source":"message_delta","semantics":"snapshot","quantities":[{"dimension":"output_tokens","value":0}],"unsupported":false,"invalid":false}],"truncated":false,"dropped_frames":0})
}
fn valid(value: Value) -> bool {
    serde_json::from_value::<ExecutionUsageEvidenceV1>(value)
        .is_ok_and(|value| value.validate().is_ok())
}
#[test]
fn required_fields_zero_absence_and_exact_numbers() -> TestResult {
    assert!(valid(document()));
    let value = document();
    let keys = value
        .as_object()
        .ok_or("object")?
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for key in keys {
        let mut missing = value.clone();
        missing.as_object_mut().ok_or("object")?.remove(&key);
        assert!(!valid(missing), "missing {key}");
    }
    let mut missing = document();
    missing["frames"][0]["quantities"][0]
        .as_object_mut()
        .ok_or("quantity")?
        .remove("value");
    assert!(!valid(missing));
    for token in ["-1", "9007199254740992", "9007199254740990.9", "0.1"] {
        let raw = serde_json::to_string(&document())?
            .replace("\"value\":0", &format!("\"value\":{token}"));
        assert!(serde_json::from_str::<ExecutionUsageEvidenceV1>(&raw).is_err());
    }
    let raw = serde_json::to_string(&document())?.replace("\"value\":0", "\"value\":0e0");
    serde_json::from_str::<ExecutionUsageEvidenceV1>(&raw)?.validate()?;
    let mut absent = document();
    absent["frames"][0]["quantities"] = json!([]);
    assert!(valid(absent));
    let mut invalid = document();
    invalid["frames"][0]["quantities"][0]["value"] = Value::Null;
    assert!(!valid(invalid.clone()));
    invalid["frames"][0]["invalid"] = true.into();
    assert!(valid(invalid));
    Ok(())
}
#[test]
fn rejects_forged_order_duplicate_dimensions_unknown_fields_and_unsupported_versions() {
    for (pointer, replacement) in [
        ("/schema_version", json!(2)),
        ("/extractor_version", json!(2)),
        ("/frames/0/sequence", json!(1)),
        ("/frames/0/attempt_id", json!("foreign")),
        ("/frames/0/source", json!("chat_chunk")),
        ("/frames/0/semantics", json!("delta")),
        ("/dropped_frames", json!(1)),
        ("/attempts/0/terminal_observed", json!(true)),
    ] {
        let mut v = document();
        if let Some(value) = v.pointer_mut(pointer) {
            *value = replacement;
        }
        assert!(!valid(v), "{pointer}");
    }
    let mut extra = document();
    extra["secret"] = json!("never accepted");
    assert!(!valid(extra));
    let mut duplicate = document();
    duplicate["frames"][0]["quantities"] =
        json!([{"dimension":"output_tokens","value":0},{"dimension":"output_tokens","value":1}]);
    assert!(!valid(duplicate));
    let mut historical = document();
    historical["capture"] = json!("not_captured");
    assert!(!valid(historical.clone()));
    historical["frames"] = json!([]);
    historical["attempts"] = json!([]);
    assert!(valid(historical));
}
#[test]
fn v1_receipt_rejects_evidence_fields_and_new_capabilities_are_separate() -> TestResult {
    let capabilities = UsageEvidenceCapabilitiesV1::new("installation".try_into()?);
    capabilities.validate()?;
    let receipt = json!({"installation_id":"installation","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","correlation_id":"correlation","execution_revision":"revision","state":"unknown","outcome":"unknown","attempts":[],"usage":{"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"provenance":"unknown","complete":false},"recorded_at":"2026-10-25T00:00:00Z"});
    serde_json::from_value::<ExecutionReceipt>(receipt.clone())?.validate()?;
    let mut widened = receipt;
    widened["usage_evidence"] = document();
    assert!(serde_json::from_value::<ExecutionReceipt>(widened).is_err());
    assert!(
        serde_json::from_value::<bitrouter_ai::tenancy::operation::TenantCapabilities>(
            serde_json::to_value(capabilities)?
        )
        .is_err()
    );
    Ok(())
}
#[cfg(feature = "schema")]
#[test]
fn dedicated_packaged_schemas_are_the_exported_schemas() -> TestResult {
    for (exported, packaged) in [
        (
            serde_json::to_value(schemars::schema_for!(ExecutionUsageEvidenceV1))?,
            include_str!("../src/tenancy/usage_evidence/execution.schema.json"),
        ),
        (
            serde_json::to_value(schemars::schema_for!(UsageEvidenceCapabilitiesV1))?,
            include_str!("../src/tenancy/usage_evidence/capabilities.schema.json"),
        ),
    ] {
        let packaged: Value = serde_json::from_str(packaged)?;
        assert_eq!(exported["properties"], packaged["properties"]);
        assert_eq!(exported["required"], packaged["required"]);
        assert_eq!(exported["additionalProperties"], json!(false));
    }
    Ok(())
}

#[test]
fn estimates_require_algorithm_version_coverage_and_exact_numeric_inputs() -> TestResult {
    let mut value = document();
    value["estimation"] = json!("captured");
    assert!(!valid(value.clone()));
    value["estimate"] = json!({"algorithm":"stream_character_ceiling","version":1,"characters_per_token":4,"coverage":"prompt_text_and_observed_text_reasoning","trigger":"disconnected","prompt_characters":5,"output_characters":9,"input_tokens":2,"output_tokens":3});
    assert!(valid(value.clone()));
    for (field, replacement) in [
        ("version", json!(2)),
        ("characters_per_token", json!(3)),
        ("input_tokens", json!(1)),
        ("output_tokens", json!(2)),
        ("coverage", json!("all_provider_work")),
    ] {
        let mut invalid = value.clone();
        invalid["estimate"][field] = replacement;
        assert!(!valid(invalid));
    }
    value["parent_execution_id"] = json!("execution");
    assert!(!valid(value));
    Ok(())
}

#[test]
fn unavailable_capture_is_distinct_and_cannot_claim_observations() -> TestResult {
    let mut value = document();
    value["capture"] = json!("unavailable");
    assert!(!valid(value.clone()));
    value["attempts"] = json!([]);
    value["frames"] = json!([]);
    assert!(valid(value.clone()));
    let decoded: ExecutionUsageEvidenceV1 = serde_json::from_value(value.clone())?;
    assert_eq!(
        decoded.capture,
        bitrouter_ai::tenancy::usage_evidence::Capture::Unavailable
    );
    for (field, replacement) in [
        ("parent_execution_id", json!("parent")),
        ("child_execution_ids", json!(["child"])),
        ("estimation", json!("not_used")),
        ("truncated", json!(true)),
    ] {
        let mut invalid = value.clone();
        invalid[field] = replacement;
        assert!(!valid(invalid), "{field}");
    }
    #[cfg(feature = "schema")]
    {
        let schema: Value = serde_json::from_str(include_str!(
            "../src/tenancy/usage_evidence/execution.schema.json"
        ))?;
        assert_eq!(
            schema["properties"]["capture"]["enum"],
            json!(["captured", "not_captured", "unavailable"])
        );
        assert_eq!(
            schema["allOf"][0]["if"]["properties"]["capture"]["enum"],
            json!(["not_captured", "unavailable"])
        );
    }
    Ok(())
}
