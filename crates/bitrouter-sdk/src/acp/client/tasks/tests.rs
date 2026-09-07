use super::*;
use agent_client_protocol::schema::ProtocolVersion;
use serde_json::json;

fn initialize(capability: serde_json::Value) -> InitializeResponse {
    let mut init = InitializeResponse::new(ProtocolVersion::V1);
    init.meta = Some(
        [(
            CONTROLLER_META_KEY.into(),
            json!({"taskControl":capability}),
        )]
        .into_iter()
        .collect(),
    );
    init
}

#[test]
fn task_capabilities_require_version_scope_and_each_method() {
    let valid = json!({"version":"1", "scope":"session", "methods":[TaskMethod::Status.wire(),TaskMethod::Select.wire()]});
    for block in [
        serde_json::Value::Null,
        json!({}),
        json!({"version":"2", "scope":"session", "methods":valid["methods"]}),
        json!({"version":"1", "scope":"global", "methods":valid["methods"]}),
    ] {
        let capability = TaskControlCapability::from_init(&initialize(block));
        assert!(!capability.allows(TaskMethod::Status));
        assert!(!capability.allows(TaskMethod::Select));
    }
    for method in [TaskMethod::Status, TaskMethod::Select] {
        let capability = TaskControlCapability::from_init(&initialize(
            json!({"version":"1", "scope":"session", "methods":[method.wire(),123]}),
        ));
        assert!(capability.allows(method));
        assert!(!capability.allows(match method {
            TaskMethod::Status => TaskMethod::Select,
            TaskMethod::Select => TaskMethod::Status,
        }));
    }
    let capability = TaskControlCapability::from_init(&initialize(valid));
    assert!(capability.allows(TaskMethod::Status));
    assert!(capability.allows(TaskMethod::Select));
}

#[test]
fn legacy_conflicts_and_unverified_outcomes_remain_unknown() {
    for data in [
        json!({"code":"task_control_conflict"}),
        json!({"code":"task_control_conflict", "outcome":"unknown"}),
        json!({"code":"another_error", "outcome":"not_applied"}),
    ] {
        assert!(matches!(
            TaskControlError::from_rpc(agent_client_protocol::Error::invalid_request().data(data)),
            TaskControlError::Unknown(_)
        ));
    }
    let certified = json!({"code":"task_control_conflict", "outcome":"not_applied", "message":"Refresh task state."});
    assert!(matches!(
        TaskControlError::from_rpc(
            agent_client_protocol::Error::invalid_request().data(certified.clone())
        ),
        TaskControlError::NotApplied(_)
    ));
    assert!(matches!(
        TaskControlError::from_rpc(agent_client_protocol::Error::internal_error().data(certified)),
        TaskControlError::Unknown(_)
    ));
}
