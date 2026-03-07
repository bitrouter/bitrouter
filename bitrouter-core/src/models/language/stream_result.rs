use std::pin::Pin;

use futures_core::Stream;

use crate::models::{
    language::stream_part::LanguageModelStreamPart,
    shared::{headers::Headers, types::JsonValue},
};

/// Represents the result of a streaming language model call.
pub struct LanguageModelStreamResult {
    /// The stream of partial results from the language model provider.
    pub stream: Pin<Box<dyn Stream<Item = LanguageModelStreamPart> + Send>>,
    /// The request sent to the language model provider.
    pub request: Option<LanguageModelStreamResultRequest>,
    /// The response received from the language model provider.
    pub response: Option<LanguageModelStreamResultResponse>,
}

/// Represents the request sent to a language model provider for a streaming call.
#[derive(Debug, Clone)]
pub struct LanguageModelStreamResultRequest {
    pub headers: Option<Headers>,
    pub body: Option<JsonValue>,
}

/// Represents the response received from a language model provider for a streaming call.
#[derive(Debug, Clone)]
pub struct LanguageModelStreamResultResponse {
    pub headers: Option<Headers>,
}
