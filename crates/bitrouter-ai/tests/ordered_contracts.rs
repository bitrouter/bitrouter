use bitrouter_ai::tenancy::{
    execution::Attempt,
    execution::{Dispatch, ExecutionReceipt, Outcome, Provenance, ReceiptState, Usage},
    ordered::{
        AcceptedSelection, CandidateManifest, GenerationProtocol, OrderedCandidateSelection,
        OrderedCapabilities, OrderedDispatchBinding,
    },
    usage_evidence::{
        AttemptEvidence, Capture, Dimension, Estimation, Frame, Protocol, Quantity, Semantics,
        Source,
    },
    usage_evidence::{EvidenceBudget, ExecutionUsageEvidence},
    validation::Validate,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[path = "ordered_contracts/dispatch.rs"]
mod dispatch;
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
fn selected(indices: Vec<u16>) -> Result<OrderedCandidateSelection, serde_json::Error> {
    serde_json::from_value(
        json!({"schema_version":1, "admission_mode":"unprofiled", "route":reference("route"), "candidate_indices":indices}),
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
fn receipt(count: u16) -> Result<ExecutionReceipt, Box<dyn std::error::Error>> {
    let accepted_selection =
        selected((0..count).collect())?.accept(GenerationProtocol::Messages)?;
    let attempts = (0..count)
        .map(|index| {
            Ok(Attempt {
                attempt_id: format!("attempt-{index}").try_into()?,
                candidate_index: Some(index),
                target: None,
                outcome: Outcome::Failed,
                observed_http_status: Some(503),
                usage: unknown_usage(),
                native_usage: vec![],
            })
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    Ok(ExecutionReceipt {
        installation_id: "installation".try_into()?,
        tenant_id: "tenant".try_into()?,
        principal_id: "principal".try_into()?,
        execution_id: "execution".try_into()?,
        correlation_id: "correlation".try_into()?,
        execution_revision: "revision".try_into()?,
        dispatch: Dispatch::Ordered {
            accepted_selection,
            attempt_limit: 64,
        },
        state: ReceiptState::Committed,
        outcome: Outcome::Failed,
        attempts,
        usage: unknown_usage(),
        recorded_at: "2026-11-02T00:00:00Z".try_into()?,
    })
}
fn evidence(receipt: &ExecutionReceipt) -> ExecutionUsageEvidence {
    ExecutionUsageEvidence {
        installation_id: receipt.installation_id.clone(),
        tenant_id: receipt.tenant_id.clone(),
        execution_id: receipt.execution_id.clone(),
        dispatch: receipt.dispatch.clone(),
        parent_execution_id: None,
        child_execution_ids: vec![],
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
fn frame(receipt: &ExecutionReceipt, sequence: usize, attempt: usize) -> Frame {
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
        if !object[key].is_null() && key != "candidate_index" {
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
fn strict_receipt(receipt: &ExecutionReceipt) -> Result<ExecutionReceipt, serde_json::Error> {
    let mut value = receipt.clone();
    value.dispatch = Dispatch::Strict;
    for attempt in &mut value.attempts {
        attempt.candidate_index = None;
    }
    Ok(value)
}
fn strict_evidence(
    evidence: &ExecutionUsageEvidence,
) -> Result<ExecutionUsageEvidence, serde_json::Error> {
    let mut value = evidence.clone();
    value.dispatch = Dispatch::Strict;
    Ok(value)
}
fn selection_mut(
    dispatch: &mut Dispatch,
) -> Result<&mut AcceptedSelection, Box<dyn std::error::Error>> {
    match dispatch {
        Dispatch::Ordered {
            accepted_selection, ..
        } => Ok(accepted_selection),
        Dispatch::Strict => Err("expected ordered dispatch".into()),
    }
}
fn limit_mut(dispatch: &mut Dispatch) -> Result<&mut u8, Box<dyn std::error::Error>> {
    match dispatch {
        Dispatch::Ordered { attempt_limit, .. } => Ok(attempt_limit),
        Dispatch::Strict => Err("expected ordered dispatch".into()),
    }
}
