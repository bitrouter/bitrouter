//! Wire compatibility for control clients without application runtime dependencies.

use bitrouter_ai::control::reload::{OperationReport, ReloadInput, ReloadOutcome};
use bitrouter_ai::control::{CapabilitiesReport, ControlScope, StateReport};
use serde_json::{Value, json};

#[test]
fn control_error_payloads_preserve_wire_values_without_debug_disclosure()
-> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_ai::control::error::ErrorEnvelope;
    let wire = json!({"error": {"code": "opaque-peer-code", "message": "private peer diagnostic"}});
    let report: ErrorEnvelope = serde_json::from_value(wire.clone())?;
    assert_eq!(serde_json::to_value(&report)?, wire);
    for debug in [format!("{report:?}"), format!("{:?}", report.error)] {
        assert!(!debug.contains("opaque-peer-code"));
        assert!(!debug.contains("private peer diagnostic"));
    }
    Ok(())
}

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
        "protocol": "bitrouter-control", "protocol_version": 2,
        "server_version": "fixture",
        "action_descriptors": {
            "reload": {
                "version": 2, "required_scope": "control:reload",
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

    // Names from protocol one cannot replace mandatory current descriptors.
    assert!(
        serde_json::from_value::<CapabilitiesReport>(json!({
            "protocol": "bitrouter-control", "protocol_version": 1,
            "server_version": "legacy", "actions": ["status", "models"]
        }))
        .is_err()
    );
    Ok(())
}

#[test]
fn protocol_one_reader_refuses_current_capability_bytes() -> Result<(), Box<dyn std::error::Error>>
{
    // Frozen peer boundary from the protocol-one client: exact protocol/version
    // and its required action-name list, before descriptor-based dispatch.
    #[derive(serde::Deserialize)]
    struct ProtocolOneCapabilities {
        protocol: String,
        protocol_version: u32,
        actions: Vec<String>,
    }
    fn protocol_one_accepts(raw: &str) -> Result<bool, serde_json::Error> {
        let report: ProtocolOneCapabilities = serde_json::from_str(raw)?;
        Ok(report.protocol == "bitrouter-control"
            && report.protocol_version == 1
            && report.actions.iter().any(|name| name == "status"))
    }
    let old = r#"{"protocol":"bitrouter-control","protocol_version":1,"server_version":"old","actions":["status"]}"#;
    let current = r#"{"protocol":"bitrouter-control","protocol_version":2,"server_version":"current","action_descriptors":{"status":{"version":1,"required_scope":"control:read","input_schema":{},"output_schema":{}}},"scopes":["control:read"]}"#;
    assert!(protocol_one_accepts(old)?);
    assert!(protocol_one_accepts(current).is_err());
    let mut version_only = serde_json::from_str::<Value>(old)?;
    version_only["protocol_version"] = json!(2);
    assert!(!protocol_one_accepts(&serde_json::to_string(
        &version_only
    )?)?);
    assert!(serde_json::from_str::<CapabilitiesReport>(old).is_err());
    let report: CapabilitiesReport = serde_json::from_str(current)?;
    assert_eq!(report.protocol_version, 2);
    assert!(serde_json::to_value(report)?.get("actions").is_none());
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

#[test]
fn current_policy_report_carries_resubmittable_source_and_refuses_runtime_ir()
-> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_ai::control::administration::PolicyReport;
    let raw = r#"{"view":"active","availability":"available","source_digest":"sha256:fixture","policies":["auto"],"bindings":{"coding":"auto"},"source":{"spec_version":"policy/1.0.0-beta.1","policies":{"auto":{"tiers":{"strong":{"model":"fixture/strong"}},"default_tier":"strong"}}}}"#;
    let report: PolicyReport = serde_json::from_str(raw)?;
    let source: &bitrouter_ai::policy::Document = report.source.as_ref().ok_or("missing source")?;
    let target: &bitrouter_ai::policy::Target = &source.policies["auto"].tiers["strong"];
    assert_eq!(target.model, "fixture/strong");
    assert_eq!(
        serde_json::to_value(report)?,
        serde_json::from_str::<Value>(raw)?
    );
    for changed in [
        raw.replacen('{', "{\"mode\":\"adaptive\",", 1),
        raw.replacen('{', "{\"definitions\":{},", 1),
        raw.replacen('{', "{\"certificates\":{},", 1),
        raw.replace(
            "\"model\":\"fixture/strong\"",
            "\"model\":\"fixture/strong\",\"context\":\"evidence\"",
        ),
        raw.replace(
            "\"strong\":{\"model\":\"fixture/strong\"}",
            "\"strong\":{\"model\":\"fixture/strong\"},\"strong\":{\"model\":\"other\"}",
        ),
    ] {
        assert!(serde_json::from_str::<PolicyReport>(&changed).is_err());
    }
    let old = r#"{"view":"active","availability":"available","digest":"old","mode":"frozen","policies":[],"bindings":{},"definitions":{},"evidence_root":null,"eval_snapshot_root":null}"#;
    assert!(serde_json::from_str::<PolicyReport>(old).is_err());
    Ok(())
}
