use http::HeaderMap;

use crate::models::{
    image::{file::ImageModelFileData, usage::ImageModelUsage},
    shared::{
        types::{Record, TimestampMillis},
        warnings::Warning,
    },
};

#[derive(Debug, Clone)]
pub struct ImageModelGenerationResult {
    pub images: Vec<ImageModelFileData>,
    pub warnings: Option<Vec<Warning>>,
    pub provider_metadata: Option<Record<String, ImageModelGenerationResultProviderMetadata>>,
    pub response: ImageModelGenerationResultResponse,
    pub usage: Option<ImageModelUsage>,
}

#[derive(Debug, Clone)]
pub struct ImageModelGenerationResultProviderMetadata {
    pub images: Vec<serde_json::Value>,
    pub extra: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct ImageModelGenerationResultResponse {
    pub timestamp: TimestampMillis,
    pub model_id: String,
    pub headers: Option<HeaderMap>,
}
