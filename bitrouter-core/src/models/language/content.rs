use crate::models::shared::{provider::ProviderMetadata, types::JsonValue};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LanguageModelContent {
    /// type: "text"
    #[serde(rename_all = "camelCase")]
    Text {
        /// The text content
        text: String,
        /// Provider-specific metadata
        provider_metadata: Option<ProviderMetadata>,
    },
    /// type: "reasoning"
    #[serde(rename_all = "camelCase")]
    Reasoning {
        /// The reasoning content as text
        text: String,
        /// Provider-specific metadata
        provider_metadata: Option<ProviderMetadata>,
    },
    /// type: "file"
    #[serde(rename_all = "camelCase")]
    File {
        /// The file data as bytes
        data: Vec<u8>,
        /// The IANA media type
        media_type: String,
        /// Provider-specific metadata
        provider_metadata: Option<ProviderMetadata>,
    },
    /// type: "tool-approval-request"
    #[serde(rename_all = "camelCase")]
    ToolApprovalRequest {
        /// The approval ID
        approval_id: String,
        /// The tool call ID
        tool_call_id: String,
        /// Provider-specific metadata
        provider_metadata: Option<ProviderMetadata>,
    },
    /// type: "url-source"
    #[serde(rename_all = "camelCase")]
    UrlSource {
        /// The URL source ID
        id: String,
        /// The URL
        url: String,
        /// The title of the URL content, if available
        title: Option<String>,
        /// Provider-specific metadata
        provider_metadata: Option<ProviderMetadata>,
    },
    /// type: "document-source"
    #[serde(rename_all = "camelCase")]
    DocumentSource {
        /// The document source ID
        id: String,
        /// The IANA media type
        media_type: String,
        /// The title of the document
        title: String,
        /// The filename of the document, if available
        filename: Option<String>,
        /// Provider-specific metadata
        provider_metadata: Option<ProviderMetadata>,
    },
    /// type: "tool-call"
    #[serde(rename_all = "camelCase")]
    ToolCall {
        /// The tool call ID
        tool_call_id: String,
        /// The tool name
        tool_name: String,
        /// The stringified tool input
        tool_input: String,
        /// Whether the tool call was executed by the provider
        provider_executed: Option<bool>,
        /// Whether the tool call is defined at runtime
        dynamic: Option<bool>,
        /// Provider-specific metadata
        provider_metadata: Option<ProviderMetadata>,
    },
    /// type: "tool-result"
    #[serde(rename_all = "camelCase")]
    ToolResult {
        /// The tool call ID
        tool_call_id: String,
        /// The tool name
        tool_name: String,
        /// The tool result content
        result: JsonValue,
        /// Optional flag if the result is an error
        is_error: Option<bool>,
        /// Preliminary tool results replace each other, e.g. image previews.
        /// There always has to be a final, non-preliminary tool result.
        preliminary: Option<bool>,
        /// Whether the tool call is defined at runtime
        dynamic: Option<bool>,
        /// Provider-specific metadata
        provider_metadata: Option<ProviderMetadata>,
    },
}
