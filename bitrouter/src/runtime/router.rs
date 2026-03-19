use std::collections::HashMap;

use bitrouter_config::{ApiProtocol, ProviderConfig};
use bitrouter_core::{
    errors::{BitrouterError, Result},
    models::language::language_model::DynLanguageModel,
    routers::{model_router::LanguageModelRouter, routing_table::RoutingTarget},
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest_middleware::ClientWithMiddleware;

/// A model router backed by `reqwest` that instantiates concrete provider
/// model objects on demand from [`ProviderConfig`] entries.
pub struct Router {
    client: ClientWithMiddleware,
    providers: HashMap<String, ProviderConfig>,
}

impl Router {
    pub fn new(client: ClientWithMiddleware, providers: HashMap<String, ProviderConfig>) -> Self {
        Self { client, providers }
    }

    fn build_openai_config(&self, provider: &ProviderConfig) -> Result<OpenAiConfig> {
        let api_key = provider.api_key.clone().unwrap_or_default();
        let base_url = provider
            .api_base
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".into());

        let default_headers = parse_headers(provider.default_headers.as_ref())?;

        Ok(OpenAiConfig {
            api_key,
            base_url,
            organization: None,
            project: None,
            default_headers,
        })
    }

    fn build_anthropic_config(&self, provider: &ProviderConfig) -> Result<AnthropicConfig> {
        let api_key = provider.api_key.clone().unwrap_or_default();
        let base_url = provider
            .api_base
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com".into());

        let default_headers = parse_headers(provider.default_headers.as_ref())?;

        Ok(AnthropicConfig {
            api_key,
            base_url,
            api_version: "2023-06-01".into(),
            default_headers,
        })
    }

    fn build_google_config(&self, provider: &ProviderConfig) -> Result<GoogleConfig> {
        let api_key = provider.api_key.clone().unwrap_or_default();
        let base_url = provider
            .api_base
            .clone()
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com".into());

        let default_headers = parse_headers(provider.default_headers.as_ref())?;

        Ok(GoogleConfig {
            api_key,
            base_url,
            default_headers,
        })
    }
}

impl LanguageModelRouter for Router {
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        let provider = self.providers.get(&target.provider_name).ok_or_else(|| {
            BitrouterError::invalid_request(
                None,
                format!("unknown provider: {}", target.provider_name),
                None,
            )
        })?;

        let protocol = provider.api_protocol.as_ref().ok_or_else(|| {
            BitrouterError::invalid_request(
                Some(&target.provider_name),
                format!(
                    "provider '{}' has no api_protocol configured",
                    target.provider_name
                ),
                None,
            )
        })?;

        match protocol {
            ApiProtocol::Openai => {
                let config = self.build_openai_config(provider)?;
                let model = OpenAiChatCompletionsModel::with_client(
                    target.model_id,
                    self.client.clone(),
                    config,
                );
                Ok(DynLanguageModel::new_box(model))
            }
            ApiProtocol::Anthropic => {
                let config = self.build_anthropic_config(provider)?;
                let model = AnthropicMessagesModel::with_client(
                    target.model_id,
                    self.client.clone(),
                    config,
                );
                Ok(DynLanguageModel::new_box(model))
            }
            ApiProtocol::Google => {
                let config = self.build_google_config(provider)?;
                let model = GoogleGenerativeAiModel::with_client(
                    target.model_id,
                    self.client.clone(),
                    config,
                );
                Ok(DynLanguageModel::new_box(model))
            }
        }
    }
}

fn parse_headers(headers: Option<&HashMap<String, String>>) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    if let Some(h) = headers {
        for (k, v) in h {
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
                BitrouterError::invalid_request(
                    None,
                    format!("invalid header name '{k}': {e}"),
                    None,
                )
            })?;
            let value = HeaderValue::from_str(v).map_err(|e| {
                BitrouterError::invalid_request(
                    None,
                    format!("invalid header value for '{k}': {e}"),
                    None,
                )
            })?;
            map.insert(name, value);
        }
    }
    Ok(map)
}

// Re-export provider types under short aliases for readability.
use bitrouter_anthropic::messages::provider::{AnthropicConfig, AnthropicMessagesModel};
use bitrouter_google::generate_content::provider::{GoogleConfig, GoogleGenerativeAiModel};
use bitrouter_openai::chat::provider::{OpenAiChatCompletionsModel, OpenAiConfig};
