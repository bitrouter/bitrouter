//! REST client — builds `RestToolProvider` instances from provider config.

use std::collections::HashMap;
use std::sync::Arc;

use bitrouter_config::routing::ConfigToolRoutingTable;
use bitrouter_config::{ApiProtocol, AuthConfig, ProviderConfig};
use bitrouter_core::routers::registry::ToolEntry;
use bitrouter_core::tools::definition::ToolDefinition;
use bitrouter_core::tools::provider::DynToolProvider;
use bitrouter_providers::rest::provider::RestToolProvider;

/// Tool providers and discovery entries produced by REST client initialization.
pub struct RestRoutes {
    pub tool_providers: Vec<(String, Arc<DynToolProvider<'static>>)>,
    /// Tool entries for `GET /v1/tools` discovery (built from config metadata).
    pub tool_entries: Vec<ToolEntry>,
}

/// Builder for REST tool providers.
pub struct RestClient {
    providers: Vec<(String, ProviderConfig)>,
    /// REST tool configs: `(tool_name, provider_name, service_id, ToolConfig)`.
    rest_tools: Vec<(String, String, String, bitrouter_config::ToolConfig)>,
}

impl RestClient {
    pub fn new(
        providers_by_protocol: &HashMap<ApiProtocol, Vec<(String, ProviderConfig)>>,
        tool_table: &ConfigToolRoutingTable,
    ) -> Self {
        let providers = providers_by_protocol
            .get(&ApiProtocol::Rest)
            .cloned()
            .unwrap_or_default();

        // Collect REST-protocol tools for discovery entries.
        let rest_provider_names: std::collections::HashSet<&str> =
            providers.iter().map(|(n, _)| n.as_str()).collect();

        let rest_tools = tool_table
            .tools()
            .iter()
            .filter_map(|(tool_name, tool_config)| {
                // Find the first REST endpoint for this tool.
                let ep = tool_config.endpoints.iter().find(|ep| {
                    let provider = tool_table.providers().get(&ep.provider);
                    let protocol = ep.api_protocol.or(provider.and_then(|p| p.api_protocol));
                    protocol == Some(ApiProtocol::Rest)
                        || (protocol.is_none()
                            && rest_provider_names.contains(ep.provider.as_str()))
                })?;
                Some((
                    tool_name.clone(),
                    ep.provider.clone(),
                    ep.service_id.clone(),
                    tool_config.clone(),
                ))
            })
            .collect();

        Self {
            providers,
            rest_tools,
        }
    }

    pub fn build(self) -> RestRoutes {
        let client = Arc::new(reqwest::Client::new());

        let tool_providers = self
            .providers
            .into_iter()
            .filter_map(|(name, config)| {
                let api_base = config.api_base.clone()?;
                let auth_header = resolve_auth_header(&config);

                let provider =
                    RestToolProvider::new(name.clone(), api_base, auth_header, client.clone());
                let dyn_provider: Arc<DynToolProvider<'static>> =
                    Arc::from(DynToolProvider::new_box(provider));
                Some((name, dyn_provider))
            })
            .collect();

        // Build ToolEntry for each REST tool so they appear in GET /v1/tools.
        let tool_entries = self
            .rest_tools
            .into_iter()
            .map(|(tool_name, provider, service_id, config)| {
                let input_schema = config
                    .input_schema
                    .and_then(|v| serde_json::from_value(v).ok());
                ToolEntry {
                    id: format!("{provider}/{service_id}"),
                    provider,
                    definition: ToolDefinition {
                        name: tool_name,
                        description: config.description,
                        input_schema,
                        annotations: None,
                        input_examples: Vec::new(),
                    },
                }
            })
            .collect();

        RestRoutes {
            tool_providers,
            tool_entries,
        }
    }
}

/// Resolve the auth header from a provider config.
///
/// Priority: `auth` config (with resolved api_key) > `api_key` field (default
/// to Bearer). When the `auth.api_key` is an unsubstituted env var placeholder,
/// falls back to the provider-level `api_key` (which is resolved by
/// `env_prefix` during config loading).
pub(crate) fn resolve_auth_header(config: &ProviderConfig) -> Option<(String, String)> {
    match config.auth.as_ref() {
        Some(AuthConfig::Bearer { api_key }) => {
            let key = resolve_key(api_key, config);
            Some(("Authorization".to_owned(), format!("Bearer {key}")))
        }
        Some(AuthConfig::Header {
            header_name,
            api_key,
        }) => {
            let key = resolve_key(api_key, config);
            Some((header_name.clone(), key))
        }
        Some(AuthConfig::X402 | AuthConfig::Mpp | AuthConfig::Custom { .. }) => None,
        None => {
            // Fall back to api_key field as Bearer token.
            config
                .api_key
                .as_ref()
                .map(|key| ("Authorization".to_owned(), format!("Bearer {key}")))
        }
    }
}

/// If the key is an unsubstituted env var placeholder (e.g. `"${EXA_API_KEY}"`),
/// fall back to the provider-level resolved `api_key`.
pub(crate) fn resolve_key(auth_key: &str, config: &ProviderConfig) -> String {
    if auth_key.starts_with("${") && auth_key.ends_with('}') {
        config
            .api_key
            .clone()
            .unwrap_or_else(|| auth_key.to_owned())
    } else {
        auth_key.to_owned()
    }
}
