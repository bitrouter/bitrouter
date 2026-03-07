use crate::models::{
    language::data_content::LanguageModelDataContent,
    shared::{
        provider::ProviderOptions,
        types::{JsonValue, Record},
    },
};

/// The prompt for a language model.
pub type LanguageModelPrompt = Vec<LanguageModelMessage>;

/// A message in the prompt, which can be from the system, user, assistant, or tool
#[derive(Debug, Clone)]
pub enum LanguageModelMessage {
    /// role: "system"
    System {
        /// System instructions content as text
        content: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// role: "user"
    User {
        /// The user content
        content: Vec<LanguageModelUserContent>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// role: "assistant"
    Assistant {
        /// The assistant content
        content: Vec<LanguageModelAssistantContent>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// role: "tool"
    Tool {
        /// The tool content
        content: Vec<LanguageModelToolResult>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

#[derive(Debug, Clone)]
pub enum LanguageModelUserContent {
    /// type: "text"
    Text {
        /// The text content
        text: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "file"
    File {
        /// The file name, if available
        filename: Option<String>,
        /// The file data, which can be bytes, a string, or a URL
        data: LanguageModelDataContent,
        /// The IANA media type
        media_type: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

#[derive(Debug, Clone)]
pub enum LanguageModelAssistantContent {
    /// type: "text"
    Text {
        /// The text content
        text: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "reasoning"
    Reasoning {
        /// The reasoning content as text
        text: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "file"
    File {
        /// The file name, if available
        filename: Option<String>,
        /// The file data, which can be bytes, a string, or a URL
        data: LanguageModelDataContent,
        /// The IANA media type
        media_type: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "tool-call"
    ToolCall {
        /// The tool call ID
        tool_call_id: String,
        /// The tool name
        tool_name: String,
        /// The tool input, which can be any JSON value
        input: JsonValue,
        /// Whether the tool call was executed by the provider
        provider_executed: Option<bool>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "tool-result"
    ToolResult {
        /// The tool call ID that this result corresponds to
        tool_call_id: String,
        tool_name: String,
        output: JsonValue,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

#[derive(Debug, Clone)]
pub enum LanguageModelToolResult {
    /// type: "tool-result"
    ToolResult {
        /// The tool call ID that this result corresponds to
        tool_call_id: String,
        /// The tool name
        tool_name: String,
        /// The tool output, which can be any JSON value
        output: LanguageModelToolResultOutput,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "tool-approval-response"
    ToolApprovalResponse {
        /// The approval ID that this response corresponds to
        approval_id: String,
        /// Whether the tool call was approved
        approved: bool,
        /// The reason for approval or denial, if provided
        reason: Option<String>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

#[derive(Debug, Clone)]
pub enum LanguageModelToolResultOutput {
    /// type: "text"
    Text {
        /// The text output
        value: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "json"
    Json {
        /// The JSON output as a JsonValue
        value: JsonValue,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "execution-denied"
    ExecutionDenied {
        /// The reason for the execution denial
        reason: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "error-text"
    ErrorText {
        /// The error message
        value: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "error-json"
    ErrorJson {
        /// The error details as a JsonValue
        value: JsonValue,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "content"
    Content {
        /// The content output, which can be text, file data, file URL, image data, image URL, or provider-specific content
        value: Vec<LanguageModelToolResultOutputContent>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

#[derive(Debug, Clone)]
pub enum LanguageModelToolResultOutputContent {
    /// type: "text"
    Text {
        /// The text output
        text: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "file-data"
    FileData {
        /// The file name, if available
        filename: Option<String>,
        /// Base64-encoded file data
        data: String,
        /// The IANA media type
        media_type: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "file-url"
    FileUrl {
        /// The file URL
        url: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "file-id"
    FileId {
        /// The file ID
        id: LanguageModelToolResultOutputContentFileId,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "image-data"
    ImageData {
        /// Base64-encoded image data
        data: String,
        /// The IANA media type (e.g. "image/png")
        media_type: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "image-url"
    ImageUrl {
        /// The image URL
        url: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "image-file-id"
    ImageFileId {
        /// The image file ID
        id: LanguageModelToolResultOutputContentFileId,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "provider-specific"
    ProviderSpecific {
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

/// If using multiple providers, you need to specify the provider specific ids using
/// the Record option. The key is the provider name, e.g. 'openai' or 'anthropic'.
#[derive(Debug, Clone)]
pub enum LanguageModelToolResultOutputContentFileId {
    Record(Record<String, String>),
    String(String),
}
