//! A2A v0.3.0 Message and Artifact types.
//!
//! Defines the communication primitives per the
//! [A2A v0.3.0 specification](https://a2a-protocol.org/latest/definitions/).

use serde::{Deserialize, Serialize};

/// Role of the message sender.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MessageRole {
    /// User-initiated message.
    #[serde(rename = "user")]
    User,
    /// Agent-generated message.
    #[serde(rename = "agent")]
    Agent,
}

/// A single communication turn between client and agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// Object kind — always `"message"`.
    #[serde(default = "default_message_kind")]
    pub kind: String,

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

fn default_message_kind() -> String {
    "message".to_string()
}

/// File content within a file Part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    /// Base64-encoded file bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>,

    /// URI reference to the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,

    /// File name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// MIME type (e.g., `"image/png"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Smallest unit of content within a Message or Artifact.
///
/// A2A v0.3.0 uses a tagged enum with `kind` discriminator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum Part {
    /// Plain text content.
    #[serde(rename = "text")]
    Text {
        /// The text content.
        text: String,
        /// Extension metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    /// File content (inline bytes or URI reference).
    #[serde(rename = "file")]
    File {
        /// The file content.
        file: FileContent,
        /// Extension metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    /// Structured JSON data.
    #[serde(rename = "data")]
    Data {
        /// The structured data.
        data: serde_json::Value,
        /// Extension metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
}

impl Part {
    /// Create a text part.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            metadata: None,
        }
    }

    /// Create a structured data part.
    pub fn data(data: serde_json::Value) -> Self {
        Self::Data {
            data,
            metadata: None,
        }
    }

    /// Create a file part with inline bytes and optional name/mime type.
    pub fn file_bytes(
        bytes: impl Into<String>,
        name: Option<String>,
        mime_type: Option<String>,
    ) -> Self {
        Self::File {
            file: FileContent {
                bytes: Some(bytes.into()),
                uri: None,
                name,
                mime_type,
            },
            metadata: None,
        }
    }

    /// Create a file part with a URI reference and optional name.
    pub fn file_uri(uri: impl Into<String>, name: Option<String>) -> Self {
        Self::File {
            file: FileContent {
                bytes: None,
                uri: Some(uri.into()),
                name,
                mime_type: None,
            },
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
        // v0.3.0: "kind" tag is present
        assert!(json.contains("\"kind\":\"text\""));
    }

    #[test]
    fn file_bytes_part_round_trip() {
        let part = Part::file_bytes(
            "aGVsbG8=",
            Some("test.png".to_string()),
            Some("image/png".to_string()),
        );
        let json = serde_json::to_string(&part).expect("serialize");
        let parsed: Part = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(part, parsed);
        assert!(json.contains("\"kind\":\"file\""));
        assert!(json.contains("\"bytes\""));
        assert!(json.contains("\"mimeType\""));
    }

    #[test]
    fn file_uri_part_round_trip() {
        let part = Part::file_uri("https://example.com/file.png", Some("file.png".to_string()));
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
            kind: "message".to_string(),
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
        assert!(json.contains("\"user\""));
    }

    #[test]
    fn message_role_v03_format() {
        let json = serde_json::to_string(&MessageRole::User).expect("serialize");
        assert_eq!(json, "\"user\"");

        let json = serde_json::to_string(&MessageRole::Agent).expect("serialize");
        assert_eq!(json, "\"agent\"");

        let parsed: MessageRole = serde_json::from_str("\"agent\"").expect("deserialize");
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
