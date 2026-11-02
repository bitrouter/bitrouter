//! Public request-local defaults contract and independent encoding vectors.
use bitrouter_ai::tenancy::{
    generation_defaults::{
        BODY_MEMBER, CanonicalGenerationDefaultsV1, ENVELOPE_BODY_MEMBER,
        GenerationDefaultsEnvelopeV1, MAX_DEFAULTS_BYTES,
    },
    validation::Validate,
};
use serde_json::{Value, json};
type TestResult = Result<(), Box<dyn std::error::Error>>;

fn envelope(supplemental: Value) -> Result<GenerationDefaultsEnvelopeV1, serde_json::Error> {
    serde_json::from_value(json!({"schema_version":1,"supplemental":supplemental}))
}

#[test]
fn envelope_digest_fixed_vector_sorted_nested_keys_and_distinct_domain() -> TestResult {
    let raw = r#"{"schema_version":1,"canonical":{"schema_version":1,"max_tokens":25},"supplemental":{"parallel_tool_calls":false,"store":false,"stream_options":{"include_obfuscation":false,"include_usage":true},"vendor_knob":{"mode":"preset"}}}"#;
    let value: GenerationDefaultsEnvelopeV1 = serde_json::from_str(raw)?;
    assert_eq!(
        value.digest()?.as_str(),
        "1677b5f7d8c59588a509c932581d556da60d770ebb23be61fbf9abf1c4f0fbe9"
    );
    let reordered: GenerationDefaultsEnvelopeV1 = serde_json::from_str(
        r#"{"supplemental":{"vendor_knob":{"mode":"preset"},"stream_options":{"include_usage":true,"include_obfuscation":false},"store":false,"parallel_tool_calls":false},"canonical":{"max_tokens":2.5e1,"schema_version":1.0},"schema_version":1e0}"#,
    )?;
    assert_eq!(value.digest()?, reordered.digest()?);
    let canonical: CanonicalGenerationDefaultsV1 =
        serde_json::from_value(json!({"schema_version":1,"max_tokens":25}))?;
    assert_ne!(value.digest()?, canonical.digest()?);
    let empty = envelope(json!({}))?;
    assert_ne!(
        empty.digest()?,
        envelope(json!({"vendor_knob":null}))?.digest()?
    );
    assert_ne!(
        envelope(json!({"vendor_knob":[1,2]}))?.digest()?,
        envelope(json!({"vendor_knob":[2,1]}))?.digest()?
    );
    assert!(!format!("{value:?}").contains("preset"));
    Ok(())
}

#[test]
fn envelope_rejects_duplicates_before_value_erasure_and_closed_outer_nulls() {
    for raw in [
        r#"{"schema_version":1,"supplemental":{},"supplemental":{}}"#,
        r#"{"schema_version":1,"supplemental":{"a":1,"\u0061":2}}"#,
        r#"{"schema_version":1,"supplemental":{"a":[{"b":1,"b":1}]}}"#,
        r#"{"schema_version":1,"supplemental":null}"#,
        r#"{"schema_version":1,"canonical":null,"supplemental":{}}"#,
        r#"{"schema_version":1,"canonical":{"schema_version":1,"seed":1,"seed":2},"supplemental":{}}"#,
        r#"{"schema_version":1,"supplemental":{},"extra":1}"#,
        r#"{"schema_version":1.00000000000000001,"supplemental":{}}"#,
        r#"{"schema_version":1}"#,
    ] {
        assert!(
            serde_json::from_str::<GenerationDefaultsEnvelopeV1>(raw).is_err(),
            "{raw}"
        );
    }
}

#[test]
fn exactly_fourteen_reserved_names_control_shaped_provider_data_remains_opaque() -> TestResult {
    for name in [
        "model",
        "messages",
        "stream",
        "temperature",
        "top_p",
        "max_tokens",
        "reasoning_effort",
        "top_k",
        "seed",
        "stop",
        "presence_penalty",
        "frequency_penalty",
        BODY_MEMBER,
        ENVELOPE_BODY_MEMBER,
    ] {
        assert!(envelope(json!({name:null}))?.validate().is_err(), "{name}");
        envelope(json!({"metadata":{name:"opaque"}}))?.validate()?;
    }
    for name in [
        "provider",
        "headers",
        "tenant_id",
        "api_key",
        "api_base",
        "authorization",
        "models",
        "fallbacks",
        "route",
        "routing",
        "candidate_indices",
        "extra_protocol",
        "supplemental_extra",
        "chat_stream_options",
        "generation_defaults_digest",
        "bitrouter_custom",
        "bitrouter-custom",
        "previous_response_id",
        "conversation",
        "context_management",
        "cachedContent",
        "max_completion_tokens",
        "max_output_tokens",
    ] {
        envelope(json!({name:{"opaque":true}}))?.validate()?;
    }
    Ok(())
}

#[test]
fn programmatic_depth_matches_complete_body_decoder_and_byte_limit() -> TestResult {
    let mut nested = Value::Null;
    for _ in 0..124 {
        nested = json!([nested]);
    }
    let mut defaults = envelope(json!({}))?;
    defaults.supplemental.insert("nested".into(), nested);
    defaults.validate()?;
    let body = format!(
        "{{\"{ENVELOPE_BODY_MEMBER}\":{}}}",
        serde_json::to_string(&defaults)?
    );
    serde_json::from_str::<Value>(&body)?;
    let nested = defaults.supplemental.remove("nested").ok_or("nested")?;
    defaults
        .supplemental
        .insert("nested".into(), json!([nested]));
    assert!(defaults.digest().is_err());
    let body = format!(
        "{{\"{ENVELOPE_BODY_MEMBER}\":{}}}",
        serde_json::to_string(&defaults)?
    );
    assert!(serde_json::from_str::<Value>(&body).is_err());
    let mut defaults = envelope(json!({"vendor_knob":""}))?;
    let overhead = serde_json::to_vec(&defaults)?.len();
    defaults.supplemental.insert(
        "vendor_knob".into(),
        Value::String("x".repeat(MAX_DEFAULTS_BYTES - overhead)),
    );
    defaults.validate()?;
    defaults
        .supplemental
        .get_mut("vendor_knob")
        .ok_or("knob")?
        .as_str()
        .ok_or("text")?;
    defaults.supplemental.insert("extra".into(), Value::Null);
    assert!(defaults.digest().is_err());
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn envelope_schema_is_closed_with_required_supplemental_and_nonnull_canonical() -> TestResult {
    let schema = serde_json::to_value(schemars::schema_for!(GenerationDefaultsEnvelopeV1))?;
    assert_eq!(
        schema["required"],
        json!(["schema_version", "supplemental"])
    );
    assert_eq!(schema["additionalProperties"], false);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/specs/service-api/generation-defaults-envelope.schema.json");
    let rendered = serde_json::to_string_pretty(&schema)? + "\n";
    if std::env::var_os("BITROUTER_BLESS").is_some() {
        std::fs::write(&path, &rendered)?;
    }
    assert_eq!(std::fs::read_to_string(path)?, rendered);
    Ok(())
}

#[test]
fn nested_canonical_uses_exact_integer_decoder_without_value_rounding() -> TestResult {
    let a: GenerationDefaultsEnvelopeV1 = serde_json::from_str(
        r#"{"schema_version":1,"canonical":{"schema_version":1,"seed":9223372036854775807},"supplemental":{}}"#,
    )?;
    let b: GenerationDefaultsEnvelopeV1 = serde_json::from_str(
        r#"{"schema_version":1.0,"canonical":{"schema_version":1e0,"seed":9.223372036854775807e18},"supplemental":{}}"#,
    )?;
    assert_eq!(a.digest()?, b.digest()?);
    for bad in [
        "9223372036854775808",
        "9.223372036854775808e18",
        "1.00000000000000001",
    ] {
        let raw = format!(
            "{{\"schema_version\":1,\"canonical\":{{\"schema_version\":1,\"seed\":{bad}}},\"supplemental\":{{}}}}"
        );
        assert!(serde_json::from_str::<GenerationDefaultsEnvelopeV1>(&raw).is_err());
    }
    Ok(())
}
