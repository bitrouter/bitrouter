use super::*;

pub(super) fn capabilities() -> Result<OrderedCapabilitiesV2, serde_json::Error> {
    serde_json::from_value(
        json!({"contract_version":2,"installation_id":"installation",
        "instance_id":"instance","capability_revision":"capability","authority_revision":1,
        "attempt_limit":64,"revision_bound_dispatch":true,"strict_receipts":true,"dispatch_enabled":true}),
    )
}
pub(super) fn manifest(count: u16) -> Result<CandidateManifestV1, serde_json::Error> {
    serde_json::from_value(json!({"schema_version":1,"installation_id":"installation",
        "tenant_id":"tenant","execution_revision":"revision","authority_revision":1,
        "route":reference("route"),"inbound_protocol":"messages",
        "candidates":(0..count).map(|index| json!({"candidate_index":index,
            "target":target(),"credential":reference("credential")})).collect::<Vec<_>>()}))
}

#[test]
fn selection_count_order_and_ordinal_bounds() -> TestResult {
    for count in [1, 8, 9, 64] {
        let indices: Vec<_> = (0..count).rev().collect();
        let selection = selected(indices.clone())?;
        selection.validate()?;
        assert_eq!(
            serde_json::to_value(&selection)?["candidate_indices"],
            json!(indices)
        );
        assert_eq!(
            selection
                .accept(GenerationProtocol::Responses)?
                .candidate_indices,
            indices
        );
    }
    for indices in [
        vec![],
        (0..65).collect(),
        vec![1, 1],
        vec![256],
        vec![u16::MAX],
    ] {
        assert!(selected(indices)?.validate().is_err());
    }
    selected(vec![255, 0, 128, 42])?.validate()?;
    for kind in [
        "credential",
        "policy",
        "configuration",
        "checkpoint",
        "execution",
    ] {
        let mut value = serde_json::to_value(selected(vec![0])?)?;
        value["route"]["resource_kind"] = json!(kind);
        assert!(!valid::<OrderedCandidateSelectionV1>(value));
    }
    Ok(())
}

#[test]
fn strict_selection_and_protocol_wire_contracts() -> TestResult {
    let selection = selected(vec![255, 0])?;
    strict_object(&selection)?;
    strict_object(&selection.accept(GenerationProtocol::Messages)?)?;
    for version in [0, 2, 255] {
        let mut value = serde_json::to_value(&selection)?;
        value["schema_version"] = json!(version);
        assert!(!valid::<OrderedCandidateSelectionV1>(value));
    }
    let raw = serde_json::to_string(&selection)?;
    serde_json::from_str::<OrderedCandidateSelectionV1>(&raw.replace("[255,0]", "[255e0,0.0]"))?
        .validate()?;
    for invalid in [
        "-1",
        "0.1",
        "65536",
        "255.00000000000000001",
        "\"1\"",
        "null",
    ] {
        assert!(
            serde_json::from_str::<OrderedCandidateSelectionV1>(
                &raw.replace("[255,0]", &format!("[{invalid}]"))
            )
            .is_err()
        );
    }
    for protocol in ["chat_completions", "messages", "responses"] {
        let mut value = serde_json::to_value(selection.accept(GenerationProtocol::Messages)?)?;
        value["inbound_protocol"] = json!(protocol);
        assert!(valid::<AcceptedSelectionV2>(value));
    }
    for protocol in [
        json!("decisions"),
        json!("custom"),
        json!({"custom":"messages"}),
        Value::Null,
    ] {
        let mut value = serde_json::to_value(selection.accept(GenerationProtocol::Messages)?)?;
        value["inbound_protocol"] = protocol;
        assert!(!valid::<AcceptedSelectionV2>(value));
    }
    Ok(())
}

#[test]
fn discovery_binding_requires_entire_chain_and_strict_guarantees() -> TestResult {
    let capabilities = capabilities()?;
    strict_object(&capabilities)?;
    for count in [1, 8, 9, 64] {
        let binding = capabilities.bind(
            "revision".try_into()?,
            "correlation".try_into()?,
            selected((0..count).collect())?,
        )?;
        strict_object(&binding)?;
        assert_eq!(binding.contract_version, "2");
        assert_eq!(
            binding.selection.candidate_indices.len(),
            usize::from(count)
        );
        assert!(
            serde_json::from_value::<bitrouter_ai::tenancy::operation::DispatchBinding>(
                serde_json::to_value(&binding)?
            )
            .is_err()
        );
    }
    assert!(
        serde_json::from_value::<bitrouter_ai::tenancy::operation::TenantCapabilities>(
            serde_json::to_value(&capabilities)?
        )
        .is_err()
    );
    for limit in [1, 8, 9, 64] {
        let mut caps = capabilities.clone();
        caps.attempt_limit = limit;
        caps.bind(
            "revision".try_into()?,
            "correlation".try_into()?,
            selected((0..u16::from(limit)).collect())?,
        )?;
        assert!(
            caps.bind(
                "revision".try_into()?,
                "correlation".try_into()?,
                selected((0..u16::from(limit) + 1).collect())?
            )
            .is_err()
        );
    }
    for limit in [0, 65, 255] {
        let mut caps = capabilities.clone();
        caps.attempt_limit = limit;
        assert!(caps.validate().is_err());
    }
    for key in [
        "revision_bound_dispatch",
        "strict_receipts",
        "dispatch_enabled",
    ] {
        let mut value = serde_json::to_value(&capabilities)?;
        value[key] = json!(false);
        let caps: OrderedCapabilitiesV2 = serde_json::from_value(value)?;
        caps.validate()?;
        assert!(
            caps.bind(
                "revision".try_into()?,
                "correlation".try_into()?,
                selected(vec![0])?
            )
            .is_err()
        );
    }
    let binding = capabilities.bind(
        "revision".try_into()?,
        "correlation".try_into()?,
        selected(vec![0])?,
    )?;
    for (key, invalid) in [
        ("Bitrouter-Tenancy-Version", "1"),
        ("Bitrouter-Receipt-Mode", "best_effort"),
    ] {
        let mut value = serde_json::to_value(&binding)?;
        value[key] = json!(invalid);
        assert!(!valid::<OrderedDispatchBindingV2>(value));
    }
    Ok(())
}

#[test]
fn manifest_keeps_sparse_root_ordinals_and_exact_references() -> TestResult {
    let mut value = manifest(3)?;
    for (candidate, ordinal) in value.candidates.iter_mut().zip([0, 128, 255]) {
        candidate.candidate_index = ordinal;
    }
    strict_object(&value)?;
    strict_object(&value.candidates[0])?;
    assert_eq!(
        serde_json::to_value(&value)?["candidates"][2]["candidate_index"],
        json!(255)
    );
    // Equal opaque targets are retained as distinct root positions by the manifest.
    value.validate()?;
    value.candidates.swap(0, 1);
    assert!(value.validate().is_err());
    value.candidates.swap(0, 1);
    value.candidates[1].candidate_index = 0;
    assert!(value.validate().is_err());
    for count in [0, 1, 256] {
        manifest(count)?.validate()?;
    }
    assert!(manifest(257)?.validate().is_err());
    let base = serde_json::to_value(manifest(1)?)?;
    for (pointer, invalid) in [
        ("/schema_version", json!(2)),
        (
            "/candidates/0/target/credential_version",
            json!("different"),
        ),
        ("/authority_revision", json!(9007199254740992_u64)),
        ("/route/resource_kind", json!("credential")),
        ("/inbound_protocol", json!("decisions")),
        ("/candidates/0/candidate_index", json!(256)),
        ("/candidates/0/credential/resource_kind", json!("route")),
        ("/candidates/0/credential/revision_id", json!("")),
        ("/candidates/0/target/model_id", json!("")),
    ] {
        let mut value = base.clone();
        *value.pointer_mut(pointer).ok_or("pointer")? = invalid;
        assert!(!valid::<CandidateManifestV1>(value), "{pointer}");
    }
    Ok(())
}

#[test]
fn maximum_manifest_and_selection_fit_documented_bounds() -> TestResult {
    let mut value = serde_json::to_value(manifest(256)?)?;
    maximize(&mut value);
    let manifest: CandidateManifestV1 = serde_json::from_value(value)?;
    manifest.validate()?;
    let bytes = serde_json::to_vec(&manifest)?.len();
    assert!(bytes > 512 * 1024 && bytes <= 1024 * 1024, "{bytes}");
    let mut value = serde_json::to_value(selected((192..256).collect())?)?;
    maximize(&mut value);
    let selection: OrderedCandidateSelectionV1 = serde_json::from_value(value)?;
    selection.validate()?;
    // Unpadded base64url size can be proved without adding an encoding dependency.
    let encoded_size = (serde_json::to_vec(&selection)?.len() * 8).div_ceil(6);
    assert!(encoded_size <= 4096);
    Ok(())
}

pub(super) fn maximize(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                if key.ends_with("_id")
                    || matches!(
                        key.as_str(),
                        "credential_version" | "execution_revision" | "capability_revision"
                    )
                {
                    *value = json!(if key == "model_id" {
                        "\u{0001}".repeat(256)
                    } else {
                        "x".repeat(128)
                    });
                } else {
                    maximize(value);
                }
            }
        }
        Value::Array(array) => {
            for value in array {
                maximize(value);
            }
        }
        _ => (),
    }
}

#[test]
fn manifest_route_and_each_credential_are_qualified_by_the_envelope_tenant() -> TestResult {
    use bitrouter_ai::tenancy::authority::OwnerKind;
    let base = manifest(2)?;
    base.validate()?;
    for position in 0..3 {
        let mut value = base.clone();
        let reference = if position == 0 {
            &mut value.route
        } else {
            &mut value.candidates[position - 1].credential
        };
        reference.owner_id = "foreign-tenant".try_into()?;
        assert!(value.validate().is_err(), "foreign reference {position}");
        let reference = if position == 0 {
            &mut value.route
        } else {
            &mut value.candidates[position - 1].credential
        };
        reference.owner_kind = OwnerKind::OperatorPool;
        value.validate()?;
    }
    let mut empty = manifest(0)?;
    empty.route.owner_id = "foreign-tenant".try_into()?;
    assert!(empty.validate().is_err());
    empty.route.owner_kind = OwnerKind::OperatorPool;
    empty.validate()?;
    Ok(())
}
