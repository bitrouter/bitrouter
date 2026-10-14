//! Wire compatibility for control clients without application runtime dependencies.

use bitrouter_ai::control::reload::{OperationReport, ReloadInput, ReloadOutcome};
use bitrouter_ai::control::{CapabilitiesReport, ControlScope, StateReport};
use serde_json::{Value, json};

fn partial_reload() -> Value {
    json!({
        "server_instance_id": "47da0290-4510-4f39-ab53-a0b09ba707c2",
        "generation": 3,
        "outcome": "partially_applied",
        "participants": [
            {"participant": "routing_table", "outcome": "applied"},
            {
                "participant": "named_policy_runtime", "outcome": "failed",
                "error": {"code": "policy_apply_failed", "message": "policy table update failed"}
            }
        ],
        "restart_required_fields": ["server.listen"],
        "started_at": "2026-10-15T01:02:03Z",
        "completed_at": "2026-10-15T01:02:04Z"
    })
}

#[test]
fn capability_scopes_and_descriptor_schemas_keep_their_wire_shape()
-> Result<(), Box<dyn std::error::Error>> {
    let wire = json!({
        "protocol": "bitrouter-control", "protocol_version": 1,
        "server_version": "fixture", "actions": ["reload"],
        "action_descriptors": {
            "reload": {
                "version": 1, "required_scope": "control:reload",
                "input_schema": {"type": "object", "additionalProperties": false},
                "output_schema": {"$ref": "#/$defs/OperationReport", "$defs": {"OperationReport": {"type": "object"}}}
            }
        },
        "resources": {"state": {"version": 1, "path": "/control/v1/state", "required_scope": "control:read"}},
        "limits": {"max_response_bytes": 8388608},
        "server_instance_id": "47da0290-4510-4f39-ab53-a0b09ba707c2",
        "scopes": ["control:read", "control:reload"]
    });
    let report: CapabilitiesReport = serde_json::from_value(wire.clone())?;
    assert_eq!(report.scopes, [ControlScope::Read, ControlScope::Reload]);
    assert_eq!(serde_json::to_value(report)?, wire);

    // An older server advertises only its legacy action names. Missing grants
    // must remain absent, rather than being inferred from the advertised verbs.
    let legacy: CapabilitiesReport = serde_json::from_value(json!({
        "protocol": "bitrouter-control", "protocol_version": 1,
        "server_version": "legacy", "actions": ["status", "models"]
    }))?;
    assert!(legacy.scopes.is_empty());
    assert!(legacy.action_descriptors.is_empty());
    assert!(legacy.resources.is_empty());
    assert!(legacy.server_instance_id.is_none());
    Ok(())
}

#[test]
fn flattened_state_retains_independent_generations_and_mixed_history()
-> Result<(), Box<dyn std::error::Error>> {
    let partial = partial_reload();
    let wire = json!({
        "server_instance_id": "47da0290-4510-4f39-ab53-a0b09ba707c2",
        "generation": 4, "running": false, "consistency": "mixed",
        "last_outcome": partial,
        "mixed_state_history": [partial],
        "live_policy_digest": null,
        "config_state": {
            "server_instance_id": "47da0290-4510-4f39-ab53-a0b09ba707c2",
            "generation": 3,
            "source": "file", "saved": "available", "running": "mixed",
            "named_policy": "reload_required", "access_policies": "not_configured",
            "last_reload": partial,
            "mixed_state_history": [partial]
        }
    });
    let report: StateReport = serde_json::from_value(wire.clone())?;
    assert_eq!(report.reload.generation, 4);
    assert_eq!(
        report
            .config_state
            .as_ref()
            .and_then(|state| state.generation),
        Some(3)
    );
    assert_eq!(
        report.reload.mixed_state_history[0].outcome,
        ReloadOutcome::PartiallyApplied
    );
    assert_eq!(serde_json::to_value(report)?, wire);
    Ok(())
}

#[test]
fn reload_identity_and_unknown_operation_remain_recoverable()
-> Result<(), Box<dyn std::error::Error>> {
    let input_wire = json!({
        "request_id": "0a01c713-e6ef-4077-b9bc-e89c58dc3ea6",
        "expected_server_instance_id": "47da0290-4510-4f39-ab53-a0b09ba707c2",
        "expected_generation": 3
    });
    let input: ReloadInput = serde_json::from_value(input_wire.clone())?;
    assert_eq!(serde_json::to_value(&input)?, input_wire);
    let mut forbidden = input_wire;
    forbidden["configuration"] = json!({"server": {"listen": "0.0.0.0:9999"}});
    assert!(serde_json::from_value::<ReloadInput>(forbidden).is_err());

    let wire = json!({
        "request_id": input.request_id,
        "server_instance_id": input.expected_server_instance_id,
        "generation_before": input.expected_generation,
        "status": "unknown", "accepted_at_unix_ms": 1792026123000_i64,
        "completed_at_unix_ms": null, "retain_until_unix_ms": null,
        "lookup_url": "/control/v1/operations/0a01c713-e6ef-4077-b9bc-e89c58dc3ea6?instance=47da0290-4510-4f39-ab53-a0b09ba707c2",
        "result": null
    });
    let report: OperationReport = serde_json::from_value(wire.clone())?;
    assert!(!report.is_running());
    assert!(!report.succeeded());
    assert_eq!(serde_json::to_value(report)?, wire);
    Ok(())
}
