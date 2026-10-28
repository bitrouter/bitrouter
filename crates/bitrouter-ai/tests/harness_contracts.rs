use bitrouter_ai::core::protocol::ModelMode;
use bitrouter_ai::harness::agent::AgentConfig;
use bitrouter_ai::harness::api::{self, HistoryQuery, ObserveQuery};
use bitrouter_ai::harness::item::{CallOrigin, CallRecord};
use bitrouter_ai::harness::runtime::ErrorCode;
use bitrouter_ai::harness::thread::{ThreadChange, ThreadObservation};
use bitrouter_ai::harness::turn::{TurnEventPayload, TurnStatus};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

type Result = std::result::Result<(), Box<dyn std::error::Error>>;
const CONTENT: &str = "harness-contract-sensitive-content";

fn roundtrip<T: DeserializeOwned + Serialize>(wire: Value) -> Result {
    let value: T = serde_json::from_value(wire.clone())?;
    assert_eq!(serde_json::to_value(value)?, wire);
    Ok(())
}

#[test]
fn capabilities_retain_owner_metadata_and_legacy_native_default() -> Result {
    let wire: Value = serde_json::from_str(include_str!("fixtures/harness/capabilities.json"))?;
    roundtrip::<api::CapabilitiesResponse>(wire.clone())?;
    let response: api::CapabilitiesResponse = serde_json::from_value(wire.clone())?;
    assert!(!response.explicit_skill_consent);
    assert!(!response.runtime.native_execution);
    assert!(!response.runtime.skill_constraints);
    assert!(matches!(
        response.runtime.execution_ownership,
        Some(bitrouter_ai::harness::runtime::OwnerClaim::Blocked { .. })
    ));
    let mut legacy = wire;
    legacy
        .as_object_mut()
        .ok_or("capabilities fixture is not an object")?
        .remove("explicit_skill_consent");
    let runtime = legacy["runtime"]
        .as_object_mut()
        .ok_or("runtime fixture is not an object")?;
    for key in [
        "native_execution",
        "skill_constraints",
        "startup_discovery",
        "execution_ownership",
    ] {
        runtime.remove(key);
    }
    let response: api::CapabilitiesResponse = serde_json::from_value(legacy.clone())?;
    assert!(!response.explicit_skill_consent);
    assert!(response.runtime.native_execution);
    assert!(!response.runtime.skill_constraints);
    assert!(response.runtime.execution_ownership.is_none());
    assert!(response.runtime.startup_discovery.is_none());
    legacy["runtime"]["limits"]
        .as_object_mut()
        .ok_or("limits fixture is not an object")?
        .remove("request_bytes");
    assert!(serde_json::from_value::<api::CapabilitiesResponse>(legacy).is_err());
    Ok(())
}

#[test]
fn request_defaults_match_existing_ingress_without_defaulting_required_controls() -> Result {
    let create: api::CreateThreadRequest = serde_json::from_value(json!({
        "workspace": "/fixture", "model": "fixture"
    }))?;
    assert_eq!(create.model_mode, ModelMode::Fixed);
    assert!(!create.read_only);
    assert_eq!(create.max_output_tokens, None);
    assert!(create.skills.is_none());
    assert_eq!(
        serde_json::to_value(create)?,
        json!({
            "workspace": "/fixture", "model": "fixture", "model_mode": "fixed",
            "effort": null, "max_output_tokens": null, "read_only": false,
            "verification_command": null
        })
    );
    assert!(
        serde_json::from_value::<api::CreateThreadRequest>(json!({"model":"fixture"})).is_err()
    );
    for wire in [
        json!({"prompt":"x"}),
        json!({"mode":"start"}),
        json!({"prompt":"x","mode":"resume"}),
    ] {
        assert!(serde_json::from_value::<api::TurnRequest>(wire).is_err());
    }
    assert!(
        serde_json::from_value::<api::InputRequest>(json!({"turn_id":"t","request_id":"r"}))
            .is_err()
    );
    assert!(serde_json::from_value::<api::CancelRequest>(json!({})).is_err());
    let history: HistoryQuery = serde_json::from_value(json!({}))?;
    assert_eq!(
        (history.after, history.cutoff, history.limit),
        (0, None, 128)
    );
    // Range policy remains server admission; serde must not silently normalize it.
    let history: HistoryQuery = serde_json::from_value(json!({"limit":0}))?;
    assert_eq!(history.limit, 0);
    assert!(serde_json::from_value::<HistoryQuery>(json!({"limit":null})).is_err());
    let observe: ObserveQuery = serde_json::from_value(json!({}))?;
    assert_eq!(observe.after, None);
    roundtrip::<api::TurnRequest>(json!({"prompt":CONTENT,"mode":"enqueue"}))?;
    roundtrip::<api::InputRequest>(
        json!({"turn_id":"t","request_id":"r","approved":false,"skill_consent":false}),
    )?;
    roundtrip::<api::SteeringRequest>(json!({"expected_turn_id":"t","text":CONTENT}))?;
    roundtrip::<api::CancelRequest>(json!({"mode":"queued"}))
}

#[test]
fn nested_recovery_resources_and_inspection_preserve_the_public_wire() -> Result {
    let wire: Value = serde_json::from_str(include_str!("fixtures/harness/view.json"))?;
    roundtrip::<api::ThreadViewResponse>(wire.clone())?;
    let response: api::ThreadViewResponse = serde_json::from_value(wire.clone())?;
    let turn = response
        .view
        .latest_turn
        .as_ref()
        .ok_or("missing Turn fixture")?;
    assert_eq!(turn.status, TurnStatus::RecoveryRequired);
    assert!(
        turn.resources
            .as_ref()
            .is_some_and(|resources| resources.tools.len() == 1)
    );
    let recovery = response
        .view
        .recovery
        .as_ref()
        .ok_or("missing recovery fixture")?;
    assert_eq!(recovery.source_server_instance_id, "previous");
    roundtrip::<api::ThreadResponse>(json!({"version":2,"thread":wire["view"]["thread"]}))?;
    roundtrip::<api::TurnResponse>(json!({"version":2,"turn":wire["view"]["latest_turn"]}))?;
    roundtrip::<api::SteeringReceiptResponse>(
        json!({"version":2,"receipt":wire["view"]["latest_turn"]["steering"][0]}),
    )?;
    assert!(!format!("{response:?} {turn:?} {recovery:?}").contains(CONTENT));
    assert!(!format!("{:?} {:?}", turn.resources, turn.context_routing).contains(CONTENT));
    assert!(!format!("{:?}", response.view.config).contains(CONTENT));
    Ok(())
}

#[test]
fn observation_variants_keep_historical_identity_and_live_cursor_anchor() -> Result {
    let wires: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/harness/observations.json"))?;
    for wire in &wires {
        roundtrip::<ThreadObservation>(wire.clone())?;
        assert!(wire.get("version").is_none());
        let value: ThreadObservation = serde_json::from_value(wire.clone())?;
        assert!(!format!("{value:?}").contains(CONTENT));
        match value {
            ThreadObservation::Snapshot {
                view,
                catchup,
                resynchronized,
            } => {
                assert!(resynchronized);
                assert_eq!(view.thread.server_instance_id, "current");
                assert_eq!(catchup[0].server_instance_id, "previous");
            }
            ThreadObservation::Event { event } => {
                assert_eq!(event.seq, 8);
                assert!(matches!(
                    event.changes[0],
                    ThreadChange::ContextRouting { .. }
                ));
            }
            ThreadObservation::Live {
                after_cursor,
                event,
            } => {
                assert_eq!(after_cursor, 9);
                assert_eq!(event.server_instance_id, "current");
                assert!(matches!(
                    event.payload,
                    TurnEventPayload::AssistantDelta { .. }
                ));
            }
        }
    }
    roundtrip::<api::HistoryResponse>(json!({"version":2,"history":{
        "server_instance_id":"current","thread_id":"thread-1","cutoff":9,
        "events":[wires[1]["event"]],"next_after":null
    }}))
}

#[test]
fn legacy_optional_fields_keep_their_distinct_null_and_omission_rules() -> Result {
    let mut wire: Value = serde_json::from_str(include_str!("fixtures/harness/view.json"))?;
    wire["view"]
        .as_object_mut()
        .ok_or("view fixture is not an object")?
        .remove("recovery");
    let turn = wire["view"]["latest_turn"]
        .as_object_mut()
        .ok_or("turn fixture is not an object")?;
    for key in [
        "context_routing",
        "resources",
        "live",
        "steering",
        "tool_mode",
    ] {
        turn.remove(key);
    }
    let decoded: api::ThreadViewResponse = serde_json::from_value(wire)?;
    let encoded = serde_json::to_value(decoded)?;
    assert!(encoded["view"].get("recovery").is_none());
    for key in ["context_routing", "resources", "live"] {
        assert!(encoded["view"]["latest_turn"].get(key).is_none());
    }
    assert_eq!(encoded["view"]["latest_turn"]["tool_mode"], "coding");
    assert_eq!(encoded["view"]["latest_turn"]["steering"], json!([]));
    assert_eq!(
        encoded["view"]["latest_turn"].get("final_answer"),
        Some(&Value::Null)
    );
    let config: AgentConfig = serde_json::from_value(encoded["view"]["config"].clone())?;
    assert_eq!(config.model_mode, ModelMode::Fixed);
    let encoded_config = serde_json::to_value(config)?;
    assert!(encoded_config.get("model_mode").is_none());
    assert!(encoded_config.get("max_output_tokens").is_none());
    let call: CallRecord = serde_json::from_value(json!({
        "item_id":"i","provider_call_id":"p","name":"tool","arguments":CONTENT
    }))?;
    assert_eq!(call.origin, CallOrigin::Model);
    assert!(!format!("{call:?}").contains(CONTENT));
    Ok(())
}

#[test]
fn http_and_observation_errors_keep_distinct_required_payloads() -> Result {
    let error = json!({"error":CONTENT,"code":"unsupported_capability"});
    let observation = json!({"message":CONTENT,"code":"unsupported_capability"});
    roundtrip::<api::ErrorResponse>(error.clone())?;
    roundtrip::<api::ObservationError>(observation.clone())?;
    assert!(serde_json::from_value::<api::ErrorResponse>(observation.clone()).is_err());
    assert!(serde_json::from_value::<api::ObservationError>(error.clone()).is_err());
    let error: api::ErrorResponse = serde_json::from_value(error)?;
    assert_eq!(error.code, ErrorCode::UnsupportedCapability);
    let observation: api::ObservationError = serde_json::from_value(observation)?;
    assert!(!format!("{error:?} {observation:?}").contains(CONTENT));
    roundtrip::<api::TurnReceiptResponse>(json!({"version":2,"receipt":{
        "user_item_id":"i","thread_id":"t","turn_id":"u","queue_order":1,"status":"queued"
    }}))
}
