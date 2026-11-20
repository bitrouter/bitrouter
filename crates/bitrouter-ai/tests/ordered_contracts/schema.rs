use super::*;
use bitrouter_ai::tenancy::ordered::CandidateDescriptor;
use schemars::{JsonSchema, SchemaGenerator};

#[path = "schema/fixtures.rs"]
mod fixtures;

const PACKAGED: &str = include_str!("../../src/tenancy/ordered/wire-schema.json");

fn exported() -> Result<Vec<(String, Value)>, serde_json::Error> {
    macro_rules! schemas {
        ($($ty:ty),+ $(,)?) => {
            vec![$((stringify!($ty).to_owned(), serde_json::to_value(schemars::schema_for!($ty))?)),+]
        };
    }
    Ok(schemas![
        GenerationProtocol,
        OrderedCandidateSelection,
        AcceptedSelection,
        CandidateDescriptor,
        CandidateManifest,
        OrderedCapabilities,
        OrderedDispatchBinding,
        Attempt,
        ExecutionReceipt,
        ExecutionUsageEvidence,
    ])
}

#[test]
fn all_ordered_wire_types_export_closed_self_contained_schemas() -> TestResult {
    let canonical = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/specs/service-api/ordered-dispatch.schema.json");
    assert_eq!(std::fs::read_to_string(canonical)?, PACKAGED);
    let bundle: Value = serde_json::from_str(PACKAGED)?;
    assert_eq!(
        bundle["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    for (name, schema) in exported()? {
        assert_eq!(schema["title"], name);
        assert_eq!(
            bundle["$defs"].get(&name).is_some(),
            !matches!(
                name.as_str(),
                "Attempt" | "ExecutionReceipt" | "ExecutionUsageEvidence"
            ),
            "{name}"
        );
        assert!(
            !schema.to_string().contains("\"$ref\""),
            "unresolved reference in {name}"
        );
        if name == "GenerationProtocol" {
            continue;
        }
        assert_eq!(schema["type"], "object", "{name}");
        assert_eq!(schema["additionalProperties"], false, "{name}");
        let properties = schema["properties"].as_object().ok_or("properties")?;
        let required = schema["required"].as_array().ok_or("required")?;
        let optional = usize::from(matches!(
            name.as_str(),
            "OrderedCandidateSelection" | "AcceptedSelection"
        ));
        assert_eq!(properties.len(), required.len() + optional, "{name}");
        for property in properties.keys() {
            assert_eq!(
                required.contains(&json!(property)),
                property != "generation_defaults_digest",
                "{name}.{property}"
            );
        }
    }
    // Embedding these contracts must work without leaking bundle-relative refs.
    let mut generator = SchemaGenerator::default();
    generator.subschema_for::<ExecutionReceipt>();
    generator.subschema_for::<ExecutionUsageEvidence>();
    let root = generator.into_root_schema_for::<OrderedDispatchBinding>();
    assert!(serde_json::to_value(root)?["$defs"].is_object());
    Ok(())
}

#[test]
fn schema_bounds_versions_and_required_nulls_are_explicit() -> TestResult {
    let schemas: std::collections::HashMap<_, _> = exported()?.into_iter().collect();
    for name in ["OrderedCandidateSelection", "AcceptedSelection"] {
        let indices = &schemas[name]["properties"]["candidate_indices"];
        assert_eq!(indices["minItems"], 1);
        assert_eq!(indices["maxItems"], 64);
        assert_eq!(indices["uniqueItems"], true);
        assert_eq!(indices["items"]["minimum"], 0);
        assert_eq!(indices["items"]["maximum"], 255);
        assert_eq!(
            schemas[name]["properties"]["route"]["properties"]["resource_kind"]["const"],
            "route"
        );
    }
    for (name, field, version) in [
        ("OrderedCandidateSelection", "schema_version", 1),
        ("CandidateManifest", "schema_version", 1),
        ("OrderedCapabilities", "contract_version", 2),
    ] {
        assert_eq!(schemas[name]["properties"][field]["const"], version);
    }
    assert_eq!(
        schemas["GenerationProtocol"]["enum"],
        json!([
            "chat_completions",
            "messages",
            "responses",
            "generate_content"
        ])
    );
    for name in ["OrderedCapabilities"] {
        assert_eq!(schemas[name]["properties"]["attempt_limit"]["minimum"], 1);
        assert_eq!(schemas[name]["properties"]["attempt_limit"]["maximum"], 64);
    }
    assert_eq!(
        schemas["ExecutionUsageEvidence"]["properties"]["extractor_version"]["enum"],
        json!([1, 2])
    );
    let descriptor = &schemas["CandidateDescriptor"]["properties"];
    assert_eq!(descriptor["candidate_index"]["maximum"], 255);
    assert_eq!(
        descriptor["credential"]["properties"]["resource_kind"]["const"],
        "credential"
    );
    for (name, field, max) in [
        ("CandidateManifest", "candidates", 256),
        ("ExecutionReceipt", "attempts", 64),
        ("ExecutionUsageEvidence", "attempts", 64),
        ("ExecutionUsageEvidence", "frames", 4096),
        ("Attempt", "native_usage", 32),
    ] {
        assert_eq!(schemas[name]["properties"][field]["maxItems"], max);
    }
    for (name, field) in [
        ("Attempt", "target"),
        ("Attempt", "observed_http_status"),
        ("ExecutionUsageEvidence", "estimate"),
    ] {
        assert!(
            schemas[name]["required"]
                .as_array()
                .ok_or("required")?
                .contains(&json!(field))
        );
        assert!(
            schemas[name]["properties"][field]["anyOf"]
                .as_array()
                .ok_or("nullable")?
                .iter()
                .any(|s| s["type"] == "null")
        );
    }
    let status = &schemas["Attempt"]["properties"]["observed_http_status"]["anyOf"][0];
    assert_eq!(status["minimum"], 200);
    assert_eq!(status["maximum"], 599);
    let frame = &schemas["ExecutionUsageEvidence"]["properties"]["frames"]["items"];
    assert_eq!(
        frame["properties"]["protocol"]["enum"]
            .as_array()
            .ok_or("protocols")?
            .len(),
        5
    );
    assert_eq!(frame["properties"]["quantities"]["maxItems"], 32);
    assert_eq!(
        frame["properties"]["quantities"]["items"]["properties"]["value"]["anyOf"][0]["maximum"],
        9007199254740991_u64
    );
    assert!(
        schemas["ExecutionUsageEvidence"]["properties"]
            .get("child_execution_ids")
            .is_some()
    );
    assert!(
        schemas["ExecutionUsageEvidence"]["properties"]
            .get("parent_execution_id")
            .is_some()
    );
    Ok(())
}

#[test]
fn shared_schemas_require_dispatch_and_preserve_strict_bounds() -> TestResult {
    let mut selection = serde_json::to_value(schemars::schema_for!(AcceptedSelection))?;
    assert_eq!(
        selection["properties"]["admission_mode"]["enum"],
        json!(["unprofiled", "profiled"])
    );
    let object = selection.as_object_mut().ok_or("selection schema")?;
    object.remove("$schema");
    object.remove("title");
    for schema in [
        serde_json::to_value(schemars::schema_for!(ExecutionReceipt))?,
        serde_json::to_value(schemars::schema_for!(ExecutionUsageEvidence))?,
    ] {
        assert!(schema["properties"].get("schema_version").is_none());
        assert_eq!(
            schema["properties"]["dispatch"]["oneOf"][0]["properties"]["mode"]["const"],
            "strict"
        );
        assert_eq!(
            schema["properties"]["dispatch"]["oneOf"][1]["properties"]["mode"]["const"],
            "ordered"
        );
        assert_eq!(
            schema["properties"]["dispatch"]["oneOf"][1]["properties"]["accepted_selection"],
            selection,
            "embedded accepted selection drift"
        );
    }
    Ok(())
}

#[test]
fn schema_qualification_fixtures_preserve_shape_and_semantic_boundaries() -> TestResult {
    let cases = fixtures::cases()?;
    assert!(cases.len() > 200);
    if let Ok(path) = std::env::var("BITROUTER_ORDERED_SCHEMA_FIXTURES") {
        // Optional export for an independent JSON Schema validator; no new AI dependency.
        std::fs::write(
            path,
            serde_json::to_vec(
                &json!({"schemas":exported()?.into_iter().collect::<serde_json::Map<_,_>>(), "cases":cases}),
            )?,
        )?;
    }
    Ok(())
}
