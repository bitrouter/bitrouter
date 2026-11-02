use bitrouter_ai::tenancy::{
    resources::{ResourcePut, ResourceRevoke},
    validation::Validate,
};
use serde_json::json;

fn put() -> serde_json::Value {
    json!({
        "format": 1, "operation_id": "create", "installation_id": "installation",
        "key": {"owner_kind":"tenant", "owner_id":"one", "resource_kind":"credential", "resource_id":"provider-key"},
        "expected_revision": null, "definition": {"kind":"credential", "secret":"private-value"}
    })
}

#[test]
fn resource_commands_require_explicit_cas_and_redact_diagnostics()
-> Result<(), Box<dyn std::error::Error>> {
    let input: ResourcePut = serde_json::from_value(put())?;
    input.validate()?;
    assert!(!format!("{input:?}").contains("private-value"));
    assert!(!format!("{:?}", input.definition).contains("private-value"));
    let mut missing = put();
    if let Some(object) = missing.as_object_mut() {
        object.remove("expected_revision");
    }
    assert!(serde_json::from_value::<ResourcePut>(missing).is_err());
    for invalid in [json!(""), json!("x".repeat(65_537))] {
        let mut value = put();
        value["definition"]["secret"] = invalid;
        assert!(
            serde_json::from_value::<ResourcePut>(value)?
                .validate()
                .is_err()
        );
    }
    let mut value = put();
    value["key"]["resource_kind"] = json!("route");
    assert!(
        serde_json::from_value::<ResourcePut>(value)?
            .validate()
            .is_err()
    );
    Ok(())
}

#[test]
fn malformed_revoke_and_credential_bindings_fail_closed() -> Result<(), Box<dyn std::error::Error>>
{
    assert!(serde_json::from_value::<ResourceRevoke>(json!({
        "format":1,"operation_id":"revoke","installation_id":"installation",
        "reference":{"owner_kind":"tenant","owner_id":"one","resource_kind":"credential","resource_id":"key","revision_id":"r1"}
    })).is_err());
    let mut value = put();
    value["key"]["resource_kind"] = json!("route");
    value["definition"] = json!({"kind":"route", "routing":{}, "selector":"provider/model", "policy":null,
        "credentials":[{"provider":"provider", "account":"default", "credential":{"owner_kind":"tenant","owner_id":"one","resource_kind":"route","resource_id":"key","revision_id":"r1"}}]});
    assert!(
        serde_json::from_value::<ResourcePut>(value)?
            .validate()
            .is_err()
    );
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn schema_requires_nullable_cas_without_removing_null() -> Result<(), Box<dyn std::error::Error>> {
    let schema = serde_json::to_value(schemars::schema_for!(ResourcePut))?;
    let required = schema["required"]
        .as_array()
        .ok_or("required properties missing")?;
    assert!(
        required
            .iter()
            .any(|property| property == "expected_revision")
    );
    let nullable = &schema["$defs"]["RequiredNullable_RevisionId"];
    assert!(serde_json::to_string(nullable)?.contains("null"));
    assert_eq!(schema["additionalProperties"], false);
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn published_resource_schemas_match_the_public_contract() -> Result<(), Box<dyn std::error::Error>>
{
    use bitrouter_ai::tenancy::resources::{ResourceCapabilities, ResourceStatus};
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/specs/resource-provisioning-v1");
    for (name, schema) in [
        ("put.schema.json", schemars::schema_for!(ResourcePut)),
        ("revoke.schema.json", schemars::schema_for!(ResourceRevoke)),
        ("status.schema.json", schemars::schema_for!(ResourceStatus)),
        (
            "capabilities.schema.json",
            schemars::schema_for!(ResourceCapabilities),
        ),
    ] {
        let rendered = format!("{}\n", serde_json::to_string_pretty(&schema)?);
        let path = directory.join(name);
        if std::env::var("BITROUTER_BLESS").as_deref() == Ok("1") {
            std::fs::write(&path, &rendered)?;
        }
        assert_eq!(
            std::fs::read_to_string(path)?,
            rendered,
            "resource schema {name} drifted"
        );
    }
    Ok(())
}
