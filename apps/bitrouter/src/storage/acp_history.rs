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
                    // Frozen from the 1.10.2 decoder. The required mode is a
                    // string. The commands/options payload and metadata are permissive.
                    let accepted = match kind {
                        "available_commands_update" | "config_option_update" => {
                            (1..=2).contains(&length)
                        }
                        "current_mode_update" => (1..=2).contains(&length) && value.is_string(),
                        "session_info_update" => length <= 3,
                        _ => false,
                    };
                    let mut fields = vec![json!(kind)];
                    fields.extend(std::iter::repeat_n(value, length));
                    let update = Value::Array(fields);
                    for payload in [
                        json!({"sessionId":"session", "update":update}),
                        json!(["session", update, null]),
                    ] {
                        assert_eq!(is_administrative_update(&payload), accepted, "{payload}");
                    }
                }
            }
        }
    }

    #[test]
    fn stored_notifications_match_the_baseline_decoder() {
        for (index, update) in updates().into_iter().enumerate() {
            let setup = index < 4;
            let payload = json!({"sessionId":"session", "update":update});
            for field in ["sessionId", "update", "_meta", "unknown"] {
                for value in values().into_iter().map(Some).chain([None]) {
                    let expected = setup
                        && match field {
                            "sessionId" => value.as_ref().is_some_and(Value::is_string),
                            "update" => false,
                            _ => true,
                        };
                    let mut changed = payload.clone();
                    change(&mut changed, field, value);
                    assert_eq!(is_administrative_update(&changed), expected, "{changed}");
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
                for value in values().into_iter().map(Some).chain([None]) {
                    let expected = setup
                        && match (index, field) {
                            (_, "sessionUpdate") => false,
                            (0, "availableCommands") | (1, "configOptions") => value.is_some(),
                            (2, "currentModeId") => value.as_ref().is_some_and(Value::is_string),
                            _ => true,
                        };
                    let mut changed = payload.clone();
                    change(&mut changed["update"], field, value);
                    assert_eq!(is_administrative_update(&changed), expected, "{changed}");
                }
            }
            for metadata in values() {
                assert_eq!(
                    is_administrative_update(&json!(["session", update, metadata])),
                    setup
                );
                assert!(!is_administrative_update(&json!([
                    "session", update, metadata, "extra"
                ])));
            }
            assert_eq!(is_administrative_update(&json!(["session", update])), setup);
            assert!(!is_administrative_update(&json!(["session"])));
            assert!(!is_administrative_update(&json!([null, update])));
        }
        for payload in values() {
            assert!(!is_administrative_update(&payload));
        }
    }

    fn change(object: &mut Value, field: &str, value: Option<Value>) {
        if let Some(value) = value {
            object[field] = value;
        } else if let Some(object) = object.as_object_mut() {
            object.remove(field);
        }
    }
}
