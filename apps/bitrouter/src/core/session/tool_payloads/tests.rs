//! Inert record/validator checks; these fixtures grant no live authority.
use super::*;
use crate::storage::checkpoint::validation::ToolResultLimitsExt;
use bitrouter_ai::core::protocol::{ArtifactRef, ToolObservation, ToolStatus};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn call(frozen: Option<ToolResultLimits>) -> Result<Invocation, serde_json::Error> {
    serde_json::from_value(json!({
        "dispatch":{"invocation_id":"tool","attempt_id":"attempt","run_id":"run","agent_id":"root",
            "agent_turn_id":"turn","step_id":"step","context_revision":0,"tool":"read","arguments":{"path":"source.txt"},
            "tool_manifest_digest":"digest","permission_revision":1,"workspace_id":"workspace",
            "execution_epoch":1,"authorizing_event_seq":1,"verification":false,"result_limits":frozen},
        "public_call_id":"call","provider_call_id":"provider-call","consumed":false,
        "result_limit_bytes":1024,"effect":"read","signal_revision":0
    }))
}

#[test]
fn stored_large_and_absent_artifact_contracts_survive_actual_record_decode() -> TestResult {
    let host = Limits::default();
    for artifact_bytes in [Some(798_915), None] {
        let mut frozen =
            crate::storage::checkpoint::validation::tool_result_limits(host.input_bytes, 1024)?;
        frozen.artifact_bytes = artifact_bytes;
        let mut invocation = call(Some(frozen))?;
        invocation.recovery_archive_allowance =
            Some(archive::observation_allowance(&invocation, frozen)?);
        let bytes = serde_json::to_vec(&invocation)?;
        let decoded: Invocation = serde_json::from_slice(&bytes)?;
        assert_eq!(limits(&decoded, &host, None, Some(&host))?, frozen);
        assert_eq!(serde_json::to_vec(&decoded)?, bytes);
        assert_eq!(remaining_artifact_slots(&decoded), 5);
    }
    Ok(())
}

#[test]
fn legacy_intent_without_result_limits_acquires_no_new_artifact_bound() -> TestResult {
    let host = Limits::default();
    let original = call(None)?;
    let bytes = serde_json::to_vec(&original)?;
    let decoded: Invocation = serde_json::from_slice(&bytes)?;
    let recovered = limits(&decoded, &host, None, Some(&host))?;
    assert_eq!(recovered.artifact_bytes, None);
    assert_eq!(decoded.dispatch.result_limits, None);
    assert_eq!(serde_json::to_vec(&decoded)?, bytes);
    Ok(())
}

#[test]
fn frozen_body_bound_applies_to_results_and_each_first_status_without_losing_slots() -> TestResult {
    let mut frozen = crate::storage::checkpoint::validation::tool_result_limits(8192, 1024)?;
    frozen.artifact_bytes = Some(64 * 1024);
    let invocation = call(Some(frozen))?;
    assert_eq!(remaining_artifact_slots(&invocation), 5);
    for bytes in [64 * 1024, 64 * 1024 + 1] {
        let evidence = vec![ArtifactRef {
            artifact_id: "bounded-evidence".into(),
            sha256: "0".repeat(64),
            bytes,
            media_type: "application/octet-stream".into(),
        }];
        for status in [ToolOutcome::EffectUnknown, ToolOutcome::Succeeded] {
            let result = ToolResult {
                invocation_id: "tool".into(),
                attempt_id: "attempt".into(),
                status,
                output: String::new(),
                evidence: evidence.clone(),
                workspace_revision: None,
            };
            let checked = frozen.validate_result(&result);
            if bytes == 64 * 1024 {
                checked?;
            } else {
                assert!(matches!(
                    checked,
                    Err(CoreError {
                        code: ErrorCode::LimitExceeded,
                        ..
                    })
                ));
            }
        }
        for status in [
            ToolStatus::Running,
            ToolStatus::Stopped,
            ToolStatus::EffectUnknown,
        ] {
            let observation = ToolObservation {
                invocation_id: "tool".into(),
                attempt_id: "attempt".into(),
                status,
                evidence: evidence.clone(),
            };
            let checked = frozen.validate_observation(&observation);
            if bytes == 64 * 1024 {
                checked?;
            } else {
                assert!(matches!(
                    checked,
                    Err(CoreError {
                        code: ErrorCode::LimitExceeded,
                        ..
                    })
                ));
            }
        }
    }
    Ok(())
}
