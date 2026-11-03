use bitrouter_ai::tenancy::{
    native_controls::{CONTROLS, OrderedNativeControlsCapabilitiesV1},
    validation::Validate,
};
use serde_json::{Value, json};
type TestResult = Result<(), Box<dyn std::error::Error>>;

fn capabilities() -> Value {
    json!({"contract_version":1,"installation_id":"installation","instance_id":"instance",
        "capability_revision":"revision","authority_revision":7,"enabled":false,
        "controls":["responses.previous_response_id","responses.conversation","messages.context_management"]})
}

#[test]
fn native_discovery_is_closed_and_disabled_is_not_malformed() -> TestResult {
    let value: OrderedNativeControlsCapabilitiesV1 = serde_json::from_value(capabilities())?;
    value.validate()?;
    assert!(!value.enabled);
    assert_eq!(value.controls, CONTROLS);
    for field in capabilities().as_object().ok_or("object")?.keys() {
        let mut missing = capabilities();
        missing.as_object_mut().ok_or("object")?.remove(field);
        assert!(serde_json::from_value::<OrderedNativeControlsCapabilitiesV1>(missing).is_err());
    }
    let mut extra = capabilities();
    extra["owner"] = json!("fabricated");
    assert!(serde_json::from_value::<OrderedNativeControlsCapabilitiesV1>(extra).is_err());
    for controls in [
        json!([]),
        json!(["responses.previous_response_id"]),
        json!([
            "responses.previous_response_id",
            "responses.conversation",
            "generate_content.cachedContent"
        ]),
    ] {
        let mut invalid = capabilities();
        invalid["controls"] = controls;
        assert!(serde_json::from_value::<OrderedNativeControlsCapabilitiesV1>(invalid).is_err());
    }
    let mut repeated = capabilities();
    repeated["controls"][2] = json!("responses.conversation");
    assert!(
        serde_json::from_value::<OrderedNativeControlsCapabilitiesV1>(repeated)?
            .validate()
            .is_err()
    );
    Ok(())
}

#[test]
fn native_discovery_uses_exact_integer_version_semantics() -> TestResult {
    let raw = serde_json::to_string(&capabilities())?;
    for spelling in ["1.0", "1e0"] {
        let value: OrderedNativeControlsCapabilitiesV1 = serde_json::from_str(&raw.replace(
            "\"contract_version\":1",
            &format!("\"contract_version\":{spelling}"),
        ))?;
        value.validate()?;
    }
    for version in [json!(0), json!(2)] {
        let mut value = capabilities();
        value["contract_version"] = version;
        assert!(
            serde_json::from_value::<OrderedNativeControlsCapabilitiesV1>(value)?
                .validate()
                .is_err()
        );
    }
    for spelling in ["1.5", "-1", "4294967296"] {
        assert!(
            serde_json::from_str::<OrderedNativeControlsCapabilitiesV1>(&raw.replace(
                "\"contract_version\":1",
                &format!("\"contract_version\":{spelling}")
            ),)
            .is_err()
        );
    }
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn native_discovery_schema_keeps_the_exact_protocol_field_set() -> TestResult {
    let schema = schemars::schema_for!(OrderedNativeControlsCapabilitiesV1);
    let json = serde_json::to_value(&schema)?;
    assert_eq!(json["additionalProperties"], false);
    assert_eq!(
        json["properties"]["controls"]["const"],
        capabilities()["controls"]
    );
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/specs/service-api/ordered-native-controls-capabilities-v1.schema.json");
    let rendered = format!("{}\n", serde_json::to_string_pretty(&schema)?);
    // Bootstrap only this new artifact; existing schemas are never rewritten.
    if std::env::var("BITROUTER_WRITE_NATIVE_CONTROLS_SCHEMA").as_deref() == Ok("1")
        && !path.exists()
    {
        std::fs::write(&path, &rendered)?;
    }
    assert_eq!(std::fs::read_to_string(path)?, rendered);
    Ok(())
}
