//! Interpretation of stored ACP setup notifications, without a live ACP runtime.

use serde::Deserialize;
use serde::de::IgnoredAny;

/// Setup notifications do not by themselves establish that a turn executed.
/// Preserve the recorded schema's admission rules, including its permissive
/// nested lists/metadata, rather than accepting a discriminator alone.
///
/// Baseline: https://docs.rs/agent-client-protocol-schema/1.10.2/agent_client_protocol_schema/v1/struct.SessionNotification.html
pub(crate) fn is_administrative_update(payload: &serde_json::Value) -> bool {
    serde_json::from_value::<AdministrativeNotification>(payload.clone()).is_ok()
}

#[derive(Deserialize)]
struct AdministrativeNotification {
    #[serde(rename = "sessionId")]
    _session_id: String,
    #[serde(rename = "update")]
    _update: AdministrativeUpdate,
    // The original schema defaults malformed metadata. It never changes this
    // classification, but this third field preserves sequence deserialization.
    #[serde(default, rename = "_meta")]
    _meta: IgnoredAny,
}

#[derive(Deserialize)]
#[serde(tag = "sessionUpdate")]
enum AdministrativeUpdate {
    #[serde(rename = "available_commands_update")]
    AvailableCommands {
        // DefaultOnError<VecSkipError<_>> accepts any present JSON value, but
        // serde's deserialize_with field handling still requires its presence.
        #[serde(rename = "availableCommands", deserialize_with = "ignore_present")]
        _commands: IgnoredAny,
        #[serde(default, rename = "_meta")]
        _meta: IgnoredAny,
    },
    #[serde(rename = "config_option_update")]
    ConfigOption {
        #[serde(rename = "configOptions", deserialize_with = "ignore_present")]
        _options: IgnoredAny,
        #[serde(default, rename = "_meta")]
        _meta: IgnoredAny,
    },
    #[serde(rename = "current_mode_update")]
    CurrentMode {
        #[serde(rename = "currentModeId")]
        _mode: String,
        #[serde(default, rename = "_meta")]
        _meta: IgnoredAny,
    },
    // All session-info fields default missing or malformed values. Keep field
    // order as well as names: serde also accepts tagged sequence updates.
    #[serde(rename = "session_info_update")]
    SessionInfo {
        #[serde(default, rename = "title")]
        _title: IgnoredAny,
        #[serde(default, rename = "updatedAt")]
        _updated_at: IgnoredAny,
        #[serde(default, rename = "_meta")]
        _meta: IgnoredAny,
    },
}

fn ignore_present<'de, D>(deserializer: D) -> Result<IgnoredAny, D::Error>
where
    D: serde::Deserializer<'de>,
{
    IgnoredAny::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::is_administrative_update;

    fn baseline(payload: &Value) -> bool {
        use agent_client_protocol_schema::v1::{SessionNotification, SessionUpdate};

        serde_json::from_value::<SessionNotification>(payload.clone()).is_ok_and(|event| {
            matches!(
                event.update,
                SessionUpdate::AvailableCommandsUpdate(_)
                    | SessionUpdate::ConfigOptionUpdate(_)
                    | SessionUpdate::CurrentModeUpdate(_)
                    | SessionUpdate::SessionInfoUpdate(_)
            )
        })
    }

    fn assert_baseline(payload: &Value) {
        assert_eq!(
            is_administrative_update(payload),
            baseline(payload),
            "stored ACP classification changed for {payload}"
        );
    }

    fn updates() -> Vec<Value> {
        vec![
            json!({"sessionUpdate":"available_commands_update", "availableCommands":[
                {"name":"build", "description":"Build the project"}
            ]}),
            json!({"sessionUpdate":"config_option_update", "configOptions":[]}),
            json!({"sessionUpdate":"current_mode_update", "currentModeId":"code"}),
            json!({"sessionUpdate":"session_info_update"}),
            json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"work"}}),
            json!({"sessionUpdate":"usage_update", "used":10,"size":100}),
            json!({"sessionUpdate":"unrecognized_update"}),
        ]
    }

    fn values() -> Vec<Value> {
        vec![
            Value::Null,
            json!(false),
            json!(1),
            json!(1.25),
            json!(""),
            json!("session"),
            json!([]),
            json!([null, 1, {}, {"name":"partial"}]),
            json!({}),
            json!({"nested":{"opaque":[null,42]}}),
        ]
    }

    #[test]
    fn administrative_setup_and_content_have_distinct_classification() {
        for (index, update) in updates().into_iter().enumerate() {
            let payload = json!({"sessionId":"session", "update":update});
            assert_eq!(is_administrative_update(&payload), index < 4);
            assert_baseline(&payload);
        }
    }

    #[test]
    fn required_fields_and_permissive_nested_values_remain_distinct() {
        for (field, kind) in [
            ("availableCommands", "available_commands_update"),
            ("configOptions", "config_option_update"),
        ] {
            let mut payload = json!({"sessionId":"session", "update":{"sessionUpdate":kind}});
            assert!(!is_administrative_update(&payload));
            payload["update"][field] = Value::Null;
            assert!(is_administrative_update(&payload));
        }
        assert!(!is_administrative_update(&json!({
            "sessionId":"session", "update":{"sessionUpdate":"current_mode_update", "currentModeId":null}
        })));
        assert!(is_administrative_update(&json!(["session", {
            "sessionUpdate":"session_info_update", "title":42, "updatedAt":[], "_meta":false
        }])));
        assert!(is_administrative_update(&json!({
            "sessionId":"session", "update":["current_mode_update", "code", null]
        })));
    }

    #[test]
    fn nested_sequence_updates_match_the_baseline_decoder() {
        for kind in [
            "available_commands_update",
            "config_option_update",
            "current_mode_update",
            "session_info_update",
            "agent_message_chunk",
            "usage_update",
            "unrecognized_update",
        ] {
            for length in 0..=5 {
                for value in values() {
                    let mut fields = vec![json!(kind)];
                    fields.extend(std::iter::repeat_n(value, length));
                    let update = Value::Array(fields);
                    assert_baseline(&json!({"sessionId":"session", "update":update}));
                    assert_baseline(&json!(["session", update, null]));
                }
            }
        }
    }

    #[test]
    fn stored_notifications_match_the_baseline_decoder() {
        for update in updates() {
            let payload = json!({"sessionId":"session", "update":update});
            assert_baseline(&payload);
            for field in ["sessionId", "update", "_meta", "unknown"] {
                for value in values() {
                    let mut changed = payload.clone();
                    changed[field] = value;
                    assert_baseline(&changed);
                }
                if let Value::Object(mut changed) = payload.clone() {
                    changed.remove(field);
                    assert_baseline(&Value::Object(changed));
                }
            }
            for field in [
                "sessionUpdate",
                "availableCommands",
                "configOptions",
                "currentModeId",
                "title",
                "updatedAt",
                "_meta",
                "unknown",
            ] {
                for value in values() {
                    let mut changed = payload.clone();
                    changed["update"][field] = value;
                    assert_baseline(&changed);
                }
                if let Value::Object(mut changed) = update.clone() {
                    changed.remove(field);
                    assert_baseline(&json!({"sessionId":"session", "update":changed}));
                }
            }
            for metadata in values() {
                assert_baseline(&json!(["session", update, metadata]));
                assert_baseline(&json!(["session", update, metadata, "extra"]));
            }
            assert_baseline(&json!(["session", update]));
            assert_baseline(&json!(["session"]));
            assert_baseline(&json!([null, update]));
        }
        for payload in values() {
            assert_baseline(&payload);
        }
    }
}
