#[path = "operations.rs"]
mod operations;
#[path = "pipeline.rs"]
mod pipeline;
// Contract values only: these fixtures create no send/source authority.
use super::*;
use serde_json::{Value, json};
type TestResult = Result<(), Box<dyn std::error::Error>>;
fn sample() -> Value {
    json!({"schema_version":3,"receipt_format":1,"installation_id":"store","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","execution_revision":"revision","operations":[],"pipeline_scope":null,"hop_observations":[{"profile":"core_json_hop_callbacks_v1","attempt_ids":["auth-a","auth-b"],"closing_attempt_id":"auth-b","duration_ms":77,"end":"failed","partial":false},{"profile":"core_stream_open_callbacks_v1","attempt_ids":["last"],"closing_attempt_id":"last","duration_ms":88,"end":"failed","partial":false}],"observer_hops":[{"profile":"core_json_hop_observer_floor_v1","attempt_ids":["auth-a","auth-b"],"closing_attempt_id":"auth-b","duration_ms":0,"end":"completed"},{"profile":"core_stream_open_hop_observer_floor_v1","attempt_ids":["last"],"closing_attempt_id":"last","duration_ms":1,"end":"failed"}]})
}
#[test]
fn guard_and_observer_group_overlap_preserves_independent_clocks() -> TestResult {
    let value: ProviderTechnicalFacts = serde_json::from_value(sample())?;
    value.validate()?;
    assert!(value.operations.is_empty() && value.pipeline_scope.is_none());
    assert_eq!(value.observer_hops[0].duration_ms, 0);
    assert_eq!(value.hop_observations[0].duration_ms, Some(77));
    let bytes = serde_json::to_vec(&value)?;
    let decoded: ProviderTechnicalFacts = serde_json::from_slice(&bytes)?;
    assert_eq!(decoded, value);
    let mut wrong = sample();
    wrong["hop_observations"][0]["profile"] = json!("core_json_hop_observer_floor_v1");
    assert!(serde_json::from_value::<ProviderTechnicalFacts>(wrong).is_err());
    Ok(())
}
#[test]
fn original_group_order_foreign_arms_and_open_success_never_guess_a_match() -> TestResult {
    for pointer in [
        "/observer_hops/0/closing_attempt_id",
        "/observer_hops/0/attempt_ids/0",
    ] {
        let mut v = sample();
        *v.pointer_mut(pointer).ok_or("pointer")? = json!("foreign");
        assert!(
            serde_json::from_value::<ProviderTechnicalFacts>(v)?
                .validate()
                .is_err()
        );
    }
    let mut v = sample();
    v["observer_hops"].as_array_mut().ok_or("array")?.reverse();
    assert!(
        serde_json::from_value::<ProviderTechnicalFacts>(v)?
            .validate()
            .is_err()
    );
    let mut v = sample();
    v["observer_hops"][1]["end"] = json!("completed");
    assert!(
        serde_json::from_value::<ProviderTechnicalFacts>(v)?
            .validate()
            .is_err()
    );
    let mut v = sample();
    v["observer_hops"][0]["end"] = json!("dropped");
    assert!(
        serde_json::from_value::<ProviderTechnicalFacts>(v)?
            .validate()
            .is_err()
    );
    let mut v = sample();
    v["observer_hops"][1] = v["observer_hops"][0].clone();
    assert!(
        serde_json::from_value::<ProviderTechnicalFacts>(v)?
            .validate()
            .is_err()
    );
    Ok(())
}
#[test]
fn closed_objects_missing_null_duplicate_numbers_and_whole_byte_bound() -> TestResult {
    let raw = sample().to_string();
    for token in ["0.5", "-1", "18446744073709551616", "null", "true", "\"0\""] {
        let changed = raw.replace("\"duration_ms\":0", &format!("\"duration_ms\":{token}"));
        assert_ne!(changed, raw);
        assert!(serde_json::from_str::<ProviderTechnicalFacts>(&changed).is_err());
    }
    // The original RawValue decoder accepts exact integral decimal/exponent
    // values without an f64 roundtrip; lexical fractions with value zero are valid.
    for (token, expected) in [
        ("0.0", 0),
        ("321.0", 321),
        ("3.21e2", 321),
        ("18446744073709551615.0", u64::MAX),
    ] {
        let changed = raw.replace("\"duration_ms\":0", &format!("\"duration_ms\":{token}"));
        assert_ne!(changed, raw);
        let decoded: ProviderTechnicalFacts = serde_json::from_str(&changed)?;
        decoded.validate()?;
        assert_eq!(decoded.observer_hops[0].duration_ms, expected);
    }
    for field in [
        "operations",
        "pipeline_scope",
        "hop_observations",
        "observer_hops",
    ] {
        let mut missing = sample();
        missing.as_object_mut().ok_or("facts object")?.remove(field);
        assert!(
            serde_json::from_value::<ProviderTechnicalFacts>(missing).is_err(),
            "{field}"
        );
        if field != "pipeline_scope" {
            let mut null = sample();
            null[field] = Value::Null;
            assert!(
                serde_json::from_value::<ProviderTechnicalFacts>(null).is_err(),
                "{field}"
            );
        }
    }
    let mut wrapped = sample();
    wrapped["scope"] = sample();
    assert!(serde_json::from_value::<ProviderTechnicalFacts>(wrapped).is_err());
    assert!(serde_json::from_value::<ProviderTechnicalFacts>(json!([sample()])).is_err());
    assert!(
        serde_json::from_str::<ProviderTechnicalFacts>(&raw.replace(
            "\"schema_version\":3",
            "\"schema_version\":3,\"schema_version\":3"
        ))
        .is_err()
    );
    let mut v = sample();
    v["observer_hops"][0] = json!([
        "core_json_hop_observer_floor_v1",
        ["auth-a", "auth-b"],
        "auth-b",
        0,
        "completed"
    ]);
    assert!(serde_json::from_value::<ProviderTechnicalFacts>(v).is_err());
    let mut v = sample();
    v["observer_hops"][0]["duration_ms"] = json!(u64::MAX);
    serde_json::from_value::<ProviderTechnicalFacts>(v)?.validate()?;
    let mut v = sample();
    v["pipeline_scope"] = json!({"profile":"core_pipeline_finalized_v1","context_request_id":"x".repeat(65536),"request_duration_ms":0,"streamed":false,"stream":null});
    assert!(
        serde_json::from_value::<ProviderTechnicalFacts>(v)?
            .validate()
            .is_err()
    );
    Ok(())
}
