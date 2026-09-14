//! Route a Codex ACP adapter's native model names to its configured subscription.
//! The harness header is a routing hint, never an authentication credential:
//! ordinary gateway authorization and provider credentials still apply.

use std::sync::Arc;

use bitrouter_sdk::config::ConfigRoutingTable;
use bitrouter_sdk::language_model::types::Prompt;
use bitrouter_sdk::{HeaderMap, PromptTransform};

const PROVIDER: &str = "openai-codex";

pub(crate) struct CodexRouter {
    routing_table: Arc<ConfigRoutingTable>,
}

impl CodexRouter {
    pub(crate) fn new(routing_table: Arc<ConfigRoutingTable>) -> Self {
        Self { routing_table }
    }
}

impl PromptTransform for CodexRouter {
    fn apply(&self, _prompt: &mut Prompt) {}

    fn apply_with_headers(&self, prompt: &mut Prompt, headers: &HeaderMap) {
        // Both ACP providers/set and the adapter's environment fallback carry
        // this marker. Do not infer subscription intent for generic API calls,
        // other agents, explicit routes, canonical names or preset expressions.
        if headers
            .get("x-bitrouter-harness")
            .and_then(|value| value.to_str().ok())
            != Some("codex-acp")
            || prompt.model.contains([':', '/', '@'])
        {
            return;
        }
        // Consult the live table so a reload's activation/model changes take
        // effect here too. User-defined virtual models remain authoritative.
        let config = self.routing_table.snapshot_config();
        if config.models.contains_key(&prompt.model) {
            return;
        }
        let Some(provider) = config
            .providers
            .get(PROVIDER)
            .filter(|provider| provider.active)
        else {
            return;
        };
        if provider
            .models
            .iter()
            .any(|model| model.provider_model_id.as_deref().unwrap_or(&model.id) == prompt.model)
        {
            // Preserve the native name in the adapter (and its model metadata).
            // Only gateway ingress qualifies it; normal resolution still owns
            // protocol, credential, policy and account selection.
            prompt.model = format!("{PROVIDER}:{}", prompt.model);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};
    use bitrouter_sdk::config::Config;
    use bitrouter_sdk::config::routing_table::resolve_route_chain;
    use bitrouter_sdk::language_model::routing::RoutingPrefs;

    fn config() -> Result<Config> {
        Ok(bitrouter_sdk::config::parse_with(
            r#"
inherit_defaults: false
database:
  url: "sqlite::memory:"
server:
  skip_auth: true
providers:
  openai-codex:
    api_base: https://example.invalid
    class: first-party-subscription
    models:
      - id: openai/gpt-native
        provider_model_id: gpt-native
        api_protocol: responses
"#,
            |_| None,
        )?)
    }

    fn prompt(model: &str) -> Prompt {
        Prompt {
            model: model.into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn headers(harness: &str) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert("x-bitrouter-harness", harness.parse()?);
        Ok(headers)
    }

    #[tokio::test]
    async fn assembled_app_routes_unpinned_codex_native_models() -> Result<()> {
        let config = config()?;
        // This is the first-run failure: a subscription is absent from the
        // generic cascade and its catalog id differs from the native default.
        assert!(resolve_route_chain(&config, "gpt-native", &RoutingPrefs::default()).is_err());
        let app = crate::assemble::build_app(&config).await?;
        let mut request = prompt("gpt-native");
        for transform in app.app.prompt_transforms() {
            transform.apply_with_headers(&mut request, &headers("codex-acp")?);
        }
        assert_eq!(request.model, "openai-codex:gpt-native");
        let routes = resolve_route_chain(&config, &request.model, &RoutingPrefs::default())?;
        let target = routes.first().context("no Codex route")?;
        assert_eq!(target.provider_name, "openai-codex");
        assert_eq!(target.service_id, "gpt-native");
        Ok(())
    }

    #[test]
    fn generic_requests_and_explicit_model_choices_are_unchanged() -> Result<()> {
        let config = config()?;
        let router = CodexRouter::new(Arc::new(ConfigRoutingTable::from_config(config.clone())));
        for marker in [None, Some("claude-acp"), Some("codex"), Some("CODEX-ACP")] {
            let mut request = prompt("gpt-native");
            router.apply_with_headers(
                &mut request,
                &marker.map(headers).transpose()?.unwrap_or_default(),
            );
            assert_eq!(request.model, "gpt-native");
        }
        for model in [
            "openai/gpt-native",
            "other:gpt-native",
            "gpt-native@cheap",
            "@auto",
            "gpt-unknown",
        ] {
            let mut request = prompt(model);
            router.apply_with_headers(&mut request, &headers("codex-acp")?);
            assert_eq!(request.model, model);
        }
        // Generic canonical requests still cannot silently use a subscription.
        assert!(
            resolve_route_chain(&config, "openai/gpt-native", &RoutingPrefs::default()).is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn model_changes_in_the_agent_and_daemon_reload_are_respected() -> Result<()> {
        let table = Arc::new(ConfigRoutingTable::from_config(config()?));
        let router = CodexRouter::new(Arc::clone(&table));
        let marker = headers("codex-acp")?;
        let mut updated = config()?;
        let provider = updated
            .providers
            .get_mut(PROVIDER)
            .context("missing provider")?;
        provider.models[0].provider_model_id = Some("gpt-next".into());
        table.replace_config(updated.clone()).await?;
        let mut old = prompt("gpt-native");
        router.apply_with_headers(&mut old, &marker);
        assert_eq!(old.model, "gpt-native");
        let mut next = prompt("gpt-next");
        router.apply_with_headers(&mut next, &marker);
        router.apply_with_headers(&mut next, &marker);
        assert_eq!(next.model, "openai-codex:gpt-next");
        updated
            .providers
            .get_mut(PROVIDER)
            .context("missing provider")?
            .active = false;
        table.replace_config(updated).await?;
        let mut inactive = prompt("gpt-next");
        router.apply_with_headers(&mut inactive, &marker);
        assert_eq!(inactive.model, "gpt-next");
        table.replace_config(Config::default()).await?;
        router.apply_with_headers(&mut inactive, &marker);
        assert_eq!(inactive.model, "gpt-next");
        Ok(())
    }

    #[test]
    fn explicit_virtual_models_win_over_the_native_default() -> Result<()> {
        let mut config = config()?;
        config.models.insert(
            "gpt-native".into(),
            serde_json::from_value(serde_json::json!({"endpoints": []}))?,
        );
        let router = CodexRouter::new(Arc::new(ConfigRoutingTable::from_config(config)));
        let mut request = prompt("gpt-native");
        router.apply_with_headers(&mut request, &headers("codex-acp")?);
        assert_eq!(request.model, "gpt-native");
        Ok(())
    }
}
