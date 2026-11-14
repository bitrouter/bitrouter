use bitrouter_ai::tenancy::{
    resources::ResourcePut,
    resources_v2::{ResourcePutRequest, ResourcePutV2},
    resources_v3::{DefinitionV3, ResourceCapabilitiesV3, ResourcePutV3},
    validation::Validate,
};
use serde_json::{Value, json};
type TestResult = Result<(), Box<dyn std::error::Error>>;
fn reference(kind: &str) -> Value {
    json!({"owner_kind":if kind=="policy"{"operator_pool"}else{"tenant"},"owner_id":if kind=="policy"{"pool"}else{"tenant"},"resource_kind":kind,"resource_id":kind,"revision_id":"revision"})
}
fn input() -> Value {
    json!({"format":3,"operation_id":"native-create","installation_id":"installation",
    "key":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"route","resource_id":"native-route"},"expected_revision":null,
    "definition":{"kind":"native_profiled_route_v1","routing":{"providers":{}},"selector":"provider/model","policy":null,
        "credentials":[{"provider":"provider","account":"default","credential":reference("credential")}],
        "attachments":[{"inbound_protocol":"generate_content","candidate_index":0,"credential":reference("credential"),"profile":reference("policy")}]}})
}
#[test]
fn native_resource_contract_is_explicit_closed_and_not_a_legacy_command() -> TestResult {
    let raw = serde_json::to_string(&input())?;
    let command: ResourcePutV3 = serde_json::from_str(&raw)?;
    command.validate()?;
    assert!(matches!(
        serde_json::from_str::<ResourcePutRequest>(&raw)?,
        ResourcePutRequest::V3(_)
    ));
    assert!(serde_json::from_str::<ResourcePut>(&raw).is_err());
    assert!(serde_json::from_str::<ResourcePutV2>(&raw).is_err());
    for field in [
        "format",
        "operation_id",
        "installation_id",
        "key",
        "expected_revision",
        "definition",
    ] {
        let mut missing = input();
        missing.as_object_mut().ok_or("object")?.remove(field);
        assert!(
            serde_json::from_value::<ResourcePutV3>(missing).is_err(),
            "{field}"
        );
    }
    let mut unknown = input();
    unknown["definition"]["unexpected"] = true.into();
    assert!(serde_json::from_value::<ResourcePutV3>(unknown).is_err());
    let raw = raw.replace(
        "\"candidate_index\":0",
        "\"candidate_index\":0,\"candidate_\\u0069ndex\":1",
    );
    assert!(serde_json::from_str::<ResourcePutV3>(&raw).is_err());
    Ok(())
}
#[test]
fn native_resource_contract_requires_native_and_exact_ref_kinds_and_static_policy() -> TestResult {
    for (pointer, value) in [
        ("/format", json!(1)),
        ("/key/resource_kind", json!("policy")),
        ("/definition/policy", reference("policy")),
        (
            "/definition/attachments/0/inbound_protocol",
            json!("chat_completions"),
        ),
        ("/definition/attachments/0/candidate_index", json!(256)),
        (
            "/definition/attachments/0/credential/resource_kind",
            json!("route"),
        ),
        (
            "/definition/attachments/0/profile/owner_kind",
            json!("tenant"),
        ),
    ] {
        let mut value_input = input();
        *value_input.pointer_mut(pointer).ok_or("pointer")? = value;
        assert!(
            serde_json::from_value::<ResourcePutV3>(value_input)?
                .validate()
                .is_err(),
            "{pointer}"
        );
    }
    let mut duplicate = input();
    duplicate["definition"]["attachments"]
        .as_array_mut()
        .ok_or("attachments")?
        .push(input()["definition"]["attachments"][0].clone());
    assert!(
        serde_json::from_value::<ResourcePutV3>(duplicate)?
            .validate()
            .is_err()
    );
    let raw = serde_json::to_string(&input())?
        .replace("\"format\":3", "\"format\":3.0")
        .replace("\"candidate_index\":0", "\"candidate_index\":0e0");
    serde_json::from_str::<ResourcePutV3>(&raw)?.validate()?;
    Ok(())
}
#[test]
fn native_resource_capability_is_separate_and_bound_to_four_explicit_protocols() -> TestResult {
    let mut value = json!({"format":3,"installation_id":"installation","provisioning_enabled":true,"max_command_bytes":1048576,
        "tenant_routes_require_approved_origins":true,"generation_protocols":["chat_completions","messages","responses","generate_content"]});
    serde_json::from_value::<ResourceCapabilitiesV3>(value.clone())?.validate()?;
    value["generation_protocols"][3] = "chat_completions".into();
    assert!(
        serde_json::from_value::<ResourceCapabilitiesV3>(value)?
            .validate()
            .is_err()
    );
    Ok(())
}
#[test]
fn native_resource_routing_preserves_opaque_tokens_and_bounds_programmatic_depth() -> TestResult {
    let mut value = input();
    value["definition"]["routing"] = json!({"opaque":{"$serde_json::private::Number":"literal","sibling":[1.25,null]},"nested":[{"$serde_json::private::RawValue":"literal-raw"}]});
    let raw = serde_json::to_string(&value)?;
    let mut command: ResourcePutV3 = serde_json::from_str(&raw)?;
    assert_eq!(
        serde_json::to_value(&command)?["definition"]["routing"],
        value["definition"]["routing"]
    );
    let DefinitionV3::NativeProfiledRouteV1 { routing, .. } = &mut command.definition;
    let mut deep = json!(0);
    for _ in 0..128 {
        deep = json!([deep]);
    }
    *routing = json!({"deep":deep});
    assert!(command.validate().is_err());
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn native_resource_schemas_are_separate_closed_and_bounded() -> TestResult {
    let put = serde_json::to_value(schemars::schema_for!(ResourcePutV3))?;
    let capability = serde_json::to_value(schemars::schema_for!(ResourceCapabilitiesV3))?;
    assert_eq!(put["additionalProperties"], false);
    assert_eq!(put["properties"]["format"]["minimum"], 3);
    assert_eq!(
        capability["properties"]["tenant_routes_require_approved_origins"]["const"],
        true
    );
    assert_eq!(
        capability["properties"]["generation_protocols"]["const"],
        json!([
            "chat_completions",
            "messages",
            "responses",
            "generate_content"
        ])
    );
    if let Ok(directory) = std::env::var("BITROUTER_NATIVE_RESOURCE_SCHEMA_EXPORT") {
        let directory = std::path::Path::new(&directory);
        std::fs::create_dir_all(directory)?;
        for (name, schema) in [
            ("native-resource-put-v3.schema.json", put),
            ("native-resource-capabilities-v3.schema.json", capability),
        ] {
            std::fs::write(
                directory.join(name),
                format!("{}\n", serde_json::to_string_pretty(&schema)?),
            )?;
        }
    }
    Ok(())
}

#[test]
fn native_selector_limit_counts_utf8_bytes_without_changing_legacy_text() -> TestResult {
    for (count, accepted) in [(256, true), (257, false)] {
        let mut value = input();
        value["definition"]["selector"] = json!("é".repeat(count));
        assert_eq!(
            serde_json::from_value::<ResourcePutV3>(value)?
                .validate()
                .is_ok(),
            accepted
        );
    }
    Ok(())
}
