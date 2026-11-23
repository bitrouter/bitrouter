//! Opaque supplemental numeric values retain standard serde_json semantics under
//! dependency feature unification, without relaxing duplicate-key detection.
use bitrouter_ai::tenancy::{
    generation_defaults::GenerationDefaultsEnvelope, validation::Validate,
};
use serde_json::Value;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn ut_supplemental_numbers_preserve_json_value_semantics() -> TestResult {
    let supplemental = r#"{"direct":0.25,"null":null,"vendor":{"fraction":0.25,"negative":-0.5,"large":18446744073709551616,"small":-9223372036854775809,"items":[1.2e-4,0.0,{"nested":0.1}]}}"#;
    let expected: Value = serde_json::from_str(supplemental)?;
    let envelope: GenerationDefaultsEnvelope = serde_json::from_str(&format!(
        r#"{{"schema_version":2,"supplemental":{supplemental},"configuration_defaults":"preserve_absence"}}"#
    ))?;
    envelope.validate()?;
    assert_eq!(serde_json::to_value(&envelope.supplemental)?, expected);
    let bytes = serde_json::to_vec(&envelope)?;
    let reopened: GenerationDefaultsEnvelope = serde_json::from_slice(&bytes)?;
    assert_eq!(reopened.supplemental, envelope.supplemental);
    assert_eq!(reopened.digest()?, envelope.digest()?);
    Ok(())
}

#[test]
fn ut_supplemental_number_normalization_still_refuses_duplicates() {
    for supplemental in [
        r#"{"x":0.25,"x":0.5}"#,
        r#"{"x":[{"a":0.25,"\u0061":0.5}]}"#,
    ] {
        assert!(serde_json::from_str::<GenerationDefaultsEnvelope>(&format!(r#"{{"schema_version":2,"supplemental":{supplemental},"configuration_defaults":"preserve_absence"}}"#)).is_err());
    }
}

#[test]
fn ut_supplemental_private_serde_keys_remain_opaque() -> TestResult {
    let embedded = r#"{"a":1,"a":2}"#;
    let expected = serde_json::json!({
        "raw_only": {"$serde_json::private::RawValue": embedded},
        "number_only": {"$serde_json::private::Number": "0.25"},
        "array": [{"$serde_json::private::RawValue":"true"}],
        "vendor": {
            "$serde_json::private::RawValue": embedded,
            "sibling": [ {"$serde_json::private::Number": "0.25"} ]
        }
    });
    let body = serde_json::json!({
        "schema_version": 2,
        "supplemental": expected,
        "configuration_defaults": "preserve_absence",
    });
    let decoded: GenerationDefaultsEnvelope = serde_json::from_slice(&serde_json::to_vec(&body)?)?;
    assert_eq!(serde_json::to_value(&decoded.supplemental)?, expected);
    let programmatic: GenerationDefaultsEnvelope = serde_json::from_value(body)?;
    assert_eq!(programmatic.supplemental, decoded.supplemental);
    let reopened: GenerationDefaultsEnvelope =
        serde_json::from_slice(&serde_json::to_vec(&decoded)?)?;
    assert_eq!(reopened.supplemental, decoded.supplemental);
    assert_eq!(reopened.digest()?, decoded.digest()?);
    Ok(())
}

#[test]
fn ut_supplemental_typed_decode_enforces_shared_depth_before_construction() -> TestResult {
    for object in [false, true] {
        for depth in [124, 125, 256] {
            let (open, close) = if object {
                (r#"{"n":"#, "}")
            } else {
                ("[", "]")
            };
            let nested = format!("{}null{}", open.repeat(depth), close.repeat(depth));
            let v2 = format!(
                r#"{{"schema_version":2,"supplemental":{{"nested":{nested}}},"configuration_defaults":"preserve_absence"}}"#
            );
            let decoded = serde_json::from_str::<GenerationDefaultsEnvelope>(&v2);
            assert_eq!(
                decoded.is_ok(),
                depth == 124,
                "object={object} depth={depth}"
            );
            if depth == 124 {
                decoded?.validate()?;
            }
            let mut value = Value::Null;
            for _ in 0..depth {
                value = if object {
                    serde_json::json!({"n": value})
                } else {
                    serde_json::json!([value])
                };
            }
            let body = serde_json::json!({
                "schema_version": 2,
                "supplemental": {"nested": value},
                "configuration_defaults": "preserve_absence",
            });
            assert_eq!(
                serde_json::from_value::<GenerationDefaultsEnvelope>(body).is_ok(),
                depth == 124
            );
        }
    }
    Ok(())
}
