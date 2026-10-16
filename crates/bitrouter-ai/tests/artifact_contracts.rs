//! Artifact wire contracts compile independently of the application runtime.

use bitrouter_ai::artifact::{ArtifactIdentity, DistributionProfile};
use serde_json::json;

#[test]
fn profiles_keep_stable_wire_and_display_spellings() -> Result<(), Box<dyn std::error::Error>> {
    for (profile, spelling) in [
        (DistributionProfile::Full, "full"),
        (DistributionProfile::Gateway, "gateway"),
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
        json!(["full", "gateway", "custom"])
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
fn contract_crate_keeps_its_dependency_boundary() {
    // All dependency declarations (including future target/build sections) must
    // stay within the contract crate's existing serialization/error vocabulary.
    // The isolated Cargo dependency-tree gate also checks transitive dependencies.
    let mut dependencies = false;
    for line in include_str!("../Cargo.toml").lines().map(str::trim) {
        if line.starts_with('[') {
            if let Some((_, name)) = line.split_once("dependencies.") {
                // cargo package rewrites inline declarations as subtables.
                assert!(
                    matches!(
                        name.trim_end_matches(']'),
                        "serde" | "serde_json" | "thiserror" | "schemars"
                    ),
                    "unexpected contract dependency: {line}"
                );
                dependencies = false;
            } else {
                dependencies = line.contains("dependencies");
            }
            continue;
        }
        if dependencies && !line.is_empty() && !line.starts_with('#') {
            let name = line.split(['.', ' ', '=']).next();
            assert!(
                matches!(
                    name,
                    Some("serde" | "serde_json" | "thiserror" | "schemars")
                ),
                "unexpected contract dependency: {line}"
            );
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
}
