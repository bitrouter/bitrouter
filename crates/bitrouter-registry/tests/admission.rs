use bitrouter_registry::document::{
    Document, ReaderPolicy, RegistryError, SPEC_REVISION, parse_json,
};
use registry_json as serde_json;
use serde_json::{Value, json};

fn minimal() -> Value {
    json!({"version":1,"spec_revision":SPEC_REVISION,"as_of":"2026-10-23T00:00:00Z","data":{"models":[],"providers":[]}})
}

#[test]
fn schema_checks_positive_weight_without_binary64_underflow() -> Result<(), RegistryError> {
    for (raw, valid) in [
        ("0.00000000000000000001e-308", true),
        ("-0.00000000000000000001e-308", false),
        ("-0e-308", false),
    ] {
        let mut value = minimal();
        let mut p = provider();
        p["weight"] = parse_json(raw.as_bytes())?;
        value["data"]["providers"] = json!([p]);
        assert_eq!(
            Document::from_value(value, &ReaderPolicy::default()).is_ok(),
            valid,
            "{raw}"
        );
    }
    Ok(())
}

#[test]
fn packaged_schema_matches_normative_copy() -> Result<(), Box<dyn std::error::Error>> {
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/specs/registry-v1/registry.schema.json");
    assert_eq!(
        std::fs::read_to_string(file)?,
        bitrouter_registry::document::SCHEMA
    );
    Ok(())
}

#[test]
fn exact_integer_forms_private_object_keys_and_unknown_rate_members_roundtrip()
-> Result<(), RegistryError> {
    use bitrouter_registry::document::integer;
    for spelling in ["1.0", "1e3", "18446744073709551615"] {
        let value = parse_json(spelling.as_bytes())?;
        assert!(integer(&value).is_some(), "{spelling}");
    }
    for spelling in ["1.0000000000000000001", "18446744073709551616"] {
        assert!(
            integer(&parse_json(spelling.as_bytes())?).is_none(),
            "{spelling}"
        );
    }
    let object = parse_json(br#"{"future":{"$serde_json::private::Number":"123","sibling":1}}"#)?;
    assert!(object["future"].is_object());
    assert_eq!(object["future"]["$serde_json::private::Number"], "123");
    let mut value = minimal();
    let mut p = provider();
    p["models"][0]["interfaces"][0]["pricing"] = json!({"kind":"token","currency":"USD","input_tokens":{"text":{"future":true}},"output_tokens":{"no_cache":"annotation"}});
    value["data"]["providers"] = json!([p]);
    Document::from_value(value.clone(), &ReaderPolicy::default())?;
    value["data"]["providers"][0]["models"][0]["interfaces"][0]["limits"] =
        json!({"context_window":parse_json(b"1.0000000000000000001")?});
    assert!(Document::from_value(value, &ReaderPolicy::default()).is_err());
    Ok(())
}

#[test]
fn known_url_and_http_auth_rules_reject_malformed_input() {
    for base in [
        "https://",
        "https://user:secret@api.example.test/v1",
        "https://api.example.test/v1#fragment",
    ] {
        let mut value = minimal();
        let mut p = provider();
        p["api_base"] = json!(base);
        value["data"]["providers"] = json!([p]);
        assert!(Document::from_value(value, &ReaderPolicy::default()).is_err());
    }
    for auth in [
        json!({"kind":"bearer"}),
        json!({"kind":"header","env":"KEY"}),
        json!({"kind":"bearer","env":"KEY","extra_headers":{"Authorization":"collision"}}),
        json!({"kind":"none","extra_headers":{"x-a":"one","X-A":"two"}}),
        json!({"kind":"none","extra_headers":{"x-a":"one\r\ntwo"}}),
    ] {
        let mut value = minimal();
        let mut p = provider();
        p["auth"] = auth;
        value["data"]["providers"] = json!([p]);
        assert!(Document::from_value(value, &ReaderPolicy::default()).is_err());
    }
}

#[test]
fn valid_documentation_fragments_and_numeric_depth_boundary_are_accepted()
-> Result<(), RegistryError> {
    let mut value = minimal();
    let mut p = provider();
    p["doc_url"] = json!("https://example.test/docs#pricing");
    value["data"]["providers"] = json!([p]);
    Document::from_value(value, &ReaderPolicy::default())?;
    let text = format!("{}0.123{}", "[".repeat(64), "]".repeat(64));
    // parse_json performs the duplicate-key pass; from_value applies actual
    // structural depth. A decimal token must not count as a private map layer.
    let parsed = parse_json(text.as_bytes())?;
    assert!(parsed.is_array());
    Ok(())
}

fn provider() -> Value {
    json!({"name":"example","status":"active","billing":"usage","models":[{"provider_model_id":"Vendor/A:B%1","status":"active","interfaces":[{"protocol":"chat_completions","operation":"generation"}]}]})
}

#[test]
fn accepts_worked_example_and_preserves_exact_unknown_information()
-> Result<(), Box<dyn std::error::Error>> {
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/specs/registry-v1/examples/registry.json");
    let document = Document::parse(&std::fs::read(file)?, &ReaderPolicy::default())?;
    let mut original = document.into_value();
    original["data"]["providers"][0]["future"] = json!({"value":null,"as_of":"not a timestamp","available_until":17,"curation":{"checked_at":42}});
    original["data"]["providers"][0]["implementation_private_key"] =
        json!({"$serde_json::private::Number":"123","sibling":"retained"});
    original["data"]["providers"][0]["models"][0]["interfaces"][0]["future_rate"] =
        parse_json(b"0.100000000000000000000000000001")?;
    let bytes = serde_json::to_vec(&original)?;
    let roundtrip = Document::parse(&bytes, &ReaderPolicy::default())?;
    assert_eq!(roundtrip.value(), &original);
    assert!(serde_json::to_string(roundtrip.value())?.contains("0.100000000000000000000000000001"));
    Ok(())
}

#[test]
fn version_revision_and_envelope_features_fail_with_upgrade_guidance() -> Result<(), RegistryError>
{
    let mut value = minimal();
    for version in [json!(null), json!(2), json!(1.5), json!("1")] {
        value["version"] = version;
        let error = Document::from_value(value.clone(), &ReaderPolicy::default())
            .err()
            .ok_or_else(|| RegistryError::invalid("", "Invalid version was admitted"))?;
        assert_eq!(error.code, "registry.unsupported_version");
        assert!(error.recovery.contains("Upgrade"));
    }
    value = minimal();
    value["spec_revision"] = json!("1.0.0-beta.7");
    assert!(Document::from_value(value.clone(), &ReaderPolicy::default()).is_err());
    value["spec_revision"] = json!("1.53.107+future");
    let policy = ReaderPolicy {
        stable_v1: true,
        ..ReaderPolicy::default()
    };
    Document::from_value(value.clone(), &policy)?;
    value["requires"] = json!(["future.required.v1"]);
    let error = Document::from_value(value, &policy)
        .err()
        .ok_or_else(|| RegistryError::invalid("", "Unsupported requirement was admitted"))?;
    assert_eq!(error.code, "registry.unsupported_feature");
    Ok(())
}

#[test]
fn unknown_record_vocabulary_is_retained_but_malformed_known_fields_fail()
-> Result<(), RegistryError> {
    let mut value = minimal();
    value["data"]["providers"] = json!([provider()]);
    value["data"]["providers"][0]["access"] = json!("future_consent");
    value["data"]["providers"][0]["auth"] = json!({"kind":"future_signing","opaque":[1,2]});
    value["data"]["providers"][0]["models"][0]["interfaces"].as_array_mut().ok_or_else(||RegistryError::invalid("","Missing interfaces"))?.push(json!({"protocol":"future_media","operation":"dream","requires":["future.media.v1"],"pricing":{"kind":"future_bundle","future":null}}));
    let admitted = Document::from_value(value.clone(), &ReaderPolicy::default())?;
    assert_eq!(admitted.value(), &value);
    value["data"]["providers"][0]["models"][0]["interfaces"][0]["limits"] =
        json!({"max_output_tokens":"large"});
    assert!(Document::from_value(value, &ReaderPolicy::default()).is_err());
    Ok(())
}

#[test]
fn identities_profiles_effort_and_prices_are_checked_without_panics() {
    let cases = [
        (
            "duplicate-native",
            json!([
                provider()["models"][0].clone(),
                provider()["models"][0].clone()
            ]),
        ),
        (
            "duplicate-default-profile",
            json!([{"provider_model_id":"a","status":"active","interfaces":[{"protocol":"decisions","operation":"decisions"},{"protocol":"decisions","operation":"decisions","protocol_profile":"default"}]}]),
        ),
        (
            "missing-systemone-profile",
            json!([{"provider_model_id":"a","status":"active","interfaces":[{"protocol":"systemone","operation":"decisions"}]}]),
        ),
        (
            "unverified-effort",
            json!([{"provider_model_id":"a","status":"active","interfaces":[{"protocol":"responses","operation":"generation","reasoning_effort":{"levels":["low"]}}]}]),
        ),
        (
            "unordered-context-tier",
            json!([{"provider_model_id":"a","status":"active","interfaces":[{"protocol":"responses","operation":"generation","pricing":{"kind":"token","currency":"USD","context_tiers":[{"above_input_tokens":10},{"above_input_tokens":9}]}}]}]),
        ),
    ];
    for (name, models) in cases {
        let mut value = minimal();
        let mut p = provider();
        p["models"] = models;
        value["data"]["providers"] = json!([p]);
        assert!(
            Document::from_value(value, &ReaderPolicy::default()).is_err(),
            "{name}"
        );
    }
}

#[test]
fn duplicate_keys_depth_trailing_data_and_excessive_decimals_fail_normally()
-> Result<(), RegistryError> {
    assert!(parse_json(br#"{"a":{"b":1,"b":2}}"#).is_err());
    assert!(parse_json(b"{} {}").is_err());
    let mut deep = "[".repeat(66);
    deep.push('0');
    deep.push_str(&"]".repeat(66));
    assert!(parse_json(deep.as_bytes()).is_err());
    assert_eq!(
        parse_json(b"1.00000000000000000000000000001")?.to_string(),
        "1.00000000000000000000000000001"
    );
    assert!(parse_json(b"1e999999999").is_err());
    Ok(())
}

#[test]
fn known_compatibility_types_are_validated_but_future_profiles_remain_open()
-> Result<(), RegistryError> {
    let mut value = minimal();
    let mut p = provider();
    p["models"] = json!([{"provider_model_id":"native","status":"active","interfaces":[{"protocol":"chat_completions","operation":"generation","compatibility":{"supports_store":"invalid"}}]}]);
    value["data"]["providers"] = json!([p]);
    assert!(Document::from_value(value.clone(), &ReaderPolicy::default()).is_err());
    value["data"]["providers"][0]["models"][0]["interfaces"][0]["protocol_profile"] =
        json!("future");
    Document::from_value(value, &ReaderPolicy::default())?;
    Ok(())
}
