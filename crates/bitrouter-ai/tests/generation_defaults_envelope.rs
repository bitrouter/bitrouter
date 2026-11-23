//! Version-two vectors freeze the layer choice independently of effective output.
use bitrouter_ai::tenancy::{
    generation_defaults::{
        ConfigurationDefaults, ENVELOPE_BODY_MEMBER, GenerationDefaultsEnvelope, MAX_DEFAULTS_BYTES,
    },
    validation::Validate,
};
use serde_json::{Value, json};
type TestResult = Result<(), Box<dyn std::error::Error>>;

fn envelope(supplemental: Value) -> Result<GenerationDefaultsEnvelope, serde_json::Error> {
    serde_json::from_value(
        json!({"schema_version":2,"supplemental":supplemental,"configuration_defaults":"preserve_absence"}),
    )
}

#[test]
fn envelope_fixed_vectors_layer_choice_field_order() -> TestResult {
    let raw = r#"{"schema_version":2,"canonical":{"schema_version":1,"max_tokens":25},"supplemental":{"parallel_tool_calls":false,"store":false,"stream_options":{"include_obfuscation":false,"include_usage":true},"vendor_knob":{"mode":"preset"}},"configuration_defaults":"preserve_absence"}"#;
    let mut value: GenerationDefaultsEnvelope = serde_json::from_str(raw)?;
    assert_eq!(
        value.digest()?.as_str(),
        "a9bc4dda6575911ae1e621daef2c6be5fadd5d2422fc61a30a0b78e1c0b9fe38"
    );
    assert_eq!(serde_json::to_string(&value)?, raw);
    let reordered: GenerationDefaultsEnvelope = serde_json::from_str(
        r#"{"configuration_defaults":"preserve_absence","supplemental":{"vendor_knob":{"mode":"preset"},"stream_options":{"include_usage":true,"include_obfuscation":false},"store":false,"parallel_tool_calls":false},"canonical":{"max_tokens":2.5e1,"schema_version":1.0},"schema_version":2e0}"#,
    )?;
    assert_eq!(value.digest()?, reordered.digest()?);
    value.configuration_defaults = ConfigurationDefaults::Apply;
    assert_eq!(
        value.digest()?.as_str(),
        "cdf93ea8ba613fc3e70513a87bd7d8e6981f1d4479e177ee435a087b196a658a"
    );
    Ok(())
}

#[test]
fn envelope_requires_exact_mode_version_and_unique_raw_keys() -> TestResult {
    let valid = r#"{"schema_version":2,"supplemental":{"nested":{"k":1}},"configuration_defaults":"preserve_absence"}"#;
    let good: GenerationDefaultsEnvelope = serde_json::from_str(valid)?;
    good.validate()?;
    for raw in [
        valid.replace(",\"configuration_defaults\":\"preserve_absence\"", ""),
        valid.replace("\"preserve_absence\"", "null"),
        valid.replace("\"preserve_absence\"", "\"unknown\""),
        valid.replace("\"preserve_absence\"", "false"),
        valid.replace(
            "\"preserve_absence\"",
            "\"apply\",\"configuration_defaults\":\"apply\"",
        ),
        valid.replace("\"k\":1", "\"k\":1,\"\\u006b\":1"),
        valid.replace(
            "\"schema_version\":2",
            "\"schema_version\":2,\"schema_version\":2",
        ),
        valid.replace(
            "\"schema_version\":2",
            "\"schema_version\":2.00000000000000001",
        ),
        valid.replace("\"supplemental\":", "\"canonical\":null,\"supplemental\":"),
        valid.replace("\"supplemental\":", "\"unknown\":{},\"supplemental\":"),
    ] {
        assert!(
            serde_json::from_str::<GenerationDefaultsEnvelope>(&raw).is_err(),
            "{raw}"
        );
    }
    for version in [0, 1, 3, 255] {
        let mut wrong = good.clone();
        wrong.schema_version = version;
        assert!(wrong.digest().is_err());
    }
    let mut payload = serde_json::to_value(&good)?;
    payload["schema_version"] = json!(1);
    let unsupported: GenerationDefaultsEnvelope = serde_json::from_value(payload)?;
    assert!(unsupported.validate().is_err());
    Ok(())
}

#[test]
fn envelope_exact_canonical_integers_and_reserved_names() -> TestResult {
    let a: GenerationDefaultsEnvelope = serde_json::from_str(
        r#"{"schema_version":2,"canonical":{"schema_version":1,"seed":9223372036854775807},"supplemental":{},"configuration_defaults":"apply"}"#,
    )?;
    let b: GenerationDefaultsEnvelope = serde_json::from_str(
        r#"{"schema_version":2.0,"canonical":{"schema_version":1e0,"seed":9.223372036854775807e18},"supplemental":{},"configuration_defaults":"apply"}"#,
    )?;
    assert_eq!(a.digest()?, b.digest()?);
    for bad in [
        "9223372036854775808",
        "9.223372036854775808e18",
        "1.00000000000000001",
    ] {
        let raw = format!(
            r#"{{"schema_version":2,"canonical":{{"schema_version":1,"seed":{bad}}},"supplemental":{{}},"configuration_defaults":"apply"}}"#
        );
        assert!(serde_json::from_str::<GenerationDefaultsEnvelope>(&raw).is_err());
    }
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
        ENVELOPE_BODY_MEMBER,
    ] {
        assert!(envelope(json!({name:null}))?.digest().is_err());
        envelope(json!({"nested":{name:null}}))?.validate()?;
    }
    envelope(json!({"provider":"opaque","headers":{"authorization":"body"},"tenant_id":"opaque","configuration_defaults":"opaque","bitrouter_private":"opaque","max_completion_tokens":0,"max_output_tokens":null}))?.validate()?;
    Ok(())
}

#[test]
fn envelope_depth_and_bytes_include_larger_required_layer_choice() -> TestResult {
    let mut nested = Value::Null;
    for _ in 0..124 {
        nested = json!([nested]);
    }
    let mut value = envelope(json!({}))?;
    value.supplemental.insert("nested".into(), nested);
    value.validate()?;
    let inner = value.supplemental.remove("nested").ok_or("nested")?;
    value.supplemental.insert("nested".into(), json!([inner]));
    assert!(value.digest().is_err());
    let mut value = envelope(json!({"large":""}))?;
    let overhead = serde_json::to_vec(&value)?.len();
    value.supplemental.insert(
        "large".into(),
        json!("x".repeat(MAX_DEFAULTS_BYTES - overhead)),
    );
    value.validate()?;
    value.supplemental.insert("extra".into(), Value::Null);
    assert!(value.digest().is_err());
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn envelope_schema_requires_explicit_configuration_choice() -> TestResult {
    let schema = serde_json::to_value(schemars::schema_for!(GenerationDefaultsEnvelope))?;
    assert_eq!(
        schema["required"],
        json!(["schema_version", "supplemental", "configuration_defaults"])
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
