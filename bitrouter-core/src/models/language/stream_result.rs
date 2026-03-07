use std::pin::Pin;

use futures_core::Stream;

use crate::models::{
    language::stream_part::LanguageModelStreamPart,
    shared::{headers::Headers, types::JsonValue},
};

pub struct LanguageModelStreamResult {
    pub stream: Pin<Box<dyn Stream<Item = LanguageModelStreamPart> + Send>>,
    pub request: Option<LanguageModelStreamResultRequest>,
    pub response: Option<LanguageModelStreamResultResponse>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LanguageModelStreamResultRequest {
    pub body: Option<JsonValue>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LanguageModelStreamResultResponse {
    pub headers: Option<Headers>,
}
