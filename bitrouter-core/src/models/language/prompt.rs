use crate::models::{
    language::data_content::LanguageModelDataContent,
    shared::{
        provider::ProviderOptions,
        types::{JsonValue, Record},
    },
};

/// The prompt for a language model, which is a sequence of messages from the system, user, assistant, and tools
pub type LanguageModelPrompt = Vec<LanguageModelMessage>;

/// A message in the prompt, which can be from the system, user, assistant, or tool
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "kebab-case")]
pub enum LanguageModelMessage {
    /// role: "system"
    #[serde(rename_all = "camelCase")]
    System {
        /// System instructions content as text
        content: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// role: "user"
    #[serde(rename_all = "camelCase")]
    User {
        /// The user content
        content: Vec<LanguageModelUserContent>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// role: "assistant"
    #[serde(rename_all = "camelCase")]
    Assistant {
        /// The assistant content
        content: Vec<LanguageModelAssistantContent>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// role: "tool"
    #[serde(rename_all = "camelCase")]
    Tool {
        /// The tool content
        content: Vec<LanguageModelToolResult>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LanguageModelUserContent {
    /// type: "text"
    #[serde(rename_all = "camelCase")]
    Text {
        /// The text content
        text: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "file"
    #[serde(rename_all = "camelCase")]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LanguageModelAssistantContent {
    /// type: "text"
    #[serde(rename_all = "camelCase")]
    Text {
        /// The text content
        text: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "reasoning"
    #[serde(rename_all = "camelCase")]
    Reasoning {
        /// The reasoning content as text
        text: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "file"
    #[serde(rename_all = "camelCase")]
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
    #[serde(rename_all = "camelCase")]
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
    #[serde(rename_all = "camelCase")]
    ToolResult {
        /// The tool call ID that this result corresponds to
        tool_call_id: String,
        tool_name: String,
        output: JsonValue,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LanguageModelToolResult {
    /// type: "tool-result"
    #[serde(rename_all = "camelCase")]
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
    #[serde(rename_all = "camelCase")]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LanguageModelToolResultOutput {
    /// type: "text"
    #[serde(rename_all = "camelCase")]
    Text {
        /// The text output
        value: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "json"
    #[serde(rename_all = "camelCase")]
    Json {
        /// The JSON output as a JsonValue
        value: JsonValue,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "execution-denied"
    #[serde(rename_all = "camelCase")]
    ExecutionDenied {
        /// The reason for the execution denial
        reason: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "error-text"
    #[serde(rename_all = "camelCase")]
    ErrorText {
        /// The error message
        value: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "error-json"
    #[serde(rename_all = "camelCase")]
    ErrorJson {
        /// The error details as a JsonValue
        value: JsonValue,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "content"
    #[serde(rename_all = "camelCase")]
    Content {
        /// The content output, which can be text, file data, file URL, image data, image URL, or provider-specific content
        value: Vec<LanguageModelToolResultOutputContent>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LanguageModelToolResultOutputContent {
    /// type: "text"
    #[serde(rename_all = "camelCase")]
    Text {
        /// The text output
        text: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "file-data"
    #[serde(rename_all = "camelCase")]
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
    #[serde(rename_all = "camelCase")]
    FileUrl {
        /// The file URL
        url: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "file-id"
    #[serde(rename_all = "camelCase")]
    FileId {
        /// The file ID
        id: LanguageModelToolResultOutputContentFileId,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "image-data"
    #[serde(rename_all = "camelCase")]
    ImageData {
        /// Base64-encoded image data
        data: String,
        /// The IANA media type (e.g. "image/png")
        media_type: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "image-url"
    #[serde(rename_all = "camelCase")]
    ImageUrl {
        /// The image URL
        url: String,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "image-file-id"
    #[serde(rename_all = "camelCase")]
    ImageFileId {
        /// The image file ID
        id: LanguageModelToolResultOutputContentFileId,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "provider-specific"
    #[serde(rename_all = "camelCase")]
    ProviderSpecific {
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

/// If you use multiple providers, you need to specify the provider specific ids using
/// the Record option. The key is the provider name, e.g. 'openai' or 'anthropic'.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LanguageModelToolResultOutputContentFileId {
    Record(Record<String, String>),
    String(String),
}
