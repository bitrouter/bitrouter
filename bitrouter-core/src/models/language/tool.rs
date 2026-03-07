use crate::models::shared::{
    provider::ProviderOptions,
    types::{JsonSchema, JsonValue, Record},
};

/// The definition of tools that can be used by language models during generation.
#[derive(Debug, Clone)]
pub enum LanguageModelTool {
    /// type: "function"
    Function {
        name: String,
        description: Option<String>,
        input_schema: JsonSchema,
        input_examples: Vec<LanguageModelFunctionToolInputExample>,
        strict: Option<bool>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "provider"
    Provider {
        id: ProviderToolId,
        name: String,
        args: Record<String, JsonValue>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

/// Represents an example input for a function tool.
#[derive(Debug, Clone)]
pub struct LanguageModelFunctionToolInputExample {
    pub input: JsonValue,
}

/// Represents the unique identifier for a provider tool, consisting of the provider name and tool ID.
#[derive(Debug, Clone)]
pub struct ProviderToolId {
    pub provider_name: String,
    pub tool_id: String,
}
