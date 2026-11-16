use bitrouter_ai::policy::{Document, SPEC_VERSION};
use serde_json::json;

#[cfg(feature = "schema")]
#[test]
fn published_policy_schema_matches_the_public_contract() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/specs/policy/policy.schema.json");
    let published: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    assert_eq!(
        published,
        serde_json::to_value(schemars::schema_for!(Document))?
    );
    Ok(())
}

#[test]
fn policy_contract_identity_is_independent_of_package_versions()
-> Result<(), Box<dyn std::error::Error>> {
    let source = json!({"spec_version":SPEC_VERSION,"policies":{"auto":{
        "tiers":{"default":{"model":"fixture:small"}},"default_tier":"default"
    }}});
    let typed: Document = serde_json::from_value(source.clone())?;
    assert_eq!(serde_json::to_value(typed)?, source);
    for version in [
        "registry/1.0.0-beta.1",
        "1.0.0-beta.1",
        "policy/1.0.0-beta.2",
        "policy/1.0.0",
    ] {
        let mut value = source.clone();
        value["spec_version"] = json!(version);
        assert!(serde_json::from_value::<Document>(value).is_err());
    }
    let mut legacy = source.clone();
    legacy
        .as_object_mut()
        .ok_or("object")?
        .remove("spec_version");
    legacy["lockfileVersion"] = json!(4);
    assert!(serde_json::from_value::<Document>(legacy).is_err());
    let mut context = source;
    context["policies"]["auto"]["tiers"]["default"]["context"] = json!("evidence");
    assert!(serde_json::from_value::<Document>(context).is_err());
    let duplicate = r#"{"spec_version":"policy/1.0.0-beta.1","policies":{"auto":{"tiers":{"same":{"model":"a"},"same":{"model":"b"}},"default_tier":"same"}}}"#;
    assert!(serde_json::from_str::<Document>(duplicate).is_err());
    let wrapped =
        format!(r#"{{"kind":"policy","document":{duplicate},"name":"auto","dependencies":[]}}"#);
    assert!(
        serde_json::from_str::<bitrouter_ai::tenancy::resources::Definition>(&wrapped).is_err()
    );
    Ok(())
}

#[test]
fn resource_and_publication_wrappers_use_the_same_policy_reader()
-> Result<(), Box<dyn std::error::Error>> {
    let source = json!({"spec_version":SPEC_VERSION,"policies":{"auto":{
        "tiers":{"default":{"model":"fixture:small"}},"default_tier":"default"
    }}});
    for invalid in [false, true] {
        let mut source = source.clone();
        if invalid {
            source["spec_version"] = json!("policy/1.0.0-beta.2");
        }
        let resource = json!({"kind":"policy","document":source,"name":"auto","dependencies":[]});
        let publication =
            json!({"format":1,"routing":{},"policies":[],"policy_lock":source,"credentials":[]});
        assert_eq!(
            serde_json::from_value::<bitrouter_ai::tenancy::resources::Definition>(resource)
                .is_ok(),
            !invalid
        );
        assert_eq!(
            serde_json::from_value::<bitrouter_ai::gateway::operator::Publication>(publication)
                .is_ok(),
            !invalid
        );
    }
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn wrapper_schemas_describe_the_required_policy_contract() -> Result<(), Box<dyn std::error::Error>>
{
    for schema in [
        serde_json::to_value(schemars::schema_for!(
            bitrouter_ai::tenancy::resources::ResourcePut
        ))?,
        serde_json::to_value(schemars::schema_for!(
            bitrouter_ai::gateway::operator::Publication
        ))?,
    ] {
        assert_eq!(
            schema["$defs"]["SpecVersion"]["oneOf"][0]["const"],
            SPEC_VERSION
        );
        assert_eq!(schema["$defs"]["Document"]["additionalProperties"], false);
        assert!(
            schema["$defs"]["Document"]["required"]
                .as_array()
                .ok_or("required fields")?
                .contains(&json!("spec_version"))
        );
    }
    Ok(())
}
