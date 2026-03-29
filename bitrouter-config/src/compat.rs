//! Adapter functions that convert [`ProviderConfig`] into the legacy runtime
//! types still expected by connection-layer code (`UpstreamConnection`,
//! `UpstreamAgentRegistry`, `FilesystemSkillRegistry`).
//!
//! These adapters exist to bridge the new unified provider config model to the
//! existing runtime machinery without modifying the provider crates.

use std::collections::HashMap;

use bitrouter_core::routers::upstream::{AgentConfig, ToolServerConfig, ToolServerTransport};

use crate::config::{ProviderConfig, ToolConfig};
use crate::skill::SkillConfig;

/// Convert a [`ProviderConfig`] with `api_protocol: mcp` into a
/// [`ToolServerConfig`] suitable for [`UpstreamConnection::connect`].
///
/// Returns `Err` if the provider has no `api_base` (required for the HTTP
/// transport).
///
/// # Limitations
///
/// Only HTTP transport is supported. Stdio MCP servers cannot be expressed
/// through `ProviderConfig` yet.
// TODO: support stdio transport via provider config (command/args/env fields)
pub fn provider_to_tool_server_config(
    name: &str,
    provider: &ProviderConfig,
) -> Result<ToolServerConfig, String> {
    let url = provider
        .api_base
        .as_deref()
        .ok_or_else(|| format!("MCP provider '{name}' requires api_base"))?
        .to_owned();

    let mut headers = provider.default_headers.clone().unwrap_or_default();

    if let Some(ref key) = provider.api_key {
        headers
            .entry("Authorization".to_owned())
            .or_insert_with(|| format!("Bearer {key}"));
    }

    Ok(ToolServerConfig {
        name: name.to_owned(),
        transport: ToolServerTransport::Http { url, headers },
        bridge: provider.bridge.unwrap_or(false),
        tool_filter: provider.tool_filter.clone(),
        param_restrictions: provider.param_restrictions.clone().unwrap_or_default(),
    })
}

/// Convert a [`ProviderConfig`] with `api_protocol: a2a` into an
/// [`AgentConfig`] suitable for [`UpstreamAgentRegistry::from_configs`].
///
/// Returns `Err` if the provider has no `api_base`.
pub fn provider_to_agent_config(
    name: &str,
    provider: &ProviderConfig,
) -> Result<AgentConfig, String> {
    let url = provider
        .api_base
        .as_deref()
        .ok_or_else(|| format!("A2A provider '{name}' requires api_base"))?
        .to_owned();

    let mut headers: HashMap<String, String> = provider.default_headers.clone().unwrap_or_default();

    if let Some(ref key) = provider.api_key {
        headers
            .entry("Authorization".to_owned())
            .or_insert_with(|| format!("Bearer {key}"));
    }

    Ok(AgentConfig {
        name: name.to_owned(),
        url,
        headers,
        card_path: None,
    })
}

/// Derive [`SkillConfig`] entries from tools routed to a skill provider.
///
/// Each `(tool_name, tool_config)` pair becomes one `SkillConfig` entry.
/// The `FilesystemSkillRegistry` will merge these with any skills discovered
/// from the filesystem.
pub fn tools_to_skill_configs(tools: &[(&str, &ToolConfig)]) -> Vec<SkillConfig> {
    tools
        .iter()
        .map(|(tool_name, _tool_config)| SkillConfig {
            name: (*tool_name).to_owned(),
            description: String::new(),
            source: None,
            required_apis: Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_core::routers::admin::ToolFilter;
    use bitrouter_core::routers::routing_table::ApiProtocol;

    fn mcp_provider() -> ProviderConfig {
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Mcp),
            api_base: Some("https://mcp.example.com".into()),
            api_key: Some("sk-test".into()),
            bridge: Some(true),
            tool_filter: Some(ToolFilter {
                allow: Some(vec!["search".into()]),
                deny: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn mcp_provider_converts_to_tool_server_config() {
        let config =
            provider_to_tool_server_config("my-mcp", &mcp_provider()).expect("should convert");
        assert_eq!(config.name, "my-mcp");
        assert!(config.bridge);
        assert!(config.tool_filter.is_some());
        match &config.transport {
            ToolServerTransport::Http { url, headers } => {
                assert_eq!(url, "https://mcp.example.com");
                assert_eq!(headers.get("Authorization").unwrap(), "Bearer sk-test");
            }
            _ => panic!("expected HTTP transport"),
        }
    }

    #[test]
    fn mcp_provider_without_api_base_fails() {
        let provider = ProviderConfig {
            api_protocol: Some(ApiProtocol::Mcp),
            ..Default::default()
        };
        assert!(provider_to_tool_server_config("bad", &provider).is_err());
    }

    #[test]
    fn a2a_provider_converts_to_agent_config() {
        let provider = ProviderConfig {
            api_protocol: Some(ApiProtocol::A2a),
            api_base: Some("https://agent.example.com".into()),
            api_key: Some("token-123".into()),
            ..Default::default()
        };
        let config = provider_to_agent_config("my-agent", &provider).expect("should convert");
        assert_eq!(config.name, "my-agent");
        assert_eq!(config.url, "https://agent.example.com");
        assert_eq!(
            config.headers.get("Authorization").unwrap(),
            "Bearer token-123"
        );
        assert!(config.card_path.is_none());
    }

    #[test]
    fn a2a_provider_without_api_base_fails() {
        let provider = ProviderConfig {
            api_protocol: Some(ApiProtocol::A2a),
            ..Default::default()
        };
        assert!(provider_to_agent_config("bad", &provider).is_err());
    }

    #[test]
    fn tools_to_skill_configs_produces_entries() {
        let tc = ToolConfig::default();
        let tools = vec![("review-code", &tc), ("translate", &tc)];
        let configs = tools_to_skill_configs(&tools);
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].name, "review-code");
        assert_eq!(configs[1].name, "translate");
    }

    #[test]
    fn existing_headers_not_overwritten_by_auth() {
        let provider = ProviderConfig {
            api_base: Some("https://example.com".into()),
            api_key: Some("sk-test".into()),
            default_headers: Some(HashMap::from([(
                "Authorization".into(),
                "Custom token-abc".into(),
            )])),
            ..Default::default()
        };
        let config = provider_to_tool_server_config("test", &provider).expect("should convert");
        match &config.transport {
            ToolServerTransport::Http { headers, .. } => {
                assert_eq!(
                    headers.get("Authorization").unwrap(),
                    "Custom token-abc",
                    "user-provided Authorization header should not be overwritten"
                );
            }
            _ => panic!("expected HTTP transport"),
        }
    }
}
