//! Tests for A2A type serialization and wire format compliance.
//!
//! Filter integration tests require a running upstream agent and are
//! covered by the `tests/` integration test suite.

use bitrouter_a2a::types::*;

// ── v0.3.0 wire format validation ────────────────────────────────

#[test]
fn part_types_serialize_with_kind_tag() {
    let text_part = Part::text("hello");
    let json = serde_json::to_value(&text_part).unwrap_or_default();
    assert_eq!(json["kind"], "text");
    assert_eq!(json["text"], "hello");

    let data_part = Part::data(serde_json::json!({"key": "value"}));
    let json = serde_json::to_value(&data_part).unwrap_or_default();
    assert_eq!(json["kind"], "data");
    assert!(json["data"].is_object());

    let file_part = Part::file_uri("https://example.com/f.png", Some("f.png".to_string()));
    let json = serde_json::to_value(&file_part).unwrap_or_default();
    assert_eq!(json["kind"], "file");
    assert!(json["file"].is_object());
    assert_eq!(json["file"]["uri"], "https://example.com/f.png");
}

#[test]
fn task_state_serializes_lowercase() {
    let cases = vec![
        (TaskState::Submitted, "submitted"),
        (TaskState::Working, "working"),
        (TaskState::Completed, "completed"),
        (TaskState::Failed, "failed"),
        (TaskState::Canceled, "canceled"),
        (TaskState::Rejected, "rejected"),
        (TaskState::InputRequired, "input-required"),
        (TaskState::AuthRequired, "auth-required"),
        (TaskState::Unknown, "unknown"),
    ];
    for (state, expected) in cases {
        let json = serde_json::to_value(&state).unwrap_or_default();
        assert_eq!(
            json.as_str().unwrap_or_default(),
            expected,
            "TaskState::{state:?} should serialize as \"{expected}\""
        );
    }
}

#[test]
fn message_role_serializes_lowercase() {
    let json = serde_json::to_value(&MessageRole::User).unwrap_or_default();
    assert_eq!(json, "user");
    let json = serde_json::to_value(&MessageRole::Agent).unwrap_or_default();
    assert_eq!(json, "agent");
}

#[test]
fn agent_card_round_trips_through_json() {
    let card = minimal_card(
        "test-agent",
        "A test agent",
        "1.0.0",
        "http://localhost/a2a/test-agent",
    );
    let json = serde_json::to_string(&card).expect("serialize");
    let parsed: AgentCard = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.name, "test-agent");
    assert_eq!(parsed.protocol_version, "0.3.0");
}
