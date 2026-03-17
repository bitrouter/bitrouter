//! A2A v1.0 Message and Artifact types.
//!
//! Defines the communication primitives per the
//! [A2A v1.0 specification](https://a2a-protocol.org/latest/definitions/).

use serde::{Deserialize, Serialize};

/// Role of the message sender.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Agent,
}

/// A single communication turn between client and agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// Sender role.
    pub role: MessageRole,

    /// Content parts (text, file, or structured data).
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Part {
    /// Plain text content.
    #[serde(rename = "text")]
    Text { text: String },

    /// File content (inline bytes or URI reference).
    #[serde(rename = "file")]
    File { file: FileContent },

    /// Structured JSON data.
    #[serde(rename = "data")]
    Data { data: serde_json::Value },
}

/// File content — either inline base64 bytes or a URI reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    /// File name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// MIME type (e.g., `"image/png"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,

    /// Base64-encoded file bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>,

    /// URI reference to the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
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
        let part = Part::Text {
            text: "hello world".to_string(),
        };
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
        assert!(json.contains("\"kind\":\"text\""));
    }

    #[test]
    fn file_part_round_trip() {
        let part = Part::File {
            file: FileContent {
                name: Some("test.png".to_string()),
                mime_type: Some("image/png".to_string()),
                bytes: Some("aGVsbG8=".to_string()),
                uri: None,
            },
        };
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
    }

    #[test]
    fn data_part_round_trip() {
        let part = Part::Data {
            data: serde_json::json!({"key": "value"}),
        };
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
    }

    #[test]
    fn message_round_trip() {
        let msg = Message {
            role: MessageRole::User,
            parts: vec![Part::Text {
                text: "Review this code".to_string(),
            }],
            message_id: "msg-001".to_string(),
            context_id: Some("ctx-abc".to_string()),
            task_id: None,
            reference_task_ids: Vec::new(),
            metadata: None,
        };
        let json = serde_json::to_string_pretty(&msg).expect("serialize");
        let parsed: Message = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, parsed);
    }

    #[test]
    fn artifact_round_trip() {
        let artifact = Artifact {
            artifact_id: "art-001".to_string(),
            name: Some("review-result".to_string()),
            parts: vec![Part::Text {
                text: "Looks good!".to_string(),
            }],
            metadata: None,
        };
        let json = serde_json::to_string_pretty(&artifact).expect("serialize");
        let parsed: Artifact = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(artifact, parsed);
    }
}
