use bitrouter_ai::tenancy::{
    admission::AdmissionProfileV1,
    admission_v2::AdmissionProfileV2,
    resources::ResourcePut,
    resources_v2::{ResourceCapabilitiesV2, ResourcePutRequest, ResourcePutV2},
    validation::Validate,
};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[cfg(feature = "schema")]
#[test]
fn shared_health_schemas_are_separate_closed_versioned_contracts() -> TestResult {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/specs/resource-provisioning-v2");
    for (name, schema) in [
        ("put.schema.json", schemars::schema_for!(ResourcePutV2)),
        (
            "capabilities.schema.json",
            schemars::schema_for!(ResourceCapabilitiesV2),
        ),
    ] {
        let value = serde_json::to_value(&schema)?;
        assert_eq!(value["additionalProperties"], false);
        let rendered = format!("{}\n", serde_json::to_string_pretty(&schema)?);
        let path = directory.join(name);
        // Bootstrap only new schema artifacts. Existing compatibility fixtures
        // cannot be overwritten by this switch.
        if std::env::var("BITROUTER_WRITE_SHARED_HEALTH_SCHEMAS").as_deref() == Ok("1")
            && !path.exists()
        {
            std::fs::create_dir_all(&directory)?;
            std::fs::write(&path, &rendered)?;
        }
        assert_eq!(std::fs::read_to_string(path)?, rendered);
    }
    let schema = serde_json::to_value(schemars::schema_for!(AdmissionProfileV2))?;
    let required = schema["required"].as_array().ok_or("required fields")?;
    for field in [
        "schema_version",
        "rate",
        "health",
        "health_group_id",
        "fallback",
    ] {
        assert!(required.iter().any(|value| value == field));
    }
    assert_eq!(schema["additionalProperties"], false);
    Ok(())
}

fn profile() -> Value {
    json!({"schema_version":2,"rate":{"rate_group_id":"window","member_rpm":1},
        "health":{"rules":"availability_v1","failure_threshold":3,
        "transient_cooldown_seconds":30,"persistent_cooldown_seconds":3600,"probe_lease_seconds":60},
        "health_group_id":"circuit","fallback":"ordered_terminal_rejections_v1"})
}

fn command() -> Value {
    json!({"format":2,"operation_id":"create-member","installation_id":"installation",
        "key":{"owner_kind":"operator_pool","owner_id":"pool","resource_kind":"policy","resource_id":"member"},
        "expected_revision":null,"definition":{"kind":"admission_profile_v2","profile":profile()}})
}

#[test]
fn shared_profile_and_put_round_trip_without_extending_v1() -> TestResult {
    let value = profile();
    let decoded: AdmissionProfileV2 = serde_json::from_value(value.clone())?;
    decoded.validate()?;
    assert_eq!(serde_json::to_value(decoded)?, value);
    assert!(serde_json::from_value::<AdmissionProfileV1>(value).is_err());
    let value = command();
    let decoded: ResourcePutV2 = serde_json::from_value(value.clone())?;
    decoded.validate()?;
    assert_eq!(serde_json::to_value(decoded)?, value);
    assert!(serde_json::from_value::<ResourcePut>(value.clone()).is_err());
    assert!(matches!(
        serde_json::from_value::<ResourcePutRequest>(value)?,
        ResourcePutRequest::V2(_)
    ));
    let original = json!({"format":1,"operation_id":"old","installation_id":"installation",
        "key":{"owner_kind":"operator_pool","owner_id":"pool","resource_kind":"policy","resource_id":"local"},
        "expected_revision":null,"definition":{"kind":"admission_profile","profile":{
        "schema_version":1,"rate":null,"health":null,"fallback":"stop"}}});
    let ResourcePutRequest::V1(decoded) = serde_json::from_value(original.clone())? else {
        return Err("wrong original decoder".into());
    };
    decoded.validate()?;
    assert_eq!(serde_json::to_value(decoded)?, original);
    Ok(())
}

#[test]
fn shared_health_requires_exact_explicit_nullable_association() -> TestResult {
    for field in [
        "health",
        "health_group_id",
        "rate",
        "fallback",
        "schema_version",
    ] {
        let mut invalid = profile();
        invalid.as_object_mut().ok_or("profile")?.remove(field);
        assert!(
            serde_json::from_value::<AdmissionProfileV2>(invalid).is_err(),
            "missing {field}"
        );
    }
    for field in ["health", "health_group_id"] {
        let mut invalid = profile();
        invalid[field] = Value::Null;
        assert!(
            serde_json::from_value::<AdmissionProfileV2>(invalid)?
                .validate()
                .is_err()
        );
    }
    let mut bypass = profile();
    bypass["rate"] = Value::Null;
    bypass["health"] = Value::Null;
    bypass["health_group_id"] = Value::Null;
    bypass["observe_rate_group_id"] = json!("window");
    serde_json::from_value::<AdmissionProfileV2>(bypass)?.validate()?;
    let mut conflict = profile();
    conflict["observe_rate_group_id"] = json!("different-window");
    assert!(
        serde_json::from_value::<AdmissionProfileV2>(conflict)?
            .validate()
            .is_err()
    );
    let mut unknown_rules = profile();
    unknown_rules["health"]["rules"] = json!("availability_v2");
    assert!(serde_json::from_value::<AdmissionProfileV2>(unknown_rules).is_err());
    for version in [0, 1, 3] {
        let mut invalid = profile();
        invalid["schema_version"] = json!(version);
        assert!(
            serde_json::from_value::<AdmissionProfileV2>(invalid)?
                .validate()
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn shared_numbers_preserve_exact_integral_decimal_and_full_width_rules() -> TestResult {
    let raw = r#"{"schema_version":2.0,"rate":{"rate_group_id":"window","member_rpm":2.5e1},"health":{"rules":"availability_v1","failure_threshold":3e0,"transient_cooldown_seconds":0.0,"persistent_cooldown_seconds":1.8446744073709551615e19,"probe_lease_seconds":6e1},"health_group_id":"circuit","fallback":"stop"}"#;
    let decoded: AdmissionProfileV2 = serde_json::from_str(raw)?;
    decoded.validate()?;
    assert_eq!(decoded.rate.as_ref().ok_or("rate")?.member_rpm, 25);
    assert_eq!(
        decoded
            .health
            .as_ref()
            .ok_or("health")?
            .persistent_cooldown_seconds,
        u64::MAX
    );
    let canonical = serde_json::to_string(&decoded)?;
    assert_eq!(
        serde_json::from_str::<AdmissionProfileV2>(&canonical)?,
        decoded
    );
    for (needle, invalid) in [
        ("2.5e1", "2.5e0"),
        ("2.5e1", "4294967296"),
        ("2.5e1", "-1"),
        ("3e0", "3.0000000000000000001"),
        ("1.8446744073709551615e19", "18446744073709551616"),
        ("1.8446744073709551615e19", "18446744073709551615.1"),
        ("2.0", "2.01"),
    ] {
        assert!(
            serde_json::from_str::<AdmissionProfileV2>(&raw.replacen(needle, invalid, 1)).is_err(),
            "{invalid}"
        );
    }
    Ok(())
}

#[test]
fn format_dispatch_rejects_ambiguous_and_foreign_commands_before_validation() -> TestResult {
    let raw = serde_json::to_string(&command())?;
    for changed in [
        raw.replace("\"format\":2", "\"format\":2,\"format\":1"),
        raw.replace("\"format\":2", "\"format\":3"),
        raw.replace("\"format\":2", "\"format\":2.1"),
        raw.replace(
            "\"kind\":\"admission_profile_v2\"",
            "\"kind\":\"admission_profile\"",
        ),
        raw.replace(
            "\"health_group_id\":\"circuit\"",
            "\"health_group_id\":\"circuit\",\"health_group_id\":null",
        ),
        raw.replacen('{', "{\"unknown\":null,", 1),
    ] {
        assert!(serde_json::from_str::<ResourcePutRequest>(&changed).is_err());
    }
    serde_json::from_str::<ResourcePutRequest>(&raw.replace("\"format\":2", "\"format\":2e0"))?
        .validate()?;
    for (field, invalid) in [("owner_kind", "tenant"), ("resource_kind", "route")] {
        let mut value = command();
        value["key"][field] = json!(invalid);
        assert!(
            serde_json::from_value::<ResourcePutV2>(value)?
                .validate()
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn rotation_changes_only_member_rpm_including_zero() -> TestResult {
    let original: AdmissionProfileV2 = serde_json::from_value(profile())?;
    for rpm in [0, 1, u32::MAX] {
        let mut next = original.clone();
        next.rate.as_mut().ok_or("rate")?.member_rpm = rpm;
        assert!(original.permits_rotation_to(&next));
    }
    for (path, value) in [
        ("/health_group_id", json!("other")),
        ("/health", Value::Null),
        ("/rate", Value::Null),
        ("/rate/rate_group_id", json!("other")),
        ("/fallback", json!("stop")),
        ("/schema_version", json!(1)),
        ("/health/failure_threshold", json!(4)),
        ("/health/transient_cooldown_seconds", json!(31)),
        ("/health/persistent_cooldown_seconds", json!(3601)),
        ("/health/probe_lease_seconds", json!(61)),
    ] {
        let mut next = profile();
        *next.pointer_mut(path).ok_or("field")? = value;
        let next: AdmissionProfileV2 = serde_json::from_value(next)?;
        assert!(!original.permits_rotation_to(&next), "{path}");
    }
    let mut next = original.clone();
    next.observe_rate_group_id = Some("window".try_into()?);
    assert!(!original.permits_rotation_to(&next));
    Ok(())
}

#[test]
fn discovery_is_closed_versioned_and_never_infers_shared_support() -> TestResult {
    let raw = r#"{"format":2.0,"installation_id":"installation","provisioning_enabled":true,"max_command_bytes":1048576,"tenant_routes_require_approved_origins":true,"admission_profile_versions":[1.0,2e0]}"#;
    serde_json::from_str::<ResourceCapabilitiesV2>(raw)?.validate()?;
    for (needle, replacement) in [
        ("[1.0,2e0]", "[1,1]"),
        ("\"format\":2.0", "\"format\":1"),
        ("1048576", "0"),
        ("1048576", "1048577"),
        (
            "\"tenant_routes_require_approved_origins\":true",
            "\"tenant_routes_require_approved_origins\":false",
        ),
    ] {
        assert!(
            serde_json::from_str::<ResourceCapabilitiesV2>(&raw.replace(needle, replacement))?
                .validate()
                .is_err()
        );
    }
    for versions in ["[1]", "[1,2,3]", "[1.1,2]", "[1,256]"] {
        assert!(
            serde_json::from_str::<ResourceCapabilitiesV2>(&raw.replace("[1.0,2e0]", versions))
                .is_err()
        );
    }
    Ok(())
}
