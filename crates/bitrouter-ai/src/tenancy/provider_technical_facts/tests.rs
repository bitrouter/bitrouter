use super::*;
use serde_json::json;
type TestResult = Result<(), Box<dyn std::error::Error>>;

fn value() -> serde_json::Value {
    json!({"schema_version":1,"receipt_format":1,"installation_id":"store","tenant_id":"tenant",
        "principal_id":"principal","execution_id":"execution","execution_revision":"revision",
        "operations":[{"attempt_ids":["attempt-a","attempt-b"],"selected_attempt_id":"attempt-b",
            "streamed":false,"upstream_duration_ms":0,"first_semantic":null,"generation_duration_ms":null,"partial":false}]})
}
fn receipt() -> Result<ExecutionReceipt, Box<dyn std::error::Error>> {
    let usage = json!({"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"provenance":"unknown","complete":false});
    Ok(serde_json::from_value(
        json!({"installation_id":"store","tenant_id":"tenant","principal_id":"principal",
        "execution_id":"execution","correlation_id":"correlation","execution_revision":"revision","state":"incomplete","outcome":"failed",
        "attempts":[{"attempt_id":"attempt-a","target":null,"outcome":"failed","usage":usage,"native_usage":[]},
            {"attempt_id":"attempt-b","target":null,"outcome":"failed","usage":usage,"native_usage":[]}],
        "usage":usage,"recorded_at":"2026-01-01T00:00:00Z"}),
    )?)
}

#[test]
fn zero_unknown_single_semantic_and_partial_remain_distinct() -> TestResult {
    let zero: ProviderTechnicalFactsV1 = serde_json::from_value(value())?;
    zero.validate()?;
    assert_eq!(zero.operations[0].upstream_duration_ms, Some(0));
    let mut data = value();
    data["operations"][0]["upstream_duration_ms"] = json!(null);
    data["operations"][0]["partial"] = json!(true);
    let unknown: ProviderTechnicalFactsV1 = serde_json::from_value(data)?;
    unknown.validate()?;
    assert_eq!(unknown.operations[0].upstream_duration_ms, None);
    let mut single = zero;
    single.operations[0].streamed = true;
    single.operations[0].first_semantic = Some(FirstSemantic {
        ttft_ms: 1,
        kind: FirstSemanticKind::Tool,
    });
    single.operations[0].generation_duration_ms = Some(0);
    single.operations[0].partial = true;
    single.validate()?;
    assert_ne!(serde_json::to_vec(&single)?, serde_json::to_vec(&unknown)?);
    // Separate rounding is not an additive arithmetic admission condition.
    single.operations[0].upstream_duration_ms = Some(1);
    single.operations[0].generation_duration_ms = Some(1);
    single.validate()?;
    Ok(())
}

#[test]
fn required_null_unknown_duplicates_integer_overflow_and_kind_fail_closed() -> TestResult {
    let raw = serde_json::to_string(&value())?;
    for field in [
        "selected_attempt_id",
        "upstream_duration_ms",
        "first_semantic",
        "generation_duration_ms",
    ] {
        let mut data = value();
        data["operations"][0]
            .as_object_mut()
            .ok_or("operation")?
            .remove(field);
        assert!(
            serde_json::from_value::<ProviderTechnicalFactsV1>(data).is_err(),
            "{field}"
        );
    }
    // The schema's closed objects are maps, never ordered positional arrays.
    assert!(
        serde_json::from_str::<ProviderTechnicalFactsV1>(
            "[1,1,\"store\",\"tenant\",\"principal\",\"execution\",\"revision\",[]]"
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<ProviderOperation>("[[\"attempt\"],null,true,0,null,null,false]")
            .is_err()
    );
    assert!(serde_json::from_str::<FirstSemantic>("[0,\"text\"]").is_err());
    let duplicate = raw.replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1,
    );
    assert_ne!(duplicate, raw);
    assert!(serde_json::from_str::<ProviderTechnicalFactsV1>(&duplicate).is_err());
    for token in ["-1", "0.5", "18446744073709551616", "true", "\"0\""] {
        let changed = raw.replace(
            "\"upstream_duration_ms\":0",
            &format!("\"upstream_duration_ms\":{token}"),
        );
        assert_ne!(changed, raw);
        assert!(serde_json::from_str::<ProviderTechnicalFactsV1>(&changed).is_err());
    }
    let mut data = value();
    data["operations"][0]["future"] = json!(1);
    assert!(serde_json::from_value::<ProviderTechnicalFactsV1>(data).is_err());
    let mut data = value();
    data["operations"][0]["streamed"] = json!(true);
    data["operations"][0]["generation_duration_ms"] = json!(0);
    data["operations"][0]["first_semantic"] = json!({"ttft_ms":0,"kind":"provider_name"});
    assert!(serde_json::from_value::<ProviderTechnicalFactsV1>(data).is_err());
    Ok(())
}

#[test]
fn receipt_binding_rejects_foreign_identity_missing_arm_reorder_and_wrong_format() -> TestResult {
    let facts: ProviderTechnicalFactsV1 = serde_json::from_value(value())?;
    let original = receipt()?;
    let before = serde_json::to_vec(&original)?;
    facts.validate_against(&original)?;
    for field in [
        "installation_id",
        "tenant_id",
        "principal_id",
        "execution_id",
        "execution_revision",
    ] {
        let mut data = serde_json::to_value(&original)?;
        data[field] = json!("foreign");
        let foreign: ExecutionReceipt = serde_json::from_value(data)?;
        assert!(facts.validate_against(&foreign).is_err(), "{field}");
    }
    let mut changed = facts.clone();
    changed.operations[0].attempt_ids.reverse();
    changed.operations[0].selected_attempt_id = Some("attempt-a".try_into()?);
    assert!(changed.validate_against(&original).is_err());
    let mut changed = facts.clone();
    changed.operations[0].attempt_ids[0] = "never-armed".try_into()?;
    assert!(changed.validate_against(&original).is_err());
    let mut changed = facts;
    changed.receipt_format = 2;
    assert!(changed.validate_against(&original).is_err());
    assert_eq!(serde_json::to_vec(&original)?, before);
    Ok(())
}

#[test]
fn operation_and_actual_attempt_budgets_and_selected_binding_are_finite() -> TestResult {
    let facts: ProviderTechnicalFactsV1 = serde_json::from_value(value())?;
    for mutation in [
        "duplicate",
        "selected",
        "missing_pair",
        "json_semantic",
        "schema",
        "format",
        "count",
    ] {
        let mut changed = facts.clone();
        match mutation {
            "duplicate" => changed.operations.push(changed.operations[0].clone()),
            "selected" => changed.operations[0].selected_attempt_id = Some("outside".try_into()?),
            "missing_pair" => changed.operations[0].generation_duration_ms = Some(0),
            "json_semantic" => {
                changed.operations[0].first_semantic = Some(FirstSemantic {
                    ttft_ms: 0,
                    kind: FirstSemanticKind::Text,
                });
                changed.operations[0].generation_duration_ms = Some(0);
            }
            "schema" => changed.schema_version = 2,
            "format" => changed.receipt_format = 3,
            "count" => {
                for index in 0..8 {
                    changed.operations[0]
                        .attempt_ids
                        .push(format!("extra-{index}").try_into()?);
                }
            }
            _ => return Err("unknown test mutation".into()),
        }
        assert!(changed.validate().is_err(), "{mutation}");
    }
    let absent: Option<ProviderTechnicalFactsV1> = serde_json::from_str("null")?;
    absent.validate()?;
    assert!(absent.is_none());
    Ok(())
}

#[test]
fn v2_original_ordinal_receipt_and_auth_retry_group_are_not_relabelled_v1() -> TestResult {
    let mut raw = serde_json::to_value(receipt()?)?;
    raw["schema_version"] = json!(2);
    raw["attempt_limit"] = json!(2);
    raw["accepted_selection"] = json!({"admission_mode":"legacy","route":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"route","resource_id":"route","revision_id":"route-revision"},"inbound_protocol":"messages","candidate_indices":[17,2]});
    for (position, ordinal) in [17, 2].into_iter().enumerate() {
        raw["attempts"][position]["candidate_index"] = json!(ordinal);
        raw["attempts"][position]["observed_http_status"] = json!(503);
    }
    let original: ExecutionReceiptV2 = serde_json::from_value(raw)?;
    original.validate()?;
    let mut facts: ProviderTechnicalFactsV1 = serde_json::from_value(value())?;
    facts.receipt_format = 2;
    facts.validate_against_v2(&original)?;
    let before = serde_json::to_vec(&original)?;
    let mut changed = original.clone();
    changed.attempts.swap(0, 1);
    assert!(facts.validate_against_v2(&changed).is_err());
    assert!(facts.validate_against(&receipt()?).is_err());
    assert_eq!(serde_json::to_vec(&original)?, before);
    Ok(())
}
