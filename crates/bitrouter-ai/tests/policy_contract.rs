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
            json!({"format":2,"routing":{},"policies":[],"model_policy":source,"credentials":[]});
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

#[test]
fn publication_format_and_source_are_closed_before_service_mutation()
-> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_ai::gateway::operator::Publication;
    let raw = r#"{"format":2,"routing":{},"policies":[{"id":"acl-independent"}],"model_policy":{"spec_version":"policy/1.0.0-beta.1","policies":{"auto":{"tiers":{"strong":{"model":"fixture:strong"}},"default_tier":"strong"}}},"credentials":[]}"#;
    let publication: Publication = serde_json::from_str(raw)?;
    let source: &Document = publication.model_policy.as_ref().ok_or("missing source")?;
    assert_eq!(
        source.policies["auto"].tiers["strong"].model,
        "fixture:strong"
    );
    assert_eq!(publication.policies, [json!({"id":"acl-independent"})]);
    for changed in [
        raw.replace("\"format\":2,", ""),
        raw.replace("\"format\":2", "\"format\":1"),
        raw.replace("\"format\":2", "\"format\":3"),
        raw.replace("\"format\":2", "\"format\":2,\"format\":1"),
        raw.replace("\"model_policy\":", "\"policy_lock\":"),
        raw.replace(
            "\"model_policy\":",
            "\"policy_lock\":null,\"model_policy\":",
        ),
        raw.replace(
            "\"model\":\"fixture:strong\"",
            "\"model\":\"fixture:strong\",\"context\":\"evidence\"",
        ),
        raw.replace(
            "\"strong\":{\"model\":\"fixture:strong\"}",
            "\"strong\":{\"model\":\"fixture:strong\"},\"strong\":{\"model\":\"fixture:other\"}",
        ),
    ] {
        assert!(
            serde_json::from_str::<Publication>(&changed).is_err(),
            "{changed}"
        );
    }
    let policy_free: Publication =
        serde_json::from_str(r#"{"format":2,"routing":{},"credentials":[]}"#)?;
    assert!(policy_free.model_policy.is_none());
    Ok(())
}

#[test]
fn typed_resource_writes_preserve_literal_names_and_reject_duplicate_source_keys()
-> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_ai::tenancy::{resources::Definition, validation::Validate};
    let raw = r#"{"kind":"policy","document":{"spec_version":"policy/1.0.0-beta.1","policies":{"auto":{"tiers":{"strong":{"model":"fixture:strong"}},"default_tier":"strong"},"auto:cost":{"tiers":{"cheap":{"model":"fixture:cheap"}},"default_tier":"cheap"}}},"name":"auto:cost","dependencies":[]}"#;
    let definition: Definition = serde_json::from_str(raw)?;
    definition.validate()?;
    let Definition::Policy { document, name, .. } = &definition else {
        return Err("wrong resource kind".into());
    };
    let source: &Document = document;
    assert_eq!(source.policies[name].tiers["cheap"].model, "fixture:cheap");
    let missing = raw.replace("\"name\":\"auto:cost\"", "\"name\":\"missing\"");
    assert!(
        serde_json::from_str::<Definition>(&missing)?
            .validate()
            .is_err()
    );
    for changed in [
        raw.replace(
            "\"model\":\"fixture:strong\"",
            "\"model\":\"fixture:strong\",\"context\":\"evidence\"",
        ),
        raw.replace(
            "\"model\":\"fixture:strong\"",
            "\"model\":\"fixture:strong\",\"model\":\"fixture:other\"",
        ),
        raw.replace(
            "\"spec_version\":\"policy/1.0.0-beta.1\"",
            "\"lockfileVersion\":4",
        ),
    ] {
        assert!(serde_json::from_str::<Definition>(&changed).is_err());
    }
    // Credential provisioning uses its own current contract.
    serde_json::from_str::<Definition>(r#"{"kind":"credential","secret":"fixture-only"}"#)?
        .validate()?;
    Ok(())
}

#[test]
fn old_publication_reader_and_current_reader_select_distinct_formats()
-> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_ai::gateway::operator::Publication;
    // Frozen old reader's field catalog, including the old format-one service
    // guard. This peer never learns the new field or guesses a source version.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FormatOnePublication {
        format: u32,
        routing: serde_json::Value,
        policy_lock: Option<serde_json::Value>,
        credentials: Vec<serde_json::Value>,
    }
    let old = r#"{"format":1,"routing":{},"policy_lock":{"lockfileVersion":4},"credentials":[]}"#;
    let current = r#"{"format":2,"routing":{},"model_policy":{"spec_version":"policy/1.0.0-beta.1","policies":{"auto":{"tiers":{"strong":{"model":"fixture/strong"}},"default_tier":"strong"}}},"credentials":[]}"#;
    let old_reader: FormatOnePublication = serde_json::from_str(old)?;
    assert_eq!(old_reader.format, 1);
    assert!(old_reader.routing.is_object());
    assert_eq!(
        old_reader.policy_lock.ok_or("old source")?["lockfileVersion"],
        4
    );
    assert!(old_reader.credentials.is_empty());
    assert!(serde_json::from_str::<FormatOnePublication>(current).is_err());
    assert!(serde_json::from_str::<Publication>(old).is_err());
    assert_eq!(serde_json::from_str::<Publication>(current)?.format, 2);
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

#[test]
fn route_keys_use_exact_labels_and_reject_duplicate_entries()
-> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_ai::policy::{NextStep, Risk, RouteClass, TaskFamily};

    let key = "code:review|verify|normal";
    let class: RouteClass = key.parse()?;
    assert_eq!(
        class,
        RouteClass::new(TaskFamily::CodeReview, NextStep::Verify, Risk::Normal)
    );
    assert_eq!(
        serde_json::to_value(class.task_family)?,
        json!("code:review")
    );
    assert_eq!(serde_json::to_value(class.next_step)?, json!("verify"));
    assert_eq!(serde_json::to_value(class.risk)?, json!("normal"));
    assert_eq!(serde_json::to_value(class)?, json!(key));
    assert_eq!(
        serde_json::to_value(class.unknown_baseline())?,
        json!("unknown|verify|normal")
    );

    let source = |routes: &str| {
        format!(
            r#"{{"spec_version":"{SPEC_VERSION}","policies":{{"auto":{{"tiers":{{"default":{{"model":"fixture:small"}}}},"default_tier":"default","routes":{{{routes}}}}}}}}}"#
        )
    };
    let input = format!("\"{key}\":\"default\"");
    let document: Document = serde_json::from_str(&source(&input))?;
    assert_eq!(document.policies["auto"].routes[&class], "default");
    assert!(serde_json::from_str::<Document>(&source(&format!("{input},{input}"))).is_err());

    for invalid in [
        "code_review|verify|normal",
        "code:review|review|normal",
        "code:review|verify|context",
        "code:review|verify|normal|extra",
        "code:review|verify",
    ] {
        let input = format!("\"{invalid}\":\"default\"");
        assert!(
            serde_json::from_str::<Document>(&source(&input)).is_err(),
            "{invalid}"
        );
    }
    Ok(())
}
