//! Observe native lifecycle metadata through the maintained Claude ACP adapter.

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

pub const METHOD: &str = "_claude/sdkMessage";

const SYSTEM_EVENTS: &[&str] = &[
    "init",
    "session_state_changed",
    "background_tasks_changed",
    "task_started",
    "task_progress",
    "task_updated",
    "task_notification",
    "compact_boundary",
];
const OTHER_EVENTS: &[&str] = &["command_lifecycle", "result", "conversation_reset"];

/// Raw SDK forwarding is an adapter setting, outside its SDK options object.
/// Preserve existing raw filters while requesting the lifecycle signals used
/// by evidence collection. This does not change model or permission settings.
/// https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts
pub fn instrument(meta: &mut Map<String, Value>) -> Result<()> {
    let configured = meta
        .entry("emitRawSDKMessages")
        .or_insert_with(|| json!([]));
    if *configured == Value::Bool(true) {
        return Ok(());
    }
    if configured.is_null() || *configured == Value::Bool(false) {
        *configured = json!([]);
    }
    let filters = configured
        .as_array_mut()
        .context("Claude raw message filters must be an array or boolean")?;
    for filter in SYSTEM_EVENTS
        .iter()
        .map(|subtype| json!({"type":"system","subtype":subtype}))
        .chain(OTHER_EVENTS.iter().map(|kind| json!({"type":kind})))
    {
        if !filters.contains(&filter) {
            filters.push(filter);
        }
    }
    Ok(())
}

/// Capture only lifecycle fields. SDK init/configuration and evaluator content
/// can contain unrelated sensitive data; the original extension is forwarded
/// to the manager unchanged and the local evidence journal receives this view.
/// https://code.claude.com/docs/en/agent-sdk/typescript
pub fn notification_fields(payload: &Value) -> Option<Value> {
    let message = payload.get("message")?;
    let kind = message.get("type")?.as_str()?;
    let subtype = message.get("subtype").and_then(Value::as_str);
    if !(OTHER_EVENTS.contains(&kind)
        || (kind == "system" && subtype.is_some_and(|value| SYSTEM_EVENTS.contains(&value))))
    {
        return None;
    }
    let mut invalid = false;
    let mut selected = fields(
        message,
        &[
            "type",
            "subtype",
            "uuid",
            "session_id",
            "claude_code_version",
            "capabilities",
            "state",
            "command_uuid",
            "task_id",
            "tool_use_id",
            "task_type",
            "subagent_type",
            "status",
            "user_message_uuid",
            "is_error",
            "stop_reason",
            "terminal_reason",
            "new_conversation_id",
        ],
        &mut invalid,
    );
    if let Some(patch) = message.get("patch") {
        selected.insert(
            "patch".into(),
            Value::Object(fields(
                patch,
                &["status", "is_backgrounded", "end_time"],
                &mut invalid,
            )),
        );
    }
    if let Some(tasks) = message.get("tasks") {
        let tasks = if let Some(tasks) = tasks.as_array() {
            Value::Array(
                tasks
                    .iter()
                    .map(|task| {
                        Value::Object(fields(task, &["task_id", "task_type"], &mut invalid))
                    })
                    .collect(),
            )
        } else {
            invalid = true;
            Value::Null
        };
        selected.insert("tasks".into(), tasks);
    }
    if let Some(origin) = message.get("origin") {
        selected.insert(
            "origin".into(),
            Value::Object(fields(origin, &["kind", "senderTaskId"], &mut invalid)),
        );
    }
    // Compact boundaries remain references to the native transcript projection;
    // their UUID is enough here. Do not duplicate messages or cost counters.
    let mut envelope = fields(payload, &["sessionId"], &mut invalid);
    if invalid {
        selected.insert("bitrouter_capture_invalid".into(), Value::Bool(true));
    }
    envelope.insert("message".into(), Value::Object(selected));
    Some(Value::Object(envelope))
}

fn fields(value: &Value, names: &[&str], invalid: &mut bool) -> Map<String, Value> {
    if !value.is_object() {
        *invalid = true;
    }
    names
        .iter()
        .filter_map(|name| {
            value.get(*name).map(|value| {
                let valid = value.is_null()
                    || match *name {
                        "capabilities" => value
                            .as_array()
                            .is_some_and(|values| values.iter().all(Value::is_string)),
                        "is_error" | "is_backgrounded" => value.is_boolean(),
                        "end_time" => value.is_number(),
                        _ => value.is_string(),
                    };
                if !valid {
                    *invalid = true;
                }
                (
                    (*name).into(),
                    if valid { value.clone() } else { Value::Null },
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_filters_are_idempotent_and_preserve_existing_selection() -> Result<()> {
        for configured in [
            Value::Bool(false),
            Value::Bool(true),
            json!([{"type":"assistant","origin":"peer"}]),
        ] {
            let mut meta = Map::from_iter([("emitRawSDKMessages".into(), configured.clone())]);
            instrument(&mut meta)?;
            let once = meta.clone();
            instrument(&mut meta)?;
            assert_eq!(meta, once);
            if let Some(filters) = configured.as_array() {
                assert!(
                    meta["emitRawSDKMessages"]
                        .as_array()
                        .context("filters")?
                        .contains(&filters[0])
                );
            }
            if configured == Value::Bool(true) {
                assert_eq!(meta["emitRawSDKMessages"], configured);
            }
        }
        Ok(())
    }

    #[test]
    fn only_native_lifecycle_metadata_enters_the_journal() -> Result<()> {
        let captured = notification_fields(&json!({"sessionId":"s","message":{
            "type":"system","subtype":"init","session_id":"s","uuid":"init", "claude_code_version":"2.1.220",
            "capabilities":["msg_lifecycle_v1"],"apiKeySource":"fixture-secret","mcp_servers":[{"name":"private"}],"tools":["private"]
        }})).context("init metadata")?;
        assert_eq!(
            captured["message"]["capabilities"],
            json!(["msg_lifecycle_v1"])
        );
        assert!(!serde_json::to_string(&captured)?.contains("private"));
        assert!(!serde_json::to_string(&captured)?.contains("fixture-secret"));
        assert!(
            notification_fields(
                &json!({"sessionId":"s","message":{"type":"auth_status","apiKey":"fixture-secret"}})
            )
            .is_none()
        );
        let background = notification_fields(&json!({"sessionId":"s","message":{
            "type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"task","description":"private"}]
        }})).context("background metadata")?;
        assert_eq!(background["message"]["tasks"], json!([{"task_id":"task"}]));
        Ok(())
    }

    #[test]
    fn malformed_lifecycle_shapes_keep_a_fixed_marker_without_nested_content() -> Result<()> {
        for message in [
            json!({"type":"system","subtype":"background_tasks_changed","tasks":{"prompt":"fixture-secret"}}),
            json!({"type":"system","subtype":"task_started","task_id":{"prompt":"fixture-secret"}}),
            json!({"type":"system","subtype":"init","capabilities":[{"token":"fixture-secret"}]}),
            json!({"type":"system","subtype":"task_updated","patch":{"status":{"prompt":"fixture-secret"}}}),
        ] {
            let captured = notification_fields(&json!({"sessionId":"s","message":message}))
                .context("selected metadata")?;
            assert!(!serde_json::to_string(&captured)?.contains("fixture-secret"));
            assert_eq!(
                captured["message"]["bitrouter_capture_invalid"],
                Value::Bool(true)
            );
        }
        Ok(())
    }
}
