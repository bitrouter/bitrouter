//! A2A v1.0 Message and Artifact types.
//!
//! Defines the communication primitives per the
//! [A2A v1.0 specification](https://a2a-protocol.org/latest/definitions/).

use serde::{Deserialize, Serialize};

/// Role of the message sender.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MessageRole {
    /// User-initiated message.
    #[serde(rename = "ROLE_USER")]
    User,
    /// Agent-generated message.
    #[serde(rename = "ROLE_AGENT")]
    Agent,
}

/// A single communication turn between client and agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// Sender role.
    pub role: MessageRole,

    /// Content parts.
    pub parts: Vec<Part>,

    /// Unique message identifier.
    pub message_id: String,

    /// Logical conversation grouping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,

    /// Associated task identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,

    /// IDs of tasks this message references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_task_ids: Vec<String>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Smallest unit of content within a Message or Artifact.
///
/// A2A v1.0 uses a flat structure where the presence of `text`, `raw`,
/// `data`, or `url` discriminates the content type. Shared optional
/// fields (`filename`, `media_type`, `metadata`) apply to all variants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    /// Plain text content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    /// Structured JSON data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,

    /// Base64-encoded raw bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,

    /// URL reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// File name (applicable to `raw` and `url` parts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,

    /// MIME type (e.g., `"image/png"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl Part {
    /// Create a text part.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            data: None,
            raw: None,
            url: None,
            filename: None,
            media_type: None,
            metadata: None,
        }
    }

    /// Create a structured data part.
    pub fn data(data: serde_json::Value) -> Self {
        Self {
            text: None,
            data: Some(data),
            raw: None,
            url: None,
            filename: None,
            media_type: None,
            metadata: None,
        }
    }

    /// Create a raw bytes part with optional filename and media type.
    pub fn raw(
        raw: impl Into<String>,
        filename: Option<String>,
        media_type: Option<String>,
    ) -> Self {
        Self {
            text: None,
            data: None,
            raw: Some(raw.into()),
            url: None,
            filename,
            media_type,
            metadata: None,
        }
    }

    /// Create a URL reference part with optional filename.
    pub fn url(url: impl Into<String>, filename: Option<String>) -> Self {
        Self {
            text: None,
            data: None,
            raw: None,
            url: Some(url.into()),
            filename,
            media_type: None,
            metadata: None,
        }
    }
}

/// An output deliverable produced by an agent task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    /// Unique artifact identifier.
    pub artifact_id: String,

    /// Human-readable artifact name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Human-readable artifact description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Content parts composing this artifact.
    pub parts: Vec<Part>,

    /// Extension metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_part_round_trip() {
        let part = Part::text("hello world");
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
        assert!(json.contains("\"text\":\"hello world\""));
        // v1.0: no "kind" tag
        assert!(!json.contains("kind"));
    }

    #[test]
    fn raw_part_round_trip() {
        let part = Part::raw(
            "aGVsbG8=",
            Some("test.png".to_string()),
            Some("image/png".to_string()),
        );
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
        assert!(json.contains("\"raw\""));
        assert!(json.contains("\"filename\""));
        assert!(json.contains("\"mediaType\""));
    }

    #[test]
    fn url_part_round_trip() {
        let part = Part::url("https://example.com/file.png", Some("file.png".to_string()));
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
    }

    #[test]
    fn data_part_round_trip() {
        let part = Part::data(serde_json::json!({"key": "value"}));
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
    }

    #[test]
    fn message_round_trip() {
        let msg = Message {
            role: MessageRole::User,
            parts: vec![Part::text("Review this code")],
            message_id: "msg-001".to_string(),
            context_id: Some("ctx-abc".to_string()),
            task_id: None,
            reference_task_ids: Vec::new(),
            metadata: None,
        };
        let json = serde_json::to_string_pretty(&msg).expect("serialize");
        let parsed: Message = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, parsed);
        assert!(json.contains("ROLE_USER"));
    }

    #[test]
    fn message_role_v1_format() {
        let json = serde_json::to_string(&MessageRole::User).expect("serialize");
        assert_eq!(json, "\"ROLE_USER\"");

        let json = serde_json::to_string(&MessageRole::Agent).expect("serialize");
        assert_eq!(json, "\"ROLE_AGENT\"");

        let parsed: MessageRole = serde_json::from_str("\"ROLE_AGENT\"").expect("deserialize");
        assert_eq!(parsed, MessageRole::Agent);
    }

    #[test]
    fn artifact_round_trip() {
        let artifact = Artifact {
            artifact_id: "art-001".to_string(),
            name: Some("review-result".to_string()),
            description: None,
            parts: vec![Part::text("Looks good!")],
            metadata: None,
        };
        let json = serde_json::to_string_pretty(&artifact).expect("serialize");
        let parsed: Artifact = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(artifact, parsed);
    }
}
