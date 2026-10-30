//! Artifact wire contracts compile independently of the application runtime.

use bitrouter_ai::artifact::{ArtifactIdentity, DistributionProfile};
use serde_json::json;

#[test]
fn profiles_keep_stable_wire_and_display_spellings() -> Result<(), Box<dyn std::error::Error>> {
    for (profile, spelling) in [
        (DistributionProfile::Full, "full"),
        (DistributionProfile::Gateway, "gateway"),
        (DistributionProfile::GatewayShared, "gateway-shared"),
        (DistributionProfile::Custom, "custom"),
    ] {
        assert_eq!(profile.as_str(), spelling);
        assert_eq!(serde_json::to_value(profile)?, json!(spelling));
        assert_eq!(
            serde_json::from_value::<DistributionProfile>(json!(spelling))?,
            profile
        );
    }
    // Unknown profiles must not silently become a supported distribution.
    assert!(serde_json::from_value::<DistributionProfile>(json!("future-profile")).is_err());
    Ok(())
}

#[test]
fn missing_source_revision_stays_explicitly_unknown() -> Result<(), Box<dyn std::error::Error>> {
    let mut wire = json!({
        "version": "1.2.3",
        "target": "x86_64-unknown-linux-gnu",
        "features": ["harness", "mcp"],
        "profile": "custom",
        "handoff_build_id": "fixture-handoff"
    });
    let identity: ArtifactIdentity = serde_json::from_value(wire.clone())?;
    assert!(identity.source_revision.is_none());
    assert_eq!(identity.clone(), identity);
    assert!(format!("{identity:?}").contains("source_revision: None"));
    wire["source_revision"] = json!(null);
    assert_eq!(serde_json::to_value(&identity)?, wire);
    assert_eq!(
        serde_json::from_value::<ArtifactIdentity>(wire.clone())?,
        identity
    );

    wire["source_revision"] = json!("0123456789abcdef0123456789abcdef01234567");
    let known: ArtifactIdentity = serde_json::from_value(wire.clone())?;
    assert_ne!(known, identity);
    assert_eq!(serde_json::to_value(known)?, wire);
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn artifact_schema_exposes_nullable_revision_and_closed_profiles()
-> Result<(), Box<dyn std::error::Error>> {
    let schema = serde_json::to_value(schemars::schema_for!(ArtifactIdentity))?;
    assert_eq!(
        schema["properties"]["source_revision"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(schema["properties"]["features"]["items"]["type"], "string");
    assert_eq!(
        schema["$defs"]["DistributionProfile"]["enum"],
        json!(["full", "gateway", "gateway-shared", "custom"])
    );
    let required = schema["required"]
        .as_array()
        .ok_or("schema has no required fields")?;
    assert!(!required.contains(&json!("source_revision")));
    for name in [
        "version",
        "target",
        "features",
        "profile",
        "handoff_build_id",
    ] {
        assert!(required.contains(&json!(name)));
    }
    Ok(())
}

#[test]
fn contract_crate_keeps_its_dependency_boundary() -> Result<(), Box<dyn std::error::Error>> {
    // All dependency declarations (including future target/build sections) must
    // stay within the contract vocabulary or optional framework bindings. Test
    // harness dependencies do not grant production runtime ownership.
    // The isolated Cargo dependency-tree gate also checks transitive dependencies.
    fn check(name: &str, declaration: &toml::Value, development: bool) {
        let contract = matches!(name, "serde" | "serde_json" | "thiserror" | "schemars");
        let binding = matches!(name, "axum" | "futures-core" | "futures-util");
        let fixture = development && matches!(name, "tokio" | "http-body-util" | "tower" | "toml");
        assert!(
            contract || binding || fixture,
            "unexpected dependency: {name}"
        );
        if binding && !development {
            assert!(
                declaration.get("optional").and_then(toml::Value::as_bool) == Some(true),
                "framework binding must remain optional: {name}"
            );
            if matches!(name, "axum" | "futures-util") {
                assert!(
                    declaration
                        .get("default-features")
                        .and_then(toml::Value::as_bool)
                        == Some(false),
                    "framework binding must disable default features: {name}"
                );
            }
        }
    }

    // Parse actual TOML values so comments cannot grant production exceptions.
    // Inline tables and cargo-package's normalized subtables share this shape.
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml"))?;
    let targets = manifest.get("target").and_then(toml::Value::as_table);
    for owner in
        std::iter::once(&manifest).chain(targets.into_iter().flat_map(|targets| targets.values()))
    {
        for kind in ["dependencies", "build-dependencies", "dev-dependencies"] {
            if let Some(dependencies) = owner.get(kind).and_then(toml::Value::as_table) {
                for (name, declaration) in dependencies {
                    check(name, declaration, kind == "dev-dependencies");
                }
            }
        }
    }
    let source = include_str!("../src/artifact.rs");
    for runtime in [
        "std::fs",
        "std::env",
        "std::process",
        "tokio::",
        "reqwest::",
        "OnceLock",
        "LazyLock",
    ] {
        assert!(
            !source.contains(runtime),
            "artifact contract acquired runtime behavior: {runtime}"
        );
    }
    Ok(())
}
