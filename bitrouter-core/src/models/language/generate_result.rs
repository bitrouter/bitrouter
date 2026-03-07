use crate::models::shared::{
    provider::ProviderMetadata,
    types::{JsonValue, Record, TimestampMillis},
    warnings::Warning,
};

use super::{
    content::LanguageModelContent, finish_reason::LanguageModelFinishReason,
    usage::LanguageModelUsage,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LanguageModelGenerateResult {
    /// The generated content
    pub content: LanguageModelContent,
    /// The finish reason, if the generation is complete
    pub finish_reason: LanguageModelFinishReason,
    pub usage: LanguageModelUsage,
    pub provider_metadata: Option<ProviderMetadata>,
    pub request: Option<LanguageModelRequest>,
    pub response_metadata: Option<LanguageModelResponse>,
    pub warnings: Option<Vec<Warning>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LanguageModelRequest {
    pub body: JsonValue,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageModelResponse {
    pub id: Option<String>,
    pub timestamp: Option<TimestampMillis>,
    pub model_id: Option<String>,
    pub headers: Option<Record<String, String>>,
    pub body: Option<JsonValue>,
}
