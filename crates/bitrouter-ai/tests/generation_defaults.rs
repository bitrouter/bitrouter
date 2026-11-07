//! Closed defaults and normalized identity, independent of protocol rendering.
use bitrouter_ai::tenancy::{
    generation_defaults::CanonicalGenerationDefaultsV1,
    ordered::{GenerationProtocol, OrderedCandidateSelectionV1},
    validation::Validate,
};
use serde_json::{Value, json};
type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn normalized_digest_has_a_fixed_vector_and_never_hashes_raw_input() -> TestResult {
    let a: CanonicalGenerationDefaultsV1 =
        serde_json::from_str(r#"{"stop":"END","seed":-7,"top_k":0,"schema_version":1}"#)?;
    let b: CanonicalGenerationDefaultsV1 =
        serde_json::from_str(r#"{ "schema_version":1, "top_k":0, "seed":-7, "stop":["END"] }"#)?;
    assert_eq!(a, b);
    assert_eq!(a.digest()?, b.digest()?);
    assert_eq!(
        a.digest()?.as_str(),
        "22c1c77860c87612e184635c2dda9bf1171097a20934eb7aa1e1c17bf740dfef"
    );
    let mut changed = a.clone();
    changed.seed = Some(0);
    assert_ne!(a.digest()?, changed.digest()?);
    assert!(!format!("{a:?}").contains("END"));
    Ok(())
}

#[test]
fn exact_integral_spellings_normalize_before_serialization_and_digest() -> TestResult {
    for (canonical, alternatives) in [
        (
            r#"{"schema_version":1,"max_tokens":25,"top_k":0,"seed":-7}"#,
            [
                r#"{"schema_version":1.0,"max_tokens":2.5e1,"top_k":0.0,"seed":-7e0}"#,
                r#"{"schema_version":1e0,"max_tokens":250e-1,"top_k":-0e999,"seed":-70e-1}"#,
            ],
        ),
        (
            r#"{"schema_version":1,"max_tokens":4294967295,"top_k":4294967295,"seed":9223372036854775807}"#,
            [
                r#"{"schema_version":1.00,"max_tokens":4294967295.0,"top_k":4.294967295e9,"seed":9.223372036854775807e18}"#,
                r#"{"schema_version":10e-1,"max_tokens":42949672950e-1,"top_k":429496729500e-2,"seed":9223372036854775807.000}"#,
            ],
        ),
        (
            r#"{"schema_version":1,"seed":-9223372036854775808}"#,
            [
                r#"{"schema_version":1.0,"seed":-9.223372036854775808e18}"#,
                r#"{"schema_version":1e0,"seed":-9223372036854775808.000}"#,
            ],
        ),
        (
            r#"{"schema_version":1,"seed":0}"#,
            [
                r#"{"schema_version":1.0,"seed":-0.000e999}"#,
                r#"{"schema_version":1e0,"seed":0e-999}"#,
            ],
        ),
        (
            r#"{"schema_version":1}"#,
            [r#"{"schema_version":1.0}"#, r#"{"schema_version":1e0}"#],
        ),
    ] {
        let expected: CanonicalGenerationDefaultsV1 = serde_json::from_str(canonical)?;
        expected.validate()?;
        for raw in alternatives {
            let actual: CanonicalGenerationDefaultsV1 = serde_json::from_str(raw)?;
            assert_eq!(actual, expected, "{raw}");
            assert_eq!(serde_json::to_string(&actual)?, canonical, "{raw}");
            assert_eq!(actual.digest()?, expected.digest()?, "{raw}");
        }
    }
    Ok(())
}

#[test]
fn fractional_and_out_of_range_integers_never_round_into_defaults() {
    for (field, invalid) in [
        ("schema_version", "1.00000000000000001"),
        ("schema_version", "256.0"),
        ("schema_version", "-1e0"),
        ("max_tokens", "25.000000000000001"),
        ("max_tokens", "4294967295.0000001"),
        ("max_tokens", "4.294967296e9"),
        ("max_tokens", "-1e0"),
        ("top_k", "0.00000000000000001"),
        ("top_k", "4294967296.0"),
        ("top_k", "-1.0"),
        ("seed", "-7.000000000000001"),
        ("seed", "9223372036854775807.0000001"),
        ("seed", "-9223372036854775808.0000001"),
        ("seed", "9.223372036854775808e18"),
        ("seed", "-9.223372036854775809e18"),
        ("seed", "1e999"),
        ("seed", "-1e999"),
        ("seed", "1e-999"),
    ] {
        let raw = if field == "schema_version" {
            format!("{{\"schema_version\":{invalid}}}")
        } else {
            format!("{{\"schema_version\":1,\"{field}\":{invalid}}}")
        };
        assert!(
            serde_json::from_str::<CanonicalGenerationDefaultsV1>(&raw).is_err(),
            "{raw}"
        );
    }
}

#[test]
fn defaults_are_nine_closed_typed_fields_without_new_provider_ranges() -> TestResult {
    let value = json!({"schema_version":1,"temperature":-20.0,"top_p":3.0,
        "max_tokens":0,"reasoning_effort":"high","top_k":0,"seed":i64::MIN,
        "stop":["a",""],"presence_penalty":-100.0,"frequency_penalty":100.0});
    serde_json::from_value::<CanonicalGenerationDefaultsV1>(value.clone())?.validate()?;
    for field in value.as_object().ok_or("object")?.keys() {
        let mut null = value.clone();
        null[field] = Value::Null;
        assert!(
            serde_json::from_value::<CanonicalGenerationDefaultsV1>(null).is_err(),
            "{field}"
        );
        let raw = value.to_string();
        let duplicate = format!("{{\"{field}\":{},{}", value[field], &raw[1..]);
        assert!(
            serde_json::from_str::<CanonicalGenerationDefaultsV1>(&duplicate).is_err(),
            "{field}"
        );
    }
    for (field, invalid) in [
        ("top_k", json!(-1)),
        ("max_tokens", json!(4294967296_u64)),
        ("seed", json!(1.5)),
        ("stop", json!([1])),
        ("reasoning_effort", json!("invented")),
        ("authorization", json!("secret")),
        ("model", json!("other")),
        ("store", json!(true)),
        ("explicit_nulls", json!(["temperature"])),
    ] {
        let mut bad = value.clone();
        bad[field] = invalid;
        assert!(
            serde_json::from_value::<CanonicalGenerationDefaultsV1>(bad).is_err(),
            "{field}"
        );
    }
    let mut nonfinite: CanonicalGenerationDefaultsV1 = serde_json::from_value(value)?;
    nonfinite.temperature = Some(f64::NAN);
    assert!(nonfinite.digest().is_err());
    Ok(())
}

#[test]
fn optional_digest_preserves_unused_bytes_and_binds_accepted_identity() -> TestResult {
    let raw = r#"{"schema_version":1,"admission_mode":"legacy","route":{"owner_kind":"tenant","owner_id":"t","resource_kind":"route","resource_id":"r","revision_id":"r1"},"candidate_indices":[255,0]}"#;
    let mut selection: OrderedCandidateSelectionV1 = serde_json::from_str(raw)?;
    assert_eq!(serde_json::to_string(&selection)?, raw);
    let old = selection.accept(GenerationProtocol::Messages)?;
    let defaults: CanonicalGenerationDefaultsV1 =
        serde_json::from_value(json!({"schema_version":1,"seed":0}))?;
    selection.generation_defaults_digest = Some(defaults.digest()?);
    let accepted = selection.accept(GenerationProtocol::Messages)?;
    assert_ne!(accepted, old);
    assert_eq!(
        accepted.generation_defaults_digest,
        selection.generation_defaults_digest
    );
    let mut missing_mode = serde_json::to_value(&selection)?;
    missing_mode
        .as_object_mut()
        .ok_or("object")?
        .remove("admission_mode");
    assert!(serde_json::from_value::<OrderedCandidateSelectionV1>(missing_mode).is_err());
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn defaults_schema_exposes_only_optional_nonnull_fields() -> TestResult {
    let schema = serde_json::to_value(schemars::schema_for!(CanonicalGenerationDefaultsV1))?;
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(["schema_version"]));
    assert_eq!(
        schema["properties"].as_object().ok_or("properties")?.len(),
        10
    );
    for name in [
        "temperature",
        "top_p",
        "max_tokens",
        "top_k",
        "seed",
        "presence_penalty",
        "frequency_penalty",
    ] {
        assert!(
            !schema["properties"][name].to_string().contains("null"),
            "{name}"
        );
    }
    for name in ["max_tokens", "top_k"] {
        assert_eq!(schema["properties"][name]["maximum"], json!(u32::MAX));
    }
    assert_eq!(schema["properties"]["seed"]["minimum"], json!(i64::MIN));
    assert_eq!(schema["properties"]["seed"]["maximum"], json!(i64::MAX));
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/specs/service-api/generation-defaults.schema.json");
    let rendered = serde_json::to_string_pretty(&schema)? + "\n";
    if std::env::var_os("BITROUTER_BLESS").is_some() {
        std::fs::write(&path, &rendered)?;
    }
    assert_eq!(std::fs::read_to_string(path)?, rendered);
    Ok(())
}
