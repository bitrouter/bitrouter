use bitrouter_ai::tenancy::{
    admission::AdmissionProfile, resources::ResourcePut, validation::Validate,
};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[cfg(feature = "schema")]
#[test]
fn shared_health_schema_requires_explicit_group() -> TestResult {
    let schema = serde_json::to_value(schemars::schema_for!(AdmissionProfile))?;
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
    json!({"schema_version":1,"rate":{"rate_group_id":"window","member_rpm":1},
        "health":{"rules":"availability_v1","failure_threshold":3,
        "transient_cooldown_seconds":30,"persistent_cooldown_seconds":3600,"probe_lease_seconds":60},
        "health_group_id":"circuit","fallback":"ordered_terminal_rejections_v1"})
}

fn command() -> Value {
    json!({"format":1,"operation_id":"create-member","installation_id":"installation",
        "key":{"owner_kind":"operator_pool","owner_id":"pool","resource_kind":"policy","resource_id":"member"},
        "expected_revision":null,"definition":{"kind":"admission_profile","profile":profile()}})
}

#[test]
fn shared_profile_and_put_use_one_current_contract() -> TestResult {
    let value = profile();
    let decoded: AdmissionProfile = serde_json::from_value(value.clone())?;
    decoded.validate()?;
    assert_eq!(serde_json::to_value(decoded)?, value);
    let value = command();
    let decoded: ResourcePut = serde_json::from_value(value.clone())?;
    decoded.validate()?;
    assert_eq!(serde_json::to_value(decoded)?, value);
    let mut missing = profile();
    missing
        .as_object_mut()
        .ok_or("profile")?
        .remove("health_group_id");
    assert!(serde_json::from_value::<AdmissionProfile>(missing).is_err());
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
            serde_json::from_value::<AdmissionProfile>(invalid).is_err(),
            "missing {field}"
        );
    }
    for field in ["health", "health_group_id"] {
        let mut invalid = profile();
        invalid[field] = Value::Null;
        assert!(
            serde_json::from_value::<AdmissionProfile>(invalid)?
                .validate()
                .is_err()
        );
    }
    let mut bypass = profile();
    bypass["rate"] = Value::Null;
    bypass["health"] = Value::Null;
    bypass["health_group_id"] = Value::Null;
    bypass["observe_rate_group_id"] = json!("window");
    serde_json::from_value::<AdmissionProfile>(bypass)?.validate()?;
    let mut conflict = profile();
    conflict["observe_rate_group_id"] = json!("different-window");
    assert!(
        serde_json::from_value::<AdmissionProfile>(conflict)?
            .validate()
            .is_err()
    );
    let mut unknown_rules = profile();
    unknown_rules["health"]["rules"] = json!("availability_v2");
    assert!(serde_json::from_value::<AdmissionProfile>(unknown_rules).is_err());
    for version in [0, 2, 3] {
        let mut invalid = profile();
        invalid["schema_version"] = json!(version);
        assert!(
            serde_json::from_value::<AdmissionProfile>(invalid)?
                .validate()
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn shared_numbers_preserve_exact_integral_decimal_and_full_width_rules() -> TestResult {
    let raw = r#"{"schema_version":1.0,"rate":{"rate_group_id":"window","member_rpm":2.5e1},"health":{"rules":"availability_v1","failure_threshold":3e0,"transient_cooldown_seconds":0.0,"persistent_cooldown_seconds":1.8446744073709551615e19,"probe_lease_seconds":6e1},"health_group_id":"circuit","fallback":"stop"}"#;
    let decoded: AdmissionProfile = serde_json::from_str(raw)?;
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
        serde_json::from_str::<AdmissionProfile>(&canonical)?,
        decoded
    );
    for (needle, invalid) in [
        ("2.5e1", "2.5e0"),
        ("2.5e1", "4294967296"),
        ("2.5e1", "-1"),
        ("3e0", "3.0000000000000000001"),
        ("1.8446744073709551615e19", "18446744073709551616"),
        ("1.8446744073709551615e19", "18446744073709551615.1"),
        ("1.0", "1.01"),
    ] {
        assert!(
            serde_json::from_str::<AdmissionProfile>(&raw.replacen(needle, invalid, 1)).is_err(),
            "{invalid}"
        );
    }
    Ok(())
}

#[test]
fn resource_decoder_rejects_duplicate_unknown_and_historical_commands() -> TestResult {
    let raw = serde_json::to_string(&command())?;
    for changed in [
        raw.replace("\"format\":1", "\"format\":1,\"format\":1"),
        raw.replace("\"format\":1", "\"format\":1.1"),
        raw.replace("admission_profile", "admission_profile_v2"),
        raw.replace(
            "\"health_group_id\":\"circuit\"",
            "\"health_group_id\":\"circuit\",\"health_group_id\":null",
        ),
        raw.replacen('{', "{\"unknown\":null,", 1),
    ] {
        assert!(serde_json::from_str::<ResourcePut>(&changed).is_err());
    }
    for version in [2, 3] {
        let mut value = command();
        value["format"] = json!(version);
        assert!(
            serde_json::from_value::<ResourcePut>(value)?
                .validate()
                .is_err()
        );
    }
    serde_json::from_str::<ResourcePut>(&raw.replace("\"format\":1", "\"format\":1e0"))?
        .validate()?;
    for (field, invalid) in [("owner_kind", "tenant"), ("resource_kind", "route")] {
        let mut value = command();
        value["key"][field] = json!(invalid);
        assert!(
            serde_json::from_value::<ResourcePut>(value)?
                .validate()
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn rotation_changes_only_member_rpm_including_zero() -> TestResult {
    let original: AdmissionProfile = serde_json::from_value(profile())?;
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
        ("/schema_version", json!(2)),
        ("/health/failure_threshold", json!(4)),
        ("/health/transient_cooldown_seconds", json!(31)),
        ("/health/persistent_cooldown_seconds", json!(3601)),
        ("/health/probe_lease_seconds", json!(61)),
    ] {
        let mut next = profile();
        *next.pointer_mut(path).ok_or("field")? = value;
        let next: AdmissionProfile = serde_json::from_value(next)?;
        assert!(!original.permits_rotation_to(&next), "{path}");
    }
    let mut next = original.clone();
    next.observe_rate_group_id = Some("window".try_into()?);
    assert!(!original.permits_rotation_to(&next));
    Ok(())
}
