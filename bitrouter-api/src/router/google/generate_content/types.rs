//! Request and response types for the Google Generative AI API format.

use serde::{Deserialize, Serialize};

// ── Request ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    pub model: String,
    pub contents: Vec<GoogleContent>,
    #[serde(default)]
    pub system_instruction: Option<GoogleSystemInstruction>,
    #[serde(default)]
    pub generation_config: Option<GoogleGenerationConfig>,
    #[serde(default)]
    pub stream: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleContent {
    pub role: String,
    #[serde(default)]
    pub parts: Option<Vec<GooglePart>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleSystemInstruction {
    #[serde(default)]
    pub parts: Option<Vec<GooglePart>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GooglePart {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleGenerationConfig {
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
}

// ── Response ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    pub candidates: Vec<GenerateContentCandidate>,
    pub usage_metadata: GenerateContentUsageMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentCandidate {
    pub content: GenerateContentCandidateContent,
    pub finish_reason: String,
    pub index: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentCandidateContent {
    pub role: String,
    pub parts: Vec<GenerateContentPart>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentPart {
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentUsageMetadata {
    pub prompt_token_count: u32,
    pub candidates_token_count: u32,
    pub total_token_count: u32,
}

// ── Streaming ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentStreamChunk {
    pub candidates: Vec<GenerateContentStreamCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_metadata: Option<GenerateContentStreamUsageMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentStreamCandidate {
    pub content: GenerateContentCandidateContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    pub index: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentStreamUsageMetadata {
    pub prompt_token_count: u32,
    pub candidates_token_count: u32,
    pub total_token_count: u32,
}
