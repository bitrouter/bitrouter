//! Receipt-bound capacity and actual-arm tests for the single current object.
use super::{TestResult, operations};
use crate::tenancy::{
    execution::{Dispatch, ExecutionReceipt},
    provider_technical_facts::ProviderTechnicalFacts,
    validation::Validate,
};
use serde_json::json;

fn receipt(count: usize, ordered: bool) -> Result<ExecutionReceipt, Box<dyn std::error::Error>> {
    let mut receipt = operations::receipt()?;
    let template = receipt.attempts[0].clone();
    receipt.attempts.clear();
    for index in 0..count {
        let mut attempt = template.clone();
        attempt.attempt_id = format!("actual-{index}").try_into()?;
        attempt.candidate_index = ordered.then(|| u16::try_from(255 - index)).transpose()?;
        receipt.attempts.push(attempt);
    }
    if ordered {
        receipt.dispatch = Dispatch::Ordered {
            accepted_selection: serde_json::from_value(json!({
                "admission_mode":"unprofiled",
                "route":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"route",
                    "resource_id":"route","revision_id":"route-revision"},
                "inbound_protocol":"messages",
                "candidate_indices":receipt.attempts.iter().map(|a|a.candidate_index).collect::<Vec<_>>()
            }))?,
            attempt_limit: u8::try_from(count)?,
        };
    }
    Ok(receipt)
}

fn facts(
    receipt: &ExecutionReceipt,
    separate: bool,
) -> Result<ProviderTechnicalFacts, Box<dyn std::error::Error>> {
    let ids: Vec<_> = receipt
        .attempts
        .iter()
        .map(|a| a.attempt_id.clone())
        .collect();
    let mut data = operations::value();
    let mut ops = Vec::new();
    let mut hops = Vec::new();
    let mut observers = Vec::new();
    for group in ids.chunks(if separate { 1 } else { ids.len() }) {
        let closing = group.last().ok_or("arm group")?;
        ops.push(json!({"attempt_ids":group,"selected_attempt_id":null,"streamed":false,
            "upstream_duration_ms":7,"first_semantic":null,"generation_duration_ms":null,"partial":false}));
        hops.push(
            json!({"profile":"core_json_hop_callbacks_v1","attempt_ids":group,
            "closing_attempt_id":closing,"duration_ms":77,"end":"failed","partial":false}),
        );
        observers.push(
            json!({"profile":"core_json_hop_observer_floor_v1","attempt_ids":group,
            "closing_attempt_id":closing,"duration_ms":0,"end":"failed"}),
        );
    }
    data["operations"] = json!(ops);
    data["hop_observations"] = json!(hops);
    data["observer_hops"] = json!(observers);
    data["pipeline_scope"] = json!({"profile":"core_pipeline_finalized_v1",
        "context_request_id":"own-context","request_duration_ms":321,"streamed":true,
        "stream":{"profile":"core_pipeline_merged_stream_v1","source_attempt_id":ids.first(),
            "terminal_attempt_id":ids.last(),"upstream_duration_ms":250,
            "first_semantic":{"ttft_ms":123,"kind":"reasoning"},"generation_duration_ms":127,"partial":false}});
    Ok(serde_json::from_value(data)?)
}

#[test]
fn strict_and_ordered_receipts_bound_all_categories_and_preserve_measurements() -> TestResult {
    for ordered in [false, true] {
        for count in [1, 8, 9, 64, 65] {
            let receipt = receipt(count, ordered)?;
            let receipt_bytes = serde_json::to_vec(&receipt)?;
            for separate in [false, true] {
                let data = facts(&receipt, separate)?;
                let before = serde_json::to_vec(&data)?;
                assert_eq!(data.validate().is_ok(), count <= 64, "wire bound {count}");
                assert_eq!(
                    data.validate_against(&receipt).is_ok(),
                    count <= if ordered { 64 } else { 8 },
                    "ordered={ordered} count={count} separate={separate}"
                );
                assert_eq!(serde_json::to_vec(&data)?, before);
                assert_eq!(serde_json::to_vec(&receipt)?, receipt_bytes);
            }
        }
    }
    Ok(())
}

#[test]
fn ordered_binding_uses_actual_arms_and_both_admission_limits() -> TestResult {
    let mut original = receipt(3, true)?;
    original.attempts.remove(1); // Accepted ordinal 254 was never armed.
    let data = facts(&original, false)?;
    data.validate_against(&original)?;
    let before = serde_json::to_vec(&original)?;
    for category in ["operations", "hop_observations", "pipeline_scope"] {
        let mut forged = data.clone();
        let unarmed = "actual-1".try_into()?;
        match category {
            "operations" => forged.operations[0].attempt_ids.insert(1, unarmed),
            "hop_observations" => {
                forged.hop_observations[0]
                    .attempt_ids
                    .insert(1, unarmed.clone());
                forged.observer_hops[0].attempt_ids.insert(1, unarmed);
            }
            "pipeline_scope" => {
                forged
                    .pipeline_scope
                    .as_mut()
                    .ok_or("scope")?
                    .stream
                    .as_mut()
                    .ok_or("stream")?
                    .source_attempt_id = Some(unarmed);
            }
            _ => return Err("unknown category".into()),
        }
        forged.validate()?;
        assert!(forged.validate_against(&original).is_err(), "{category}");
    }
    for limit in [1, 2, 65] {
        let mut invalid = original.clone();
        let Dispatch::Ordered { attempt_limit, .. } = &mut invalid.dispatch else {
            return Err("ordered fixture".into());
        };
        *attempt_limit = limit;
        assert!(data.validate_against(&invalid).is_err(), "limit={limit}");
    }
    let mut invalid = original.clone();
    let Dispatch::Ordered {
        accepted_selection,
        attempt_limit,
    } = &mut invalid.dispatch
    else {
        return Err("ordered fixture".into());
    };
    *attempt_limit = 64;
    accepted_selection.candidate_indices.truncate(1);
    assert!(
        data.validate_against(&invalid).is_err(),
        "actual count exceeds selection"
    );
    assert_eq!(serde_json::to_vec(&original)?, before);
    Ok(())
}

#[test]
fn removed_receipt_format_is_rejected_by_the_closed_current_wire() -> TestResult {
    for format in [1, 2, 3] {
        let mut value = operations::value();
        value["receipt_format"] = json!(format);
        assert!(serde_json::from_value::<ProviderTechnicalFacts>(value).is_err());
    }
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn current_schema_has_only_the_shared_transfer_bound() -> TestResult {
    let schema = serde_json::to_value(schemars::schema_for!(ProviderTechnicalFacts))?;
    assert_eq!(schema["additionalProperties"], false);
    assert!(schema["properties"].get("receipt_format").is_none());
    assert!(
        !schema["required"]
            .as_array()
            .ok_or("required")?
            .contains(&json!("receipt_format"))
    );
    assert!(schema.get("allOf").is_none());
    for category in ["operations", "hop_observations", "observer_hops"] {
        assert_eq!(schema["properties"][category]["maxItems"], 64);
        assert_eq!(
            schema["properties"][category]["items"]["properties"]["attempt_ids"]["maxItems"],
            64
        );
    }
    Ok(())
}
