use crate::models::shared::types::JsonValue;

/// Represents the token usage information for a language model call.
#[derive(Debug, Clone)]
pub struct LanguageModelUsage {
    pub input_tokens: LanguageModelInputTokens,
    pub output_tokens: LanguageModelOutputTokens,
    pub raw: Option<JsonValue>,
}

/// Represents the token usage information for the input to a language model call.
#[derive(Debug, Clone)]
pub struct LanguageModelInputTokens {
    pub total: Option<u32>,
    pub no_cache: Option<u32>,
    pub cache_read: Option<u32>,
    pub cache_write: Option<u32>,
}

/// Represents the token usage information for the output from a language model call.
#[derive(Debug, Clone)]
pub struct LanguageModelOutputTokens {
    pub total: Option<u32>,
    pub text: Option<u32>,
    pub reasoning: Option<u32>,
}
