use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Outcome, Provenance, ReceiptState, Usage},
    execution_v2::{AttemptV2, ExecutionReceiptV2},
    ordered::{
        AcceptedSelectionV2, CandidateManifestV1, GenerationProtocol, OrderedCandidateSelectionV1,
        OrderedCapabilitiesV2, OrderedDispatchBindingV2,
    },
    usage_evidence::{
        AttemptEvidence, Capture, Dimension, Estimation, ExecutionUsageEvidenceV1, Frame, Protocol,
        Quantity, Semantics, Source,
    },
    usage_evidence_v2::{EvidenceBudgetV2, ExecutionUsageEvidenceV2},
    validation::Validate,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[path = "ordered_contracts/evidence.rs"]
mod evidence;
#[path = "ordered_contracts/receipt.rs"]
mod receipt;
#[cfg(feature = "schema")]
#[path = "ordered_contracts/schema.rs"]
mod schema;
#[path = "ordered_contracts/selection.rs"]
mod selection;

fn reference(kind: &str) -> Value {
    json!({"owner_kind":"tenant", "owner_id":"tenant", "resource_kind":kind,
        "resource_id":kind, "revision_id":"revision"})
}
fn selected(indices: Vec<u16>) -> Result<OrderedCandidateSelectionV1, serde_json::Error> {
    serde_json::from_value(
        json!({"schema_version":1, "admission_mode":"legacy", "route":reference("route"), "candidate_indices":indices}),
    )
}
fn unknown_usage() -> Usage {
    Usage {
        input_tokens: None,
        output_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
        provenance: Provenance::Unknown,
        complete: false,
    }
}
fn receipt(count: u16) -> Result<ExecutionReceiptV2, Box<dyn std::error::Error>> {
    let accepted_selection =
        selected((0..count).collect())?.accept(GenerationProtocol::Messages)?;
    let attempts = (0..count)
        .map(|index| {
            Ok(AttemptV2 {
                attempt_id: format!("attempt-{index}").try_into()?,
                candidate_index: index,
                target: None,
                outcome: Outcome::Failed,
                observed_http_status: Some(503),
                usage: unknown_usage(),
                native_usage: vec![],
            })
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    Ok(ExecutionReceiptV2 {
        schema_version: 2,
        installation_id: "installation".try_into()?,
        tenant_id: "tenant".try_into()?,
        principal_id: "principal".try_into()?,
        execution_id: "execution".try_into()?,
        correlation_id: "correlation".try_into()?,
        execution_revision: "revision".try_into()?,
        accepted_selection,
        attempt_limit: 64,
        state: ReceiptState::Committed,
        outcome: Outcome::Failed,
        attempts,
        usage: unknown_usage(),
        recorded_at: "2026-11-02T00:00:00Z".try_into()?,
    })
}
fn evidence(receipt: &ExecutionReceiptV2) -> ExecutionUsageEvidenceV2 {
    ExecutionUsageEvidenceV2 {
        schema_version: 2,
        installation_id: receipt.installation_id.clone(),
        tenant_id: receipt.tenant_id.clone(),
        execution_id: receipt.execution_id.clone(),
        accepted_selection: receipt.accepted_selection.clone(),
        capture: Capture::Captured,
        extractor_version: 1,
        estimation: Estimation::NotCaptured,
        estimate: None,
        attempts: receipt
            .attempts
            .iter()
            .map(|a| AttemptEvidence {
                attempt_id: a.attempt_id.clone(),
                target: a.target.clone(),
                outcome: a.outcome,
                terminal_observed: false,
            })
            .collect(),
        frames: vec![],
        truncated: false,
        dropped_frames: 0,
    }
}
fn frame(receipt: &ExecutionReceiptV2, sequence: usize, attempt: usize) -> Frame {
    Frame {
        sequence: sequence as u64,
        attempt_id: receipt.attempts[attempt].attempt_id.clone(),
        protocol: Protocol::Messages,
        source: Source::MessageDelta,
        semantics: Semantics::Snapshot,
        quantities: vec![Quantity {
            dimension: Dimension::OutputTokens,
            value: Some(0),
        }],
        unsupported: false,
        invalid: false,
    }
}
fn target() -> Value {
    json!({"provider_id":"provider", "model_id":"model", "account_id":"opaque-account",
        "credential_id":"opaque-credential", "credential_version":"revision"})
}
fn valid<T: DeserializeOwned + Validate>(value: Value) -> bool {
    serde_json::from_value::<T>(value).is_ok_and(|value| value.validate().is_ok())
}
fn strict_object<T: Serialize + DeserializeOwned>(value: &T) -> TestResult {
    let value = serde_json::to_value(value)?;
    let object = value.as_object().ok_or("object")?;
    assert!(serde_json::from_value::<T>(value.clone()).is_ok());
    for key in object.keys() {
        let mut missing = value.clone();
        missing.as_object_mut().ok_or("object")?.remove(key);
        assert!(
            serde_json::from_value::<T>(missing).is_err(),
            "missing {key}"
        );
        if !object[key].is_null() {
            let mut null = value.clone();
            null[key] = Value::Null;
            assert!(serde_json::from_value::<T>(null).is_err(), "null {key}");
        }
        let raw = serde_json::to_string(&value)?;
        let duplicated = format!(
            "{{{}:{},{}",
            serde_json::to_string(key)?,
            object[key],
            &raw[1..]
        );
        assert!(
            serde_json::from_str::<T>(&duplicated).is_err(),
            "duplicate {key}"
        );
    }
    let mut unknown = value;
    unknown["unexpected"] = json!(true);
    assert!(serde_json::from_value::<T>(unknown).is_err());
    Ok(())
}
fn legacy_receipt(receipt: &ExecutionReceiptV2) -> Result<ExecutionReceipt, serde_json::Error> {
    let mut value = serde_json::to_value(receipt)?;
    if let Some(object) = value.as_object_mut() {
        for field in ["schema_version", "accepted_selection", "attempt_limit"] {
            object.remove(field);
        }
    }
    if let Some(attempts) = value["attempts"].as_array_mut() {
        for attempt in attempts {
            if let Some(object) = attempt.as_object_mut() {
                object.remove("candidate_index");
                object.remove("observed_http_status");
            }
        }
    }
    serde_json::from_value(value)
}
fn legacy_evidence(
    evidence: &ExecutionUsageEvidenceV2,
) -> Result<ExecutionUsageEvidenceV1, serde_json::Error> {
    let mut value = serde_json::to_value(evidence)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("accepted_selection");
    }
    value["schema_version"] = json!(1);
    value["parent_execution_id"] = Value::Null;
    value["child_execution_ids"] = json!([]);
    serde_json::from_value(value)
}
