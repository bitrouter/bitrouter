//! Non-authoritative fallback admission without inferred execution semantics.
use bitrouter_registry::{models_dev, reconcile};
use registry_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn feed() -> Value {
    json!({"id":"fallback","kind":"models_dev","provider_id":"fixture","writes":["models","metadata"],"request":{"url":models_dev::URL,"auth":{"kind":"none"}},"pagination":{"kind":"none"},"inventory":{"mode":"merge","scope":"public","complete":false,"allow_empty":false}})
}

/// SDV for provider selection, native IDs and unsupported model types.
#[test]
fn sdv_fallback_keeps_native_offers_without_granting_interfaces_or_bindings() -> TestResult {
    let snapshot = json!({"fixture":{"id":"fixture","models":{
        "Native/Decision":{"id":"Native/Decision","name":"Decision","type":"decision","canonical_model_id":"other/canonical","cost":{"input":0},"api":{"npm":"future"}},
        "Future":{"id":"Future","type":"future-type","new_field":true},
        "Old":{"id":"Old","status":"deprecated"}
    }},"foreign":{"models":{"DoNotImport":{"id":"DoNotImport"}}}});
    let observed = models_dev::observation(&feed(), &snapshot)?;
    assert_eq!(observed.models.len(), 2);
    assert_eq!(observed.excluded, 1);
    for model in observed.models {
        assert_eq!(model.value["status"], "staging");
        assert_eq!(model.value["interfaces"], json!([]));
        assert!(model.value.get("canonical_id").is_none());
        assert!(model.value.get("pricing").is_none());
    }
    Ok(())
}

/// SDV for fallback authority and credential isolation.
#[test]
fn sdv_fallback_rejects_credentials_and_provider_lifecycle_authority() {
    for pointer in [
        "/request/auth/kind",
        "/inventory/mode",
        "/inventory/complete",
        "/writes",
    ] {
        let mut config = feed();
        let value = match pointer {
            "/request/auth/kind" => json!("bearer"),
            "/inventory/mode" => json!("snapshot"),
            "/inventory/complete" => json!(true),
            _ => json!(["models", "metadata", "pricing"]),
        };
        if let Some(target) = config.pointer_mut(pointer) {
            *target = value;
        }
        assert!(models_dev::validate(&config).is_err());
    }
}

/// SDV for malformed/changed source records retaining the previous observation.
#[test]
fn sdv_fallback_rejects_missing_provider_identity_conflicts_and_unknown_status() {
    for snapshot in [
        json!({}),
        json!({"fixture":{"models":{}}}),
        json!({"fixture":{"id":7,"models":{}}}),
        json!({"fixture":{"id":"foreign","models":{}}}),
        json!({"fixture":{"id":"fixture","models":{"A":{"id":"B"}}}}),
        json!({"fixture":{"id":"fixture","models":{"A":{"id":"A","status":"future"}}}}),
    ] {
        assert!(models_dev::observation(&feed(), &snapshot).is_err());
    }
}

/// SDV for preservation of manual interfaces, prices and foreign metadata.
#[test]
fn sdv_fallback_cannot_overwrite_manual_execution_data() -> TestResult {
    let provider = json!({"name":"fixture","status":"active","models":[{"provider_model_id":"Native","status":"active","interfaces":[{"protocol":"future-protocol","operation":"future-operation","pricing":{"kind":"future-tariff"},"unknown":true}]}],"auto_sync":{"feeds":[feed()]}});
    let observation = models_dev::observation(
        &feed(),
        &json!({"fixture":{"id":"fixture","models":{"Native":{"id":"Native","name":"New"}}}}),
    )?;
    let stamp = reconcile::next_stamp(&feed(), "run".into(), "2026-11-01T00:00:00Z".parse()?)?;
    let (next, _) = reconcile::apply(&provider, "fallback", &observation, &stamp)?;
    assert_eq!(
        next["models"][0]["interfaces"],
        provider["models"][0]["interfaces"]
    );
    Ok(())
}

/// SDV for the published worked example's source and observation contract.
#[test]
fn sdv_worked_example_is_admitted_without_activation() -> TestResult {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/specs/registry-v1/examples");
    let config: Value = registry_json::from_str(&std::fs::read_to_string(
        directory.join("models-dev-feed.json"),
    )?)?;
    let snapshot: Value = registry_json::from_str(&std::fs::read_to_string(
        directory.join("models-dev-response.json"),
    )?)?;
    let observed = models_dev::observation(&config, &snapshot)?;
    assert_eq!(observed.models.len(), 2);
    assert_eq!(observed.excluded, 1);
    assert!(
        observed
            .models
            .iter()
            .all(|model| model.value["status"] == "staging"
                && model.value["interfaces"] == json!([]))
    );
    Ok(())
}
