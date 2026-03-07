use crate::models::shared::types::JsonValue;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageModelUsage {
    pub input_tokens: LanguageModelInputTokens,
    pub output_tokens: LanguageModelOutputTokens,
    pub raw: Option<JsonValue>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageModelInputTokens {
    pub total: Option<u32>,
    pub no_cache: Option<u32>,
    pub cache_read: Option<u32>,
    pub cache_write: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageModelOutputTokens {
    pub total: Option<u32>,
    pub text: Option<u32>,
    pub reasoning: Option<u32>,
}
