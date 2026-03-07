use crate::models::shared::{
    provider::ProviderMetadata,
    types::{JsonValue, TimestampMillis},
    warnings::Warning,
};

use super::{finish_reason::LanguageModelFinishReason, usage::LanguageModelUsage};

/// Represents a part of a streaming response from a language model provider.
#[derive(Debug, Clone)]
pub enum LanguageModelStreamPart {
    TextStart {
        id: String,
        provider_metadata: Option<ProviderMetadata>,
    },
    TextDelta {
        id: String,
        delta: String,
        provider_metadata: Option<ProviderMetadata>,
    },
    TextEnd {
        id: String,
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningStart {
        id: String,
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningDelta {
        id: String,
        delta: String,
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningEnd {
        id: String,
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolInputStart {
        id: String,
        tool_name: String,
        provider_executed: Option<bool>,
        dynamic: Option<bool>,
        title: Option<String>,
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolInputDelta {
        id: String,
        delta: String,
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolInputEnd {
        id: String,
        provider_metadata: Option<ProviderMetadata>,
    },
    /// type: "file"
    File {
        /// The file data as bytes
        data: Vec<u8>,
        /// The IANA media type
        media_type: String,
        /// Provider-specific metadata
        provider_metadata: Option<ProviderMetadata>,
    },
    /// type: "tool-approval-request"
    ToolApprovalRequest {
        /// The approval ID
        approval_id: String,
        /// The tool call ID
        tool_call_id: String,
        /// Provider-specific metadata
        provider_metadata: Option<ProviderMetadata>,
    },
    /// type: "url-source"
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
    StreamStart {
        warnings: Vec<Warning>,
    },
    ResponseMetadata {
        id: Option<String>,
        timestamp: Option<TimestampMillis>,
        model_id: Option<String>,
    },
    Finish {
        usage: LanguageModelUsage,
        finish_reason: LanguageModelFinishReason,
        provider_metadata: Option<ProviderMetadata>,
    },
    Raw {
        raw_value: JsonValue,
    },
    Error {
        error: JsonValue,
    },
}
