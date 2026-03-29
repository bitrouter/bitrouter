use serde::{Deserialize, Serialize};

/// The result of invoking a tool, regardless of underlying protocol.
///
/// This is the tool equivalent of [`LanguageModelGenerateResult`] — a
/// protocol-neutral representation that MCP, A2A, REST, and future tool
/// protocols all map into.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    /// Content blocks returned by the tool.
    pub content: Vec<ToolContent>,
    /// Whether the tool invocation resulted in an error.
    #[serde(default)]
    pub is_error: bool,
    /// Protocol-specific metadata (e.g. A2A task ID, HTTP status code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// A single content block in a tool call result.
///
/// Designed to be protocol-agnostic while covering the content types that
/// MCP, A2A, and REST APIs commonly return.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolContent {
    /// Plain text content.
    Text { text: String },
    /// Structured JSON data.
    Json { data: serde_json::Value },
    /// Base64-encoded image content.
    Image { data: String, mime_type: String },
    /// A resource reference, optionally with inline text.
    Resource {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_content_round_trips() {
        let result = ToolCallResult {
            content: vec![ToolContent::Text {
                text: "hello".into(),
            }],
            is_error: false,
            metadata: None,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let back: ToolCallResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.content.len(), 1);
        assert!(!back.is_error);
    }

    #[test]
    fn error_result_with_metadata() {
        let result = ToolCallResult {
            content: vec![ToolContent::Text {
                text: "not found".into(),
            }],
            is_error: true,
            metadata: Some(serde_json::json!({"status": 404})),
        };
        assert!(result.is_error);
        assert!(result.metadata.is_some());
    }

    #[test]
    fn all_content_variants_serialize() {
        let contents = vec![
            ToolContent::Text { text: "t".into() },
            ToolContent::Json {
                data: serde_json::json!({"k": "v"}),
            },
            ToolContent::Image {
                data: "base64data".into(),
                mime_type: "image/png".into(),
            },
            ToolContent::Resource {
                uri: "file:///a.txt".into(),
                text: Some("contents".into()),
            },
        ];
        let json = serde_json::to_string(&contents).expect("serialize");
        let back: Vec<ToolContent> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.len(), 4);
    }
}
