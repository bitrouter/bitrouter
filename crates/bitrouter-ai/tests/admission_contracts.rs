use bitrouter_ai::tenancy::{
    admission::{AdmissionMode, AdmissionProfileV1, AvailabilityObservation, HealthObservation},
    ordered::{
        CandidateManifestV1, GenerationProtocol, MAX_MANIFEST_BYTES, OrderedCandidateSelectionV1,
    },
    resources::{Definition, ResourcePut},
    validation::Validate,
};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn profile() -> Value {
    json!({"schema_version":1,"rate":{"rate_group_id":"shared","member_rpm":0},
        "health":{"rules":"availability_v1","failure_threshold":3,"transient_cooldown_seconds":0,
            "persistent_cooldown_seconds":3600,"probe_lease_seconds":60},"fallback":"stop"})
}

fn reference(kind: &str, maximal: bool) -> Value {
    let id = if maximal {
        "x".repeat(128)
    } else {
        "id".into()
    };
    json!({"owner_kind":"operator_pool","owner_id":id,"resource_kind":kind,
        "resource_id":id,"revision_id":id})
}

fn manifest(count: usize) -> Result<CandidateManifestV1, serde_json::Error> {
    let id = "x".repeat(128);
    serde_json::from_value(json!({"schema_version":1,"admission_mode":"profiled_v1",
        "installation_id":id,"tenant_id":id,"execution_revision":id,"authority_revision":9007199254740991_u64,
        "route":reference("route",true),"inbound_protocol":"chat_completions",
        "candidates":(0..count).map(|ordinal| json!({"candidate_index":ordinal,
            "target":{"provider_id":id,"model_id":"\u{0001}".repeat(256),"account_id":id,"credential_id":id,"credential_version":id},
            "credential":reference("credential",true),"admission_profile":reference("policy",true)})).collect::<Vec<_>>(),
        "runtime_observation":{"instance_id":id,"observed_at":"2026-11-02T00:00:00Z",
            "candidates":(0..count).map(|ordinal|json!({"candidate_index":ordinal,"current_profile_revision":id,
                "rate":{"member_rpm":u32::MAX,"shared_count":9007199254740991_u64},
                "health":"unavailable","availability":"authority_denied"})).collect::<Vec<_>>()}}))
}

#[test]
fn zero_rpm_and_disabled_are_distinct_and_required_nulls_are_not_defaults() -> TestResult {
    let zero: AdmissionProfileV1 = serde_json::from_value(profile())?;
    zero.validate()?;
    assert_eq!(zero.rate.as_ref().map(|rate| rate.member_rpm), Some(0));
    for field in ["rate", "health"] {
        let mut missing = profile();
        missing
            .as_object_mut()
            .ok_or("profile object")?
            .remove(field);
        assert!(serde_json::from_value::<AdmissionProfileV1>(missing).is_err());
        let mut null = profile();
        null[field] = Value::Null;
        serde_json::from_value::<AdmissionProfileV1>(null)?.validate()?;
    }
    for (pointer, value) in [
        ("/schema_version", json!(2)),
        ("/rate/member_rpm", json!(-1)),
        ("/rate/member_rpm", json!(4294967296_u64)),
        ("/health/rules", json!("unknown")),
        ("/health/failure_threshold", json!(0)),
        ("/health/probe_lease_seconds", json!(0)),
        ("/health/transient_cooldown_seconds", json!(3601)),
        ("/fallback", json!("retry")),
    ] {
        let mut value_to_parse = profile();
        *value_to_parse.pointer_mut(pointer).ok_or("pointer")? = value;
        assert!(
            !serde_json::from_value::<AdmissionProfileV1>(value_to_parse)
                .is_ok_and(|profile| profile.validate().is_ok()),
            "{pointer}"
        );
    }
    let mut unknown = profile();
    unknown["health"]["retry"] = json!(true);
    assert!(serde_json::from_value::<AdmissionProfileV1>(unknown).is_err());
    Ok(())
}

#[test]
fn cooldown_integers_preserve_full_u64_without_fraction_rounding() -> TestResult {
    let mut value = profile();
    for field in [
        "transient_cooldown_seconds",
        "persistent_cooldown_seconds",
        "probe_lease_seconds",
    ] {
        value["health"][field] = json!(u64::MAX);
    }
    let encoded = serde_json::to_string(&value)?;
    let parsed: AdmissionProfileV1 = serde_json::from_str(&encoded)?;
    parsed.validate()?;
    assert_eq!(
        parsed
            .health
            .as_ref()
            .map(|health| health.probe_lease_seconds),
        Some(u64::MAX)
    );
    for bad in [
        "18446744073709551616",
        "18446744073709551615.1",
        "1e100000",
        "-1",
    ] {
        assert!(
            serde_json::from_str::<AdmissionProfileV1>(
                &encoded.replace("18446744073709551615", bad)
            )
            .is_err()
        );
    }
    let integral: AdmissionProfileV1 =
        serde_json::from_str(&encoded.replace("18446744073709551615", "184467440737095516150e-1"))?;
    assert_eq!(parsed, integral);
    Ok(())
}

#[test]
fn live_rotations_only_change_the_member_ceiling() -> TestResult {
    let original: AdmissionProfileV1 = serde_json::from_value(profile())?;
    let mut ceiling = profile();
    ceiling["rate"]["member_rpm"] = json!(u32::MAX);
    assert!(original.permits_rotation_to(&serde_json::from_value(ceiling)?));
    for (pointer, replacement) in [
        ("/rate/rate_group_id", json!("other")),
        ("/rate", Value::Null),
        ("/health", Value::Null),
        ("/health/failure_threshold", json!(4)),
        ("/fallback", json!("ordered_terminal_rejections_v1")),
    ] {
        let mut changed = profile();
        *changed.pointer_mut(pointer).ok_or("pointer")? = replacement;
        assert!(
            !original.permits_rotation_to(&serde_json::from_value(changed)?),
            "{pointer}"
        );
    }
    Ok(())
}

#[test]
fn read_only_group_is_explicit_same_owner_immutable_and_never_a_rate_ceiling() -> TestResult {
    let original: AdmissionProfileV1 = serde_json::from_value(profile())?;
    assert!(
        !serde_json::to_value(&original)?
            .as_object()
            .ok_or("profile")?
            .contains_key("observe_rate_group_id")
    );
    let mut observed = original.clone();
    observed.observe_rate_group_id = Some("shared".try_into()?);
    observed.validate()?;
    assert!(!original.permits_rotation_to(&observed));
    let mut rotated = observed.clone();
    rotated.rate.as_mut().ok_or("rate")?.member_rpm = 5;
    assert!(observed.permits_rotation_to(&rotated));
    rotated.observe_rate_group_id = Some("foreign".try_into()?);
    assert!(rotated.validate().is_err());
    assert!(!observed.permits_rotation_to(&rotated));
    observed.rate = None;
    observed.health = None;
    observed.validate()?;
    assert!(observed.rate.is_none());
    let roundtrip: AdmissionProfileV1 = serde_json::from_slice(&serde_json::to_vec(&observed)?)?;
    assert_eq!(roundtrip, observed);
    Ok(())
}

#[test]
fn read_only_observation_preserves_bounds_authority_and_exact_counts() -> TestResult {
    let mut value = manifest(256)?;
    for candidate in &mut value
        .runtime_observation
        .as_mut()
        .ok_or("observation")?
        .candidates
    {
        candidate.observed_shared_count = Some(9007199254740991);
    }
    value.validate()?;
    let bytes = serde_json::to_vec(&value)?;
    assert!(bytes.len() < MAX_MANIFEST_BYTES);
    let candidate = &mut value
        .runtime_observation
        .as_mut()
        .ok_or("observation")?
        .candidates[0];
    candidate.observed_shared_count = Some(0);
    assert!(
        value.validate().is_err(),
        "same-group counts cannot conflict"
    );
    let candidate = &mut value
        .runtime_observation
        .as_mut()
        .ok_or("observation")?
        .candidates[0];
    candidate.rate = None;
    candidate.health = HealthObservation::Disabled;
    candidate.availability = AvailabilityObservation::Eligible;
    value.validate()?;
    value
        .runtime_observation
        .as_mut()
        .ok_or("observation")?
        .candidates[0]
        .current_profile_revision = None;
    assert!(
        value.validate().is_err(),
        "missing authority is not observed zero"
    );
    let mut raw = serde_json::to_value(manifest(1)?)?;
    for invalid in [json!(-1), json!(0.5), json!(9007199254740992_u64)] {
        raw["runtime_observation"]["candidates"][0]["observed_shared_count"] = invalid;
        assert!(serde_json::from_value::<CandidateManifestV1>(raw.clone()).is_err());
    }
    Ok(())
}

#[test]
fn actual_selection_serialization_preserves_arbitrary_order_and_requires_mode() -> TestResult {
    for (count, maximum) in [(1, 572), (8, 600), (9, 604), (64, 824)] {
        let indices: Vec<_> = (0..count).map(|i| 255 - i).collect();
        let selected: OrderedCandidateSelectionV1 = serde_json::from_value(
            json!({"schema_version":1,
            "admission_mode":"profiled_v1","route":reference("route",true),"candidate_indices":indices}),
        )?;
        selected.validate()?;
        assert_eq!(serde_json::to_vec(&selected)?.len(), maximum);
        let mut with_defaults = selected.clone();
        with_defaults.generation_defaults_digest = Some("a".repeat(64).try_into()?);
        let bytes = serde_json::to_vec(&with_defaults)?.len();
        assert_eq!(bytes, maximum + 96);
        assert!((bytes * 8).div_ceil(6) <= 4096);
        let accepted = selected.accept(GenerationProtocol::Responses)?;
        assert_eq!(accepted.admission_mode, AdmissionMode::ProfiledV1);
        assert_eq!(accepted.candidate_indices, indices);
        let mut missing = serde_json::to_value(selected)?;
        missing
            .as_object_mut()
            .ok_or("selection")?
            .remove("admission_mode");
        assert!(serde_json::from_value::<OrderedCandidateSelectionV1>(missing).is_err());
    }
    Ok(())
}

#[test]
fn final_manifest_bound_includes_observations_and_escaped_models() -> TestResult {
    let maximum = manifest(256)?;
    maximum.validate()?;
    let bytes = serde_json::to_vec(&maximum)?;
    assert_eq!(bytes.len(), 894522);
    assert!(bytes.len() < MAX_MANIFEST_BYTES);
    let mut observed = maximum.clone();
    for candidate in &mut observed
        .runtime_observation
        .as_mut()
        .ok_or("observation")?
        .candidates
    {
        candidate.observed_shared_count = Some(candidate.rate.as_ref().ok_or("rate")?.shared_count);
    }
    observed.validate()?;
    assert_eq!(serde_json::to_vec(&observed)?.len(), 905018);
    let mut oversized = serde_json::to_value(maximum)?;
    oversized["runtime_observation"]["observed_at"] = json!(format!(
        "2026-11-02T00:00:00.{}Z",
        "0".repeat(MAX_MANIFEST_BYTES)
    ));
    let typed: CandidateManifestV1 = serde_json::from_value(oversized)?;
    assert!(typed.validate().is_err());
    Ok(())
}

#[test]
fn observations_cannot_relabel_foreign_ordinals_or_missing_authority_as_zero() -> TestResult {
    let mut baseline = manifest(2)?;
    let observation = baseline.runtime_observation.as_mut().ok_or("observation")?;
    observation.candidates[0].current_profile_revision = None;
    assert!(baseline.validate().is_err());
    let observation = baseline.runtime_observation.as_mut().ok_or("observation")?;
    observation.candidates[0].rate = None;
    baseline.validate()?;
    let observation = baseline.runtime_observation.as_mut().ok_or("observation")?;
    observation.candidates[0].health = HealthObservation::Disabled;
    observation.candidates[0].availability = AvailabilityObservation::Eligible;
    assert!(baseline.validate().is_err());
    for change in 0..3 {
        let mut changed = manifest(2)?;
        let observation = changed.runtime_observation.as_mut().ok_or("observation")?;
        match change {
            0 => observation.candidates.swap(0, 1),
            1 => {
                observation.candidates.pop();
            }
            _ => observation.candidates[0].candidate_index = 255,
        }
        assert!(changed.validate().is_err());
    }
    for field in ["admission_mode", "runtime_observation"] {
        let mut changed = serde_json::to_value(manifest(1)?)?;
        changed.as_object_mut().ok_or("manifest")?.remove(field);
        assert!(serde_json::from_value::<CandidateManifestV1>(changed).is_err());
    }
    Ok(())
}

#[test]
fn resource_envelope_keeps_explicit_variants_and_operator_profile_ownership() -> TestResult {
    let mut key = reference("policy", false);
    key.as_object_mut().ok_or("key")?.remove("revision_id");
    let mut command = json!({"format":1,"operation_id":"operation","installation_id":"installation",
        "key":key,"expected_revision":null,"definition":{"kind":"admission_profile","profile":profile()}});
    serde_json::from_value::<ResourcePut>(command.clone())?.validate()?;
    let raw = serde_json::to_string(&command)?;
    serde_json::from_str::<ResourcePut>(&raw)?.validate()?;
    for bad in ["0.00000000000000000000001", "4294967296", "-1"] {
        assert!(
            serde_json::from_str::<ResourcePut>(
                &raw.replace("\"member_rpm\":0", &format!("\"member_rpm\":{bad}"))
            )
            .is_err()
        );
    }
    let duplicated = raw.replace(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
    );
    assert!(serde_json::from_str::<ResourcePut>(&duplicated).is_err());
    command["key"]["owner_kind"] = json!("tenant");
    assert!(
        serde_json::from_value::<ResourcePut>(command)?
            .validate()
            .is_err()
    );
    let old: Definition = serde_json::from_value(
        json!({"kind":"policy","document":{},"name":"old","dependencies":[]}),
    )?;
    assert_eq!(
        serde_json::to_value(old)?,
        json!({"kind":"policy","document":{},"name":"old","dependencies":[]})
    );
    Ok(())
}

#[test]
fn historical_ordered_binding_is_not_implicitly_upgraded_to_send_authority() -> TestResult {
    use bitrouter_ai::tenancy::ordered::OrderedDispatchBindingV2;
    // This former wire shape is deliberately not a new send-capable binding.
    // Archive consumers must retain their historical codec and original bytes.
    let historical = r#"{"Bitrouter-Tenancy-Version":"2","Bitrouter-Installation-Id":"installation","Bitrouter-Instance-Id":"instance","Bitrouter-Capability-Revision":"capability","Bitrouter-Execution-Revision":"execution","Bitrouter-Correlation-Id":"correlation","Bitrouter-Receipt-Mode":"strict","selection":{"schema_version":1,"route":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"route","resource_id":"route","revision_id":"revision"},"candidate_indices":[0]}}"#;
    let rejected = serde_json::from_str::<OrderedDispatchBindingV2>(historical);
    assert!(
        rejected.is_err_and(|error| error.to_string().contains("missing field `admission_mode`"))
    );
    let mut explicit: Value = serde_json::from_str(historical)?;
    explicit["selection"]["admission_mode"] = json!("legacy");
    serde_json::from_value::<OrderedDispatchBindingV2>(explicit)?.validate()?;
    Ok(())
}

#[test]
fn actual_profile_and_full_command_serializers_preserve_aggregate_byte_limits() -> TestResult {
    let maximum: AdmissionProfileV1 = serde_json::from_value(json!({"schema_version":1,
        "rate":{"rate_group_id":"g".repeat(128),"member_rpm":u32::MAX},
        "health":{"rules":"availability_v1","failure_threshold":u32::MAX,
            "transient_cooldown_seconds":u64::MAX,"persistent_cooldown_seconds":u64::MAX,
            "probe_lease_seconds":u64::MAX},"fallback":"ordered_terminal_rejections_v1"}))?;
    maximum.validate()?;
    assert_eq!(serde_json::to_vec(&maximum)?.len(), 456);
    let attachments: Vec<_> = (0..768).map(|index| json!({
        "inbound_protocol":match index / 256 {0 => "chat_completions",1 => "messages",_ => "responses"},
        "candidate_index":index % 256,"credential":reference("credential",true),"profile":reference("policy",true),
    })).collect();
    let credentials: Vec<_> = (0..256)
        .map(|index| {
            json!({"provider":format!("p{index:03}{}","p".repeat(124)),
        "account":"a".repeat(128),"credential":reference("credential",true)})
        })
        .collect();
    let mut key = reference("route", true);
    key.as_object_mut().ok_or("key")?.remove("revision_id");
    // Cardinality ceilings do not promise every maximum-width routing source
    // will fit. This is a DTO byte-bound case, not an executable route fixture.
    let mut command: ResourcePut = serde_json::from_value(json!({"format":1,
        "operation_id":"o".repeat(128),"installation_id":"i".repeat(128),"key":key,
        "expected_revision":"v".repeat(128),"definition":{"kind":"profiled_route",
            "routing":{},"selector":"s".repeat(512),"policy":reference("policy",true),
            "credentials":credentials,"attachments":attachments}}))?;
    command.validate()?;
    assert_eq!(serde_json::to_vec(&command)?.len(), 1017903);
    let Definition::ProfiledRoute { routing, .. } = &mut command.definition else {
        return Err("profiled command".into());
    };
    *routing = json!({"padding":"x".repeat(30661)});
    assert_eq!(serde_json::to_vec(&command)?.len(), 1024 * 1024);
    command.validate()?;
    let Definition::ProfiledRoute { routing, .. } = &mut command.definition else {
        return Err("profiled command".into());
    };
    *routing = json!({"padding":"x".repeat(30662)});
    assert_eq!(serde_json::to_vec(&command)?.len(), 1024 * 1024 + 1);
    assert!(command.validate().is_err());
    Ok(())
}
