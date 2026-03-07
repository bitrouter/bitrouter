use regex::Regex;

use crate::models::shared::types::Record;

use super::{
    call_options::LanguageModelCallOptions, generate_result::LanguageModelGenerateResult,
    stream_result::LanguageModelStreamResult,
};

pub trait LanguageModel {
    const PROVIDER_NAME: &'static str;
    const MODEL_ID: &'static str;

    /// Media type -> Regex for supported URLs of that media type
    fn supported_urls(&self) -> impl Future<Output = Record<String, Regex>>;

    fn generate(
        &self,
        options: LanguageModelCallOptions,
    ) -> impl Future<Output = LanguageModelGenerateResult>;

    fn stream(
        &self,
        options: LanguageModelCallOptions,
    ) -> impl Future<Output = LanguageModelStreamResult>;
}
