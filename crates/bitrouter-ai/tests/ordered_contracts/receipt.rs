use super::*;
use bitrouter_ai::tenancy::identity::AuthorityRevision;

fn provider_usage(value: u64) -> Usage {
    Usage {
        input_tokens: Some(value),
        output_tokens: Some(value),
        cache_read_tokens: Some(value),
        cache_write_tokens: Some(value),
        reasoning_tokens: Some(value),
        provenance: Provenance::Provider,
        complete: true,
    }
}

#[test]
fn actual_attempts_are_a_unique_ordered_subsequence() -> TestResult {
    for count in [1, 8, 9, 64] {
        let receipt = receipt(count)?;
        receipt.validate()?;
        assert_eq!(legacy_receipt(&receipt)?.validate().is_ok(), count <= 8);
    }
    let mut value = receipt(3)?;
    value.accepted_selection =
        selected(vec![255, 30, 1, 128, 0])?.accept(GenerationProtocol::Messages)?;
    for (attempt, index) in value.attempts.iter_mut().zip([255, 1, 0]) {
        attempt.candidate_index = index;
    }
    value.validate()?;
    value.attempts.swap(0, 1);
    assert!(value.validate().is_err());
    value.attempts.swap(0, 1);
    value.attempts[1].candidate_index = 255;
    assert!(value.validate().is_err());
    value.attempts[1].candidate_index = 2;
    assert!(value.validate().is_err());
    value.attempts[1].candidate_index = 1;
    value.attempts[1].attempt_id = value.attempts[0].attempt_id.clone();
    assert!(value.validate().is_err());
    let mut value = receipt(1)?;
    value.attempts.push(value.attempts[0].clone());
    assert!(value.validate().is_err());
    let mut value = receipt(64)?;
    value.attempt_limit = 63;
    assert!(value.validate().is_err());
    for limit in [0, 65, 255] {
        value.attempt_limit = limit;
        assert!(value.validate().is_err());
    }
    let mut value = serde_json::to_value(receipt(64)?)?;
    value["accepted_selection"]["candidate_indices"] = json!((0..65).collect::<Vec<_>>());
    assert!(!valid::<ExecutionReceiptV2>(value));
    Ok(())
}

#[test]
fn strict_receipt_wire_status_and_schema() -> TestResult {
    let mut receipt = receipt(1)?;
    receipt.attempts[0].observed_http_status = None;
    strict_object(&receipt)?;
    strict_object(&receipt.attempts[0])?;
    for status in [200, 299, 400, 429, 500, 599] {
        receipt.attempts[0].observed_http_status = Some(status);
        receipt.validate()?;
    }
    for status in [0, 199, 600, u16::MAX] {
        receipt.attempts[0].observed_http_status = Some(status);
        assert!(receipt.validate().is_err());
    }
    receipt.attempts[0].observed_http_status = Some(503);
    let raw = serde_json::to_string(&receipt)?;
    serde_json::from_str::<ExecutionReceiptV2>(&raw.replace(
        "\"observed_http_status\":503",
        "\"observed_http_status\":503e0",
    ))?
    .validate()?;
    for invalid in ["-1", "503.00000000000001", "65536", "\"503\""] {
        assert!(
            serde_json::from_str::<ExecutionReceiptV2>(&raw.replace(
                "\"observed_http_status\":503",
                &format!("\"observed_http_status\":{invalid}")
            ))
            .is_err()
        );
    }
    for version in [0, 1, 3, 255] {
        receipt.schema_version = version;
        assert!(receipt.validate().is_err());
    }
    receipt.schema_version = 2;
    assert!(serde_json::from_value::<ExecutionReceipt>(serde_json::to_value(&receipt)?).is_err());
    assert!(
        serde_json::from_value::<ExecutionReceiptV2>(serde_json::to_value(legacy_receipt(
            &receipt
        )?)?)
        .is_err()
    );
    Ok(())
}

#[test]
fn all_failed_and_final_success_include_every_attempt_in_totals() -> TestResult {
    for count in [1, 8, 9, 64] {
        let mut value = receipt(count)?;
        for attempt in &mut value.attempts {
            attempt.usage = provider_usage(1);
        }
        value.usage = provider_usage(u64::from(count));
        value.validate()?;
        value.outcome = Outcome::Completed;
        assert!(value.validate().is_err());
        let last = value.attempts.last_mut().ok_or("attempt")?;
        last.outcome = Outcome::Completed;
        last.observed_http_status = Some(200);
        assert!(value.validate().is_err());
        value.attempts.last_mut().ok_or("attempt")?.target =
            Some(serde_json::from_value(target())?);
        value.validate()?;
        value.usage.input_tokens = Some(u64::from(count) - 1);
        assert!(value.validate().is_err());
    }
    Ok(())
}

#[test]
fn unknown_partial_estimated_and_overflow_usage_remain_conservative() -> TestResult {
    let mut value = receipt(2)?;
    for state in [
        ReceiptState::Committed,
        ReceiptState::Incomplete,
        ReceiptState::Unknown,
    ] {
        value.state = state;
        value.validate()?;
    }
    value.attempts[0].usage = provider_usage(1);
    value.attempts[1].outcome = Outcome::Unknown;
    value.attempts[1].observed_http_status = None;
    value.outcome = Outcome::Unknown;
    value.validate()?;
    value.usage.input_tokens = Some(0);
    assert!(value.validate().is_err());
    value.usage = provider_usage(1);
    assert!(value.validate().is_err());
    value.attempts[1].usage = provider_usage(1);
    value.attempts[1].usage.provenance = Provenance::Estimated;
    value.attempts[1].usage.complete = false;
    value.usage = provider_usage(2);
    assert!(value.validate().is_err());
    value.usage.provenance = Provenance::Estimated;
    value.usage.complete = false;
    value.validate()?;
    value.attempts[1].usage.output_tokens = None;
    assert!(value.validate().is_err());
    value.usage.output_tokens = None;
    value.validate()?;
    for attempt in &mut value.attempts {
        attempt.usage = provider_usage(AuthorityRevision::MAX);
    }
    value.usage = provider_usage(AuthorityRevision::MAX);
    assert!(value.validate().is_err());
    value.usage = Usage {
        provenance: Provenance::Provider,
        ..unknown_usage()
    };
    value.validate()?;
    // Summed counters exceed the exact JSON integer ceiling; preserve absence.
    let mut overflow = receipt(64)?;
    for attempt in &mut overflow.attempts {
        attempt.usage = provider_usage(AuthorityRevision::MAX);
    }
    overflow.usage = Usage {
        provenance: Provenance::Provider,
        ..unknown_usage()
    };
    overflow.validate()?;
    overflow.usage.input_tokens = Some(0);
    assert!(overflow.validate().is_err());
    value.attempts.clear();
    value.usage = unknown_usage();
    value.validate()?;
    value.usage = Usage {
        provenance: Provenance::Provider,
        ..unknown_usage()
    };
    assert!(value.validate().is_err());
    Ok(())
}

#[test]
fn maximum_receipt_fits_and_byte_limit_is_exact() -> TestResult {
    let mut receipt = receipt(64)?;
    for attempt in &mut receipt.attempts {
        attempt.target = Some(serde_json::from_value(target())?);
        attempt.usage = provider_usage(AuthorityRevision::MAX / 64);
        attempt.native_usage = serde_json::from_value(json!([
            {"name":"input_tokens","value":AuthorityRevision::MAX},
            {"name":"output_tokens","value":AuthorityRevision::MAX},
            {"name":"cache_read_tokens","value":AuthorityRevision::MAX},
            {"name":"cache_write_tokens","value":AuthorityRevision::MAX},
            {"name":"reasoning_tokens","value":AuthorityRevision::MAX}]))?;
    }
    receipt.usage = provider_usage((AuthorityRevision::MAX / 64) * 64);
    let mut value = serde_json::to_value(&receipt)?;
    selection::maximize(&mut value);
    receipt = serde_json::from_value(value)?;
    for (index, attempt) in receipt.attempts.iter_mut().enumerate() {
        attempt.attempt_id = format!("{:0>128}", index).try_into()?;
    }
    receipt.validate()?;
    let length = serde_json::to_vec(&receipt)?.len();
    assert!(length > 128 * 1024 && length < 1024 * 1024, "{length}");
    let mut bound = receipt.clone();
    bound.accepted_selection.generation_defaults_digest = Some("a".repeat(64).try_into()?);
    bound.validate()?;
    assert_eq!(serde_json::to_vec(&bound)?.len(), length + 96);
    // Timestamp fractions are valid arbitrary precision in the frozen primitive.
    // They provide a semantic value to exercise the exact document byte fence.
    receipt.recorded_at = "2026-11-02T00:00:00.0Z".try_into()?;
    let length = serde_json::to_vec(&receipt)?.len();
    let padding = 1024 * 1024 - length;
    receipt.recorded_at = format!("2026-11-02T00:00:00.{}Z", "0".repeat(padding + 1)).try_into()?;
    assert_eq!(serde_json::to_vec(&receipt)?.len(), 1024 * 1024);
    receipt.validate()?;
    receipt.recorded_at = format!("2026-11-02T00:00:00.{}Z", "0".repeat(padding + 2)).try_into()?;
    assert_eq!(serde_json::to_vec(&receipt)?.len(), 1024 * 1024 + 1);
    assert!(receipt.validate().is_err());
    Ok(())
}

#[test]
fn v1_golden_bytes_and_validation_errors_remain_frozen() -> TestResult {
    const GOLDEN: &str = r#"{"installation_id":"installation","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","correlation_id":"correlation","execution_revision":"revision","state":"committed","outcome":"failed","attempts":[{"attempt_id":"attempt-0","target":null,"outcome":"failed","usage":{"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"provenance":"unknown","complete":false},"native_usage":[]}],"usage":{"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"provenance":"unknown","complete":false},"recorded_at":"2026-11-02T00:00:00Z"}"#;
    let legacy = legacy_receipt(&receipt(1)?)?;
    legacy.validate()?;
    assert_eq!(serde_json::to_string(&legacy)?, GOLDEN);
    assert_eq!(
        serde_json::to_string(&serde_json::from_str::<ExecutionReceipt>(GOLDEN)?)?,
        GOLDEN
    );
    let cases = [
        (
            "/outcome",
            json!("completed"),
            "completed execution attempt",
        ),
        (
            "/attempts/0/outcome",
            json!("completed"),
            "completed attempt target",
        ),
        (
            "/attempts/0/usage/input_tokens",
            json!(0),
            "unknown usage evidence",
        ),
        (
            "/usage/provenance",
            json!("provider"),
            "aggregate provenance",
        ),
    ];
    for (pointer, change, error) in cases {
        let mut value = serde_json::to_value(&legacy)?;
        *value.pointer_mut(pointer).ok_or("pointer")? = change;
        let invalid: ExecutionReceipt = serde_json::from_value(value)?;
        assert_eq!(
            invalid.validate().err().ok_or("error")?.to_string(),
            format!("invalid tenancy contract: {error}")
        );
    }
    for count in [9, 64] {
        assert_eq!(
            legacy_receipt(&receipt(count)?)?
                .validate()
                .err()
                .ok_or("error")?
                .to_string(),
            "invalid tenancy contract: attempt count"
        );
    }
    Ok(())
}

#[test]
fn terminal_attempts_never_allow_a_following_attempt() -> TestResult {
    for terminal in [Outcome::Unknown, Outcome::Cancelled, Outcome::Completed] {
        for following in [
            Outcome::Failed,
            Outcome::Unknown,
            Outcome::Cancelled,
            Outcome::Completed,
        ] {
            for state in [
                ReceiptState::Committed,
                ReceiptState::Incomplete,
                ReceiptState::Unknown,
            ] {
                let mut value = receipt(2)?;
                for attempt in &mut value.attempts {
                    attempt.target = Some(serde_json::from_value(target())?);
                    attempt.observed_http_status = Some(200);
                }
                value.attempts[0].outcome = terminal;
                value.attempts[1].outcome = following;
                value.state = state;
                value.outcome = following;
                // This ordering restriction belongs only to ordered v2 execution.
                legacy_receipt(&value)?.validate()?;
                assert!(
                    value.validate().is_err(),
                    "{terminal:?} -> {following:?}, {state:?}"
                );
                assert!(evidence(&value).validate_against(&value).is_err());
            }
        }
    }
    Ok(())
}

#[test]
fn an_unknown_actual_attempt_requires_unknown_receipt_classification() -> TestResult {
    for state in [
        ReceiptState::Committed,
        ReceiptState::Incomplete,
        ReceiptState::Unknown,
    ] {
        for outcome in [
            Outcome::Failed,
            Outcome::Unknown,
            Outcome::Cancelled,
            Outcome::Completed,
        ] {
            let mut value = receipt(2)?;
            value.attempts[1].outcome = Outcome::Unknown;
            value.attempts[1].observed_http_status = Some(200);
            value.state = state;
            value.outcome = outcome;
            let expected = state == ReceiptState::Unknown && outcome == Outcome::Unknown;
            assert_eq!(value.validate().is_ok(), expected, "{state:?}, {outcome:?}");
            assert_eq!(evidence(&value).validate_against(&value).is_ok(), expected);
        }
    }
    Ok(())
}

#[test]
fn missing_usage_can_leave_a_successful_receipt_incomplete() -> TestResult {
    let mut value = receipt(2)?;
    value.attempts[1].target = Some(serde_json::from_value(target())?);
    value.attempts[1].outcome = Outcome::Completed;
    value.attempts[1].observed_http_status = Some(200);
    value.outcome = Outcome::Completed;
    value.state = ReceiptState::Incomplete;
    assert!(!value.usage.complete);
    assert!(value.usage.input_tokens.is_none());
    value.validate()?;
    evidence(&value).validate_against(&value)?;
    // Definitive failure is the only prior outcome that permits another send.
    for outcome in [Outcome::Failed, Outcome::Cancelled] {
        value.attempts[1].outcome = outcome;
        value.outcome = outcome;
        value.validate()?;
    }
    Ok(())
}

#[test]
fn observed_http_errors_cannot_describe_completed_attempts() -> TestResult {
    let mut value = receipt(1)?;
    value.attempts[0].target = Some(serde_json::from_value(target())?);
    for outcome in [
        Outcome::Completed,
        Outcome::Failed,
        Outcome::Unknown,
        Outcome::Cancelled,
    ] {
        value.outcome = outcome;
        value.state = if outcome == Outcome::Unknown {
            ReceiptState::Unknown
        } else {
            ReceiptState::Incomplete
        };
        value.attempts[0].outcome = outcome;
        for status in [
            None,
            Some(200),
            Some(299),
            Some(300),
            Some(400),
            Some(429),
            Some(503),
            Some(599),
        ] {
            value.attempts[0].observed_http_status = status;
            let expected =
                outcome != Outcome::Completed || status.is_none_or(|status| status < 300);
            assert_eq!(
                value.attempts[0].validate().is_ok(),
                expected,
                "{outcome:?}, {status:?}"
            );
            assert_eq!(
                value.validate().is_ok(),
                expected,
                "{outcome:?}, {status:?}"
            );
            assert_eq!(evidence(&value).validate_against(&value).is_ok(), expected);
        }
    }
    Ok(())
}

#[test]
fn receipt_route_owner_must_match_tenant_but_operator_pools_remain_eligible() -> TestResult {
    use bitrouter_ai::tenancy::authority::OwnerKind;
    let mut value = receipt(1)?;
    value.validate()?;
    value.accepted_selection.route.owner_id = "foreign-tenant".try_into()?;
    assert!(value.validate().is_err());
    assert!(evidence(&value).validate_against(&value).is_err());
    value.accepted_selection.route.owner_kind = OwnerKind::OperatorPool;
    value.validate()?;
    evidence(&value).validate_against(&value)?;
    value.accepted_selection.route.owner_kind = OwnerKind::Tenant;
    value.tenant_id = "foreign-tenant".try_into()?;
    value.validate()?;
    Ok(())
}
