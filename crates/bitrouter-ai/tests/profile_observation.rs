use bitrouter_ai::tenancy::{
    profile_observation::{
        AdmissionProfileObservationCapabilitiesV1 as Capabilities,
        AdmissionProfileObservationRequestV1 as Request,
        AdmissionProfileObservationSnapshotV1 as Snapshot, ProfileObservationV1,
    },
    validation::Validate,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
type TestResult = Result<(), Box<dyn std::error::Error>>;

fn reference() -> Value {
    json!({"owner_kind":"operator_pool","owner_id":"pool","resource_kind":"policy","resource_id":"member","revision_id":"pin"})
}
fn request() -> Value {
    json!({"schema_version":1,"expected_authority_revision":7,"profiles":[reference()]})
}
fn entry() -> Value {
    json!({"profile":reference(),"current_profile_revision":"head","rate":{"member_rpm":10,"shared_count":2},"observed_shared_count":2,"health":"healthy","availability":"eligible"})
}
fn snapshot() -> Value {
    json!({"schema_version":1,"installation_id":"installation","instance_id":"instance","authority_revision":7,"observed_at":"2026-11-03T00:00:00Z","profiles":[entry()]})
}
fn capabilities() -> Value {
    json!({"contract_version":1,"installation_id":"installation","instance_id":"instance","authority_revision":7,"observation_enabled":true,"max_profiles":256,"max_request_bytes":1048576,"max_response_bytes":1048576,"profile_versions":[1,2]})
}
fn valid<T: DeserializeOwned + Validate>(value: Value) -> bool {
    serde_json::from_value::<T>(value).is_ok_and(|v| v.validate().is_ok())
}
fn closed<T: DeserializeOwned + Serialize + Validate>(value: Value) -> TestResult {
    let decoded: T = serde_json::from_value(value.clone())?;
    decoded.validate()?;
    assert_eq!(serde_json::to_value(decoded)?, value);
    for field in value.as_object().ok_or("object")?.keys() {
        let mut missing = value.clone();
        missing.as_object_mut().ok_or("object")?.remove(field);
        assert!(
            serde_json::from_value::<T>(missing).is_err(),
            "missing {field}"
        );
        let raw = serde_json::to_string(&value)?;
        let duplicate = format!(
            "{{{}:{},{}",
            serde_json::to_string(field)?,
            value[field],
            &raw[1..]
        );
        assert!(
            serde_json::from_str::<T>(&duplicate).is_err(),
            "duplicate {field}"
        );
    }
    let mut unknown = value;
    unknown["unknown"] = json!(0);
    assert!(serde_json::from_value::<T>(unknown).is_err());
    Ok(())
}

#[test]
fn all_profile_read_objects_are_closed_with_required_nullable_fields() -> TestResult {
    closed::<Capabilities>(capabilities())?;
    closed::<Request>(request())?;
    closed::<Snapshot>(snapshot())?;
    closed::<ProfileObservationV1>(entry())?;
    let mut denied = entry();
    for field in ["current_profile_revision", "rate", "observed_shared_count"] {
        denied[field] = Value::Null;
    }
    denied["health"] = json!("unavailable");
    denied["availability"] = json!("authority_denied");
    closed::<ProfileObservationV1>(denied)?;
    Ok(())
}

#[test]
fn exact_integral_numeric_spellings_normalize_without_float_rounding() -> TestResult {
    for (original, replacements) in [
        (
            capabilities(),
            vec![
                ("\"contract_version\":1", "\"contract_version\":1e0"),
                ("[1,2]", "[1.0,2e0]"),
                ("256", "2.56e2"),
                ("1048576", "1048576.0"),
            ],
        ),
        (
            request(),
            vec![
                ("\"schema_version\":1", "\"schema_version\":1.0"),
                (
                    "\"expected_authority_revision\":7",
                    "\"expected_authority_revision\":7e0",
                ),
            ],
        ),
        (
            snapshot(),
            vec![
                ("\"schema_version\":1", "\"schema_version\":1.0"),
                ("\"authority_revision\":7", "\"authority_revision\":7e0"),
                ("\"member_rpm\":10", "\"member_rpm\":1e1"),
                ("\"shared_count\":2", "\"shared_count\":2.0"),
                (
                    "\"observed_shared_count\":2",
                    "\"observed_shared_count\":2e0",
                ),
            ],
        ),
    ] {
        let mut raw = serde_json::to_string(&original)?;
        for (from, to) in replacements {
            raw = raw.replace(from, to);
        }
        let canonical = if original.get("contract_version").is_some() {
            serde_json::to_value(serde_json::from_str::<Capabilities>(&raw)?)?
        } else if original
            .get("profiles")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .is_some_and(|v| v.get("profile").is_some())
        {
            serde_json::to_value(serde_json::from_str::<Snapshot>(&raw)?)?
        } else {
            serde_json::to_value(serde_json::from_str::<Request>(&raw)?)?
        };
        assert_eq!(canonical, original);
    }
    let raw = serde_json::to_string(&snapshot())?;
    for (needle, invalids) in [
        ("\"schema_version\":1", vec!["1.1", "4294967296"]),
        (
            "\"authority_revision\":7",
            vec!["9007199254740992", "7.000000000000000001", "-1"],
        ),
        ("\"member_rpm\":10", vec!["4294967296", "1.5", "-1"]),
        (
            "\"shared_count\":2",
            vec!["9007199254740992", "2.000000000000000001"],
        ),
        (
            "\"observed_shared_count\":2",
            vec!["9007199254740992", "2.000000000000000001"],
        ),
    ] {
        let field = needle.split(':').next().ok_or("field")?;
        for bad in invalids {
            assert!(
                serde_json::from_str::<Snapshot>(&raw.replace(needle, &format!("{field}:{bad}")))
                    .is_err()
            );
        }
    }
    let raw = serde_json::to_string(&capabilities())?;
    for bad in ["[1.1,2]", "[1,256]", "[1]", "[1,2,3]"] {
        assert!(serde_json::from_str::<Capabilities>(&raw.replace("[1,2]", bad)).is_err());
    }
    Ok(())
}

#[test]
fn refs_are_distinct_full_operator_policy_pins_with_exact_batch_bounds() -> TestResult {
    for count in [0, 1, 256, 257] {
        let mut value = request();
        value["profiles"] = Value::Array(
            (0..count)
                .map(|i| {
                    let mut r = reference();
                    r["revision_id"] = json!(format!("r{i}"));
                    r
                })
                .collect(),
        );
        assert_eq!(valid::<Request>(value), (1..=256).contains(&count));
    }
    let mut value = request();
    value["profiles"] = json!([reference(), reference()]);
    assert!(!valid::<Request>(value));
    for (field, bad) in [("owner_kind", "tenant"), ("resource_kind", "route")] {
        let mut value = request();
        value["profiles"][0][field] = json!(bad);
        assert!(!valid::<Request>(value));
        let mut value = snapshot();
        value["profiles"][0]["profile"][field] = json!(bad);
        assert!(!valid::<Snapshot>(value));
    }
    for field in ["max_profiles", "max_request_bytes", "max_response_bytes"] {
        for bad in [
            0,
            if field == "max_profiles" {
                257
            } else {
                1048577
            },
        ] {
            let mut value = capabilities();
            value[field] = json!(bad);
            assert!(!valid::<Capabilities>(value));
        }
    }
    for version in [0, 2] {
        let mut value = request();
        value["schema_version"] = json!(version);
        assert!(!valid::<Request>(value));
        let mut value = snapshot();
        value["schema_version"] = json!(version);
        assert!(!valid::<Snapshot>(value));
        let mut value = capabilities();
        value["contract_version"] = json!(version);
        assert!(!valid::<Capabilities>(value));
    }
    Ok(())
}

#[test]
fn independent_presence_never_allows_conflicting_shared_counts() -> TestResult {
    for rate in [true, false] {
        for observed in [true, false] {
            let mut value = entry();
            if !rate {
                value["rate"] = Value::Null;
            }
            if !observed {
                value["observed_shared_count"] = Value::Null;
            }
            assert!(valid::<ProfileObservationV1>(value));
        }
    }
    let mut value = entry();
    value["observed_shared_count"] = json!(9);
    assert!(!valid::<ProfileObservationV1>(value));
    let mut typed: ProfileObservationV1 = serde_json::from_value(entry())?;
    typed.rate.as_mut().ok_or("rate")?.shared_count = u64::MAX;
    typed.observed_shared_count = None;
    assert!(typed.validate().is_err());
    typed.rate = None;
    typed.observed_shared_count = Some(u64::MAX);
    assert!(typed.validate().is_err());
    Ok(())
}

#[test]
fn authority_availability_and_zero_rpm_follow_actual_sampler_priority() -> TestResult {
    let mut value = entry();
    value["rate"]["member_rpm"] = json!(0);
    value["rate"]["shared_count"] = json!(0);
    value["observed_shared_count"] = json!(0);
    assert!(!valid::<ProfileObservationV1>(value.clone()));
    value["availability"] = json!("rate_limited");
    assert!(valid::<ProfileObservationV1>(value.clone()));
    value["health"] = json!("blocked");
    assert!(!valid::<ProfileObservationV1>(value.clone()));
    value["availability"] = json!("circuit_blocked");
    assert!(valid::<ProfileObservationV1>(value));
    for (field, bad) in [
        ("availability", json!("authority_denied")),
        ("availability", json!("rate_limited")),
        ("availability", json!("circuit_blocked")),
        ("health", json!("blocked")),
        ("health", json!("unavailable")),
        ("current_profile_revision", Value::Null),
    ] {
        let mut value = entry();
        value[field] = bad;
        assert!(!valid::<ProfileObservationV1>(value));
    }
    let mut value = entry();
    value["availability"] = json!("capacity_denied");
    assert!(valid::<ProfileObservationV1>(value));
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn separate_profile_read_schemas_preserve_required_null_and_ref_bounds() -> TestResult {
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/service-api");
    for (name, schema) in [
        (
            "profile-observation-capabilities-v1.schema.json",
            schemars::schema_for!(Capabilities),
        ),
        (
            "profile-observation-request-v1.schema.json",
            schemars::schema_for!(Request),
        ),
        (
            "profile-observation-snapshot-v1.schema.json",
            schemars::schema_for!(Snapshot),
        ),
        (
            "profile-observation-entry-v1.schema.json",
            schemars::schema_for!(ProfileObservationV1),
        ),
    ] {
        let value = serde_json::to_value(&schema)?;
        assert_eq!(value["additionalProperties"], false);
        assert_eq!(
            value["properties"].as_object().ok_or("properties")?.len(),
            value["required"].as_array().ok_or("required")?.len()
        );
        let rendered = format!("{}\n", serde_json::to_string_pretty(&schema)?);
        let path = directory.join(name);
        // Only bootstrap new artifacts. Existing schemas are never overwritten.
        if std::env::var("BITROUTER_WRITE_PROFILE_OBSERVATION_SCHEMAS").as_deref() == Ok("1")
            && !path.exists()
        {
            std::fs::write(&path, &rendered)?;
        }
        assert_eq!(std::fs::read_to_string(path)?, rendered);
    }
    let entry_schema = serde_json::to_value(schemars::schema_for!(ProfileObservationV1))?;
    assert_eq!(
        entry_schema["properties"]["observed_shared_count"]["maximum"],
        9007199254740991_u64
    );
    assert_eq!(
        entry_schema["$defs"]["RateObservationV1"]["properties"]["shared_count"]["maximum"],
        9007199254740991_u64
    );
    let schema = serde_json::to_value(schemars::schema_for!(Request))?;
    assert_eq!(schema["properties"]["profiles"]["maxItems"], 256);
    assert_eq!(schema["properties"]["profiles"]["uniqueItems"], true);
    assert_eq!(
        schema["properties"]["profiles"]["items"]["allOf"][0]["properties"]["owner_kind"]["const"],
        "operator_pool"
    );
    Ok(())
}
