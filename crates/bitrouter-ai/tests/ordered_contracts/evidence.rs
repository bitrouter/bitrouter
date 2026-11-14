use super::*;
use bitrouter_ai::tenancy::identity::AuthorityRevision;

#[path = "evidence/native.rs"]
mod native;

fn dimensions() -> [Dimension; 13] {
    [
        Dimension::InputTokens,
        Dimension::OutputTokens,
        Dimension::TotalTokens,
        Dimension::CacheReadTokens,
        Dimension::CacheWriteTokens,
        Dimension::ReasoningTokens,
        Dimension::WebSearchRequests,
        Dimension::InputAudioTokens,
        Dimension::OutputAudioTokens,
        Dimension::InputImageTokens,
        Dimension::OutputImageTokens,
        Dimension::CacheWriteFiveMinuteTokens,
        Dimension::CacheWriteOneHourTokens,
    ]
}

#[test]
fn budgets_follow_accepted_count_and_legacy_stays_at_eight() -> TestResult {
    for (count, blocks) in [(1, 1), (8, 1), (9, 2), (64, 8)] {
        let budget = EvidenceBudgetV2::for_attempts(count)?;
        assert_eq!(budget.max_bytes, 256 * 1024 * blocks);
        assert_eq!(budget.max_frame_bytes, 192 * 1024 * blocks);
        assert_eq!(budget.reserved_bytes, 64 * 1024 * blocks);
        assert_eq!(budget.max_frames, 512 * blocks);
        let mut receipt = receipt(count as u16)?;
        let evidence = evidence(&receipt);
        evidence.validate_against(&receipt)?;
        assert_eq!(legacy_evidence(&evidence)?.validate().is_ok(), count <= 8);
        // Local refusals can leave no attempts; the accepted reservation survives.
        receipt.attempts.clear();
        evidence::evidence_for_empty(&receipt)?;
    }
    for count in [0, 65, usize::MAX] {
        assert!(EvidenceBudgetV2::for_attempts(count).is_err());
    }
    Ok(())
}
fn evidence_for_empty(receipt: &ExecutionReceiptV2) -> TestResult {
    evidence(receipt).validate_against(receipt)?;
    Ok(())
}

#[test]
fn strict_evidence_fields_schema_and_no_auxiliary_linkage() -> TestResult {
    let receipt = receipt(1)?;
    let mut evidence = evidence(&receipt);
    evidence.frames.push(frame(&receipt, 0, 0));
    strict_object(&evidence)?;
    strict_object(&evidence.attempts[0])?;
    strict_object(&evidence.frames[0])?;
    // Quantity validation belongs to its frame, including the invalid marker.
    let baseline = serde_json::to_value(&evidence)?;
    for version in [0, 1, 3, 255] {
        let mut value = baseline.clone();
        value["schema_version"] = json!(version);
        assert!(!valid::<ExecutionUsageEvidenceV2>(value));
    }
    for field in [
        "parent_execution_id",
        "child_execution_ids",
        "auxiliary",
        "children",
    ] {
        let mut value = baseline.clone();
        value[field] = Value::Null;
        assert!(!valid::<ExecutionUsageEvidenceV2>(value));
    }
    for (pointer, replacement) in [
        ("/extractor_version", json!(2)),
        ("/frames/0/sequence", json!(1)),
        ("/frames/0/attempt_id", json!("foreign")),
        ("/frames/0/protocol", json!("decisions")),
        ("/frames/0/protocol", json!("custom")),
        ("/frames/0/source", json!("chat_chunk")),
        ("/frames/0/semantics", json!("delta")),
        (
            "/frames/0/quantities/0/value",
            json!(AuthorityRevision::MAX + 1),
        ),
        ("/frames/0/quantities/0/value", json!(-1)),
        ("/frames/0/quantities/0/value", json!(0.5)),
        ("/frames/0/quantities/0/dimension", json!("unqualified")),
    ] {
        let mut value = baseline.clone();
        *value.pointer_mut(pointer).ok_or("pointer")? = replacement;
        assert!(!valid::<ExecutionUsageEvidenceV2>(value), "{pointer}");
    }
    let mut missing = baseline.clone();
    missing["frames"][0]["quantities"][0]
        .as_object_mut()
        .ok_or("object")?
        .remove("value");
    assert!(!valid::<ExecutionUsageEvidenceV2>(missing));
    let raw = serde_json::to_string(&evidence)?.replace("\"value\":0", "\"value\":0,\"value\":1");
    assert!(serde_json::from_str::<ExecutionUsageEvidenceV2>(&raw).is_err());
    assert!(serde_json::from_value::<ExecutionUsageEvidenceV1>(baseline).is_err());
    assert!(
        serde_json::from_value::<ExecutionUsageEvidenceV2>(serde_json::to_value(legacy_evidence(
            &evidence
        )?)?)
        .is_err()
    );
    Ok(())
}

#[test]
fn observation_absence_invalid_null_and_explicit_zero_round_trip_without_invention() -> TestResult {
    let receipt = receipt(1)?;
    let mut evidence = evidence(&receipt);
    let mut observed = frame(&receipt, 0, 0);
    observed.quantities = vec![
        Quantity {
            dimension: Dimension::OutputTokens,
            value: Some(0),
        },
        Quantity {
            dimension: Dimension::InputTokens,
            value: None,
        },
    ];
    evidence.frames.push(observed);
    assert!(evidence.validate().is_err());
    evidence.frames[0].invalid = true;
    evidence.validate_against(&receipt)?;
    let raw = serde_json::to_string(&evidence)?;
    let decoded: ExecutionUsageEvidenceV2 = serde_json::from_str(&raw)?;
    assert_eq!(decoded.frames[0].quantities, evidence.frames[0].quantities);
    assert_eq!(decoded.frames[0].quantities.len(), 2);
    evidence.frames[0].quantities.clear();
    evidence.frames[0].invalid = false;
    evidence.validate_against(&receipt)?;
    assert_eq!(
        serde_json::to_value(&evidence)?["frames"][0]["quantities"],
        json!([])
    );
    // Translation may change the upstream protocol without changing the ingress binding.
    evidence.frames[0].protocol = Protocol::ChatCompletions;
    evidence.frames[0].source = Source::ChatChunk;
    evidence.validate_against(&receipt)?;
    evidence.frames[0].protocol = Protocol::Responses;
    evidence.frames[0].source = Source::ResponseTerminal;
    evidence.validate_against(&receipt)?;
    Ok(())
}

#[test]
fn evidence_binding_rejects_changed_selection_protocol_and_actual_attempts() -> TestResult {
    let mut receipt = receipt(3)?;
    receipt.attempts[0].target = Some(serde_json::from_value(target())?);
    let base = evidence(&receipt);
    base.validate_against(&receipt)?;
    for (pointer, replacement) in [
        ("/installation_id", json!("other")),
        ("/tenant_id", json!("other")),
        ("/execution_id", json!("other")),
        ("/accepted_selection/inbound_protocol", json!("responses")),
        ("/accepted_selection/route/revision_id", json!("other")),
        ("/accepted_selection/candidate_indices", json!([2, 1, 0])),
        ("/attempts/0/attempt_id", json!("other")),
        ("/attempts/0/outcome", json!("unknown")),
        ("/attempts/0/target/provider_id", json!("other")),
        ("/attempts/0/target/model_id", json!("other")),
        ("/attempts/0/target/account_id", json!("other")),
        ("/attempts/0/target/credential_id", json!("other")),
        ("/attempts/0/target/credential_version", json!("other")),
        ("/attempts/0/target", Value::Null),
    ] {
        let mut value = serde_json::to_value(&base)?;
        *value.pointer_mut(pointer).ok_or("pointer")? = replacement;
        if pointer == "/tenant_id" {
            // Keep this a valid standalone document with a different owner.
            value["accepted_selection"]["route"]["owner_id"] = value["tenant_id"].clone();
        }
        let value: ExecutionUsageEvidenceV2 = serde_json::from_value(value)?;
        value.validate()?;
        assert!(value.validate_against(&receipt).is_err(), "{pointer}");
    }
    let mut value = base.clone();
    value.attempts.pop();
    assert!(value.validate_against(&receipt).is_err());
    value = base.clone();
    value.attempts.swap(0, 1);
    assert!(value.validate_against(&receipt).is_err());
    value = base.clone();
    value.attempts[1].attempt_id = value.attempts[0].attempt_id.clone();
    assert!(value.validate().is_err());
    value = base;
    value.attempts.push(value.attempts[0].clone());
    assert!(value.validate().is_err());
    Ok(())
}

#[test]
fn unavailable_capture_and_truncation_are_explicit_not_empty_success() -> TestResult {
    let receipt = receipt(1)?;
    let mut value = evidence(&receipt);
    for capture in [Capture::Unavailable, Capture::NotCaptured] {
        value.capture = capture;
        assert!(value.validate().is_err());
        value.attempts.clear();
        value.validate_against(&receipt)?;
        value.frames.push(frame(&receipt, 0, 0));
        assert!(value.validate().is_err());
        value.frames.clear();
        value = evidence(&receipt);
    }
    value.truncated = true;
    assert!(value.validate().is_err());
    value.dropped_frames = 1;
    value.validate_against(&receipt)?;
    value.dropped_frames = AuthorityRevision::MAX;
    value.validate_against(&receipt)?;
    value.dropped_frames = AuthorityRevision::MAX + 1;
    assert!(value.validate().is_err());
    value.dropped_frames = 1;
    value.truncated = false;
    assert!(value.validate().is_err());
    value = evidence(&receipt);
    value.attempts[0].outcome = Outcome::Unknown;
    value.attempts[0].terminal_observed = true;
    assert!(value.validate().is_err());
    Ok(())
}

#[test]
fn frame_count_per_attempt_quantity_and_byte_limits_are_independent() -> TestResult {
    let receipt = receipt(64)?;
    let mut evidence = evidence(&receipt);
    evidence.frames = (0..4096)
        .map(|index| frame(&receipt, index, index % 64))
        .collect();
    evidence.validate_against(&receipt)?;
    evidence.frames.push(frame(&receipt, 4096, 0));
    assert!(evidence.validate().is_err());
    for count in [128, 129] {
        evidence.frames = (0..count).map(|index| frame(&receipt, index, 0)).collect();
        assert_eq!(evidence.validate().is_ok(), count == 128);
    }
    evidence.frames = vec![frame(&receipt, 0, 0)];
    evidence.frames[0].quantities = dimensions()
        .into_iter()
        .map(|dimension| Quantity {
            dimension,
            value: Some(AuthorityRevision::MAX),
        })
        .collect();
    evidence.validate()?;
    let quantity = evidence.frames[0].quantities[0].clone();
    evidence.frames[0].quantities.push(quantity.clone());
    assert!(evidence.validate().is_err());
    evidence.frames[0].quantities = vec![quantity; 33];
    assert!(evidence.validate().is_err());
    let receipt = super::receipt(8)?;
    let mut evidence = super::evidence(&receipt);
    evidence.frames = (0..512)
        .map(|index| frame(&receipt, index, index % 8))
        .collect();
    evidence.validate()?;
    evidence.frames.push(frame(&receipt, 512, 0));
    assert!(evidence.validate().is_err());
    evidence.frames.pop();
    for frame in &mut evidence.frames {
        frame.quantities = dimensions()
            .into_iter()
            .map(|dimension| Quantity {
                dimension,
                value: Some(AuthorityRevision::MAX),
            })
            .collect();
    }
    assert!(evidence.validate().is_err());
    assert!(serde_json::to_vec(&evidence.frames)?.len() > 192 * 1024);
    assert!(serde_json::to_vec(&evidence)?.len() > 256 * 1024);
    evidence.truncated = true;
    evidence.dropped_frames = 1;
    assert!(evidence.validate().is_err());
    let mut removed = 0;
    while serde_json::to_vec(&evidence.frames)?.len() > 192 * 1024 {
        evidence.frames.pop().ok_or("frame")?;
        removed += 1;
    }
    evidence.dropped_frames = removed;
    evidence.validate_against(&receipt)?;
    assert_eq!(evidence.attempts.len(), 8);
    Ok(())
}

#[test]
fn maximum_identities_and_dense_quantities_preserve_all_64_attempts() -> TestResult {
    let mut receipt = receipt(64)?;
    for attempt in &mut receipt.attempts {
        attempt.target = Some(serde_json::from_value(target())?);
    }
    let mut value = serde_json::to_value(&receipt)?;
    selection::maximize(&mut value);
    receipt = serde_json::from_value(value)?;
    for (index, attempt) in receipt.attempts.iter_mut().enumerate() {
        attempt.attempt_id = format!("{:0>128}", index).try_into()?;
    }
    receipt.accepted_selection.generation_defaults_digest = Some("a".repeat(64).try_into()?);
    let budget = EvidenceBudgetV2::for_attempts(64)?;
    let mut evidence = evidence(&receipt);
    evidence.frames = (0..4096)
        .map(|index| {
            let mut frame = frame(&receipt, index, index % 64);
            frame.quantities = dimensions()
                .into_iter()
                .map(|dimension| Quantity {
                    dimension,
                    value: Some(AuthorityRevision::MAX),
                })
                .collect();
            frame
        })
        .collect();
    assert!(evidence.validate().is_err());
    // Keep the maximal prefix that fits the frame-byte reservation, not a new total.
    let full_length = serde_json::to_vec(&evidence.frames)?.len();
    let approximate_fit = budget.max_frame_bytes * evidence.frames.len() / full_length;
    evidence.frames.truncate(approximate_fit);
    while serde_json::to_vec(&evidence.frames)?.len() > budget.max_frame_bytes {
        evidence.frames.pop().ok_or("frame")?;
    }
    evidence.truncated = true;
    evidence.dropped_frames = 4096 - evidence.frames.len() as u64;
    evidence.validate_against(&receipt)?;
    let frame_bytes = serde_json::to_vec(&evidence.frames)?.len();
    let total_bytes = serde_json::to_vec(&evidence)?.len();
    assert!(frame_bytes > budget.max_frame_bytes * 99 / 100);
    assert!(total_bytes <= 2 * 1024 * 1024);
    assert!(total_bytes - frame_bytes <= budget.reserved_bytes);
    assert_eq!(evidence.attempts.len(), 64);
    assert!(
        evidence
            .frames
            .iter()
            .all(|frame| frame.quantities.len() == 13)
    );
    assert_eq!(
        legacy_evidence(&evidence)?
            .validate()
            .err()
            .ok_or("error")?
            .to_string(),
        "invalid tenancy contract: collection length"
    );
    Ok(())
}

#[test]
fn v1_evidence_golden_bytes_and_extractor_semantics_remain_frozen() -> TestResult {
    const GOLDEN: &str = r#"{"schema_version":1,"installation_id":"installation","tenant_id":"tenant","execution_id":"execution","capture":"captured","extractor_version":1,"estimation":"not_captured","estimate":null,"parent_execution_id":null,"child_execution_ids":[],"attempts":[{"attempt_id":"attempt-0","target":null,"outcome":"failed","terminal_observed":false}],"frames":[],"truncated":false,"dropped_frames":0}"#;
    let receipt = receipt(1)?;
    let mut evidence = evidence(&receipt);
    let legacy = legacy_evidence(&evidence)?;
    legacy.validate()?;
    assert_eq!(serde_json::to_string(&legacy)?, GOLDEN);
    assert_eq!(
        serde_json::to_string(&serde_json::from_str::<ExecutionUsageEvidenceV1>(GOLDEN)?)?,
        GOLDEN
    );
    evidence.frames.push(frame(&receipt, 0, 0));
    evidence.frames[0].protocol = Protocol::Decisions;
    evidence.frames[0].semantics = Semantics::Unknown;
    evidence.frames[0].source = Source::Json;
    legacy_evidence(&evidence)?.validate()?;
    assert!(evidence.validate().is_err());
    evidence.frames[0].semantics = Semantics::Snapshot;
    assert_eq!(
        legacy_evidence(&evidence)?
            .validate()
            .err()
            .ok_or("error")?
            .to_string(),
        "invalid tenancy contract: extractor semantics"
    );
    Ok(())
}

#[test]
fn standalone_evidence_checks_tenant_route_ownership_even_without_capture() -> TestResult {
    use bitrouter_ai::tenancy::authority::OwnerKind;
    for capture in [
        Capture::Captured,
        Capture::Unavailable,
        Capture::NotCaptured,
    ] {
        let mut value = evidence(&receipt(1)?);
        value.capture = capture;
        if capture != Capture::Captured {
            value.attempts.clear();
        }
        value.validate()?;
        value.accepted_selection.route.owner_id = "foreign-tenant".try_into()?;
        assert!(value.validate().is_err(), "{capture:?}");
        value.accepted_selection.route.owner_kind = OwnerKind::OperatorPool;
        value.validate()?;
        value.accepted_selection.route.owner_kind = OwnerKind::Tenant;
        value.tenant_id = "foreign-tenant".try_into()?;
        value.validate()?;
    }
    Ok(())
}
