use crate::models::shared::{
    provider::ProviderOptions,
    types::{JsonSchema, Record},
};

use super::{
    prompt::LanguageModelPrompt, tool::LanguageModelTool, tool_choice::LanguageModelToolChoice,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageModelCallOptions {
    pub prompt: LanguageModelPrompt,
    pub stream: Option<bool>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub stop_sequences: Option<Vec<String>>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub response_format: Option<ResponseFormat>,
    pub seed: Option<u64>,
    pub tools: Option<Vec<LanguageModelTool>>,
    pub tool_choice: Option<LanguageModelToolChoice>,
    pub include_raw_chunks: Option<bool>,
    pub abort_signal: Option<()>,
    pub headers: Option<Record<String, String>>,

    /// Provider-specific options that can be used to pass additional information to the provider or control provider-specific behavior
    pub provider_options: Option<ProviderOptions>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ResponseFormat {
    Text,
    Json {
        schema: Option<JsonSchema>,
        /// The name of the output
        name: Option<String>,
        /// Description of the object that should be generated
        description: Option<String>,
    },
}
