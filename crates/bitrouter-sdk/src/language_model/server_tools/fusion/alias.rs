//! The `bitrouter/fusion` model alias — the bitrouter analog of OpenRouter's
//! Fusion Router.
//!
//! A request addressed to the alias is rewritten, before the pipeline sees it,
//! into a normal request on a default outer model carrying a `bitrouter:fusion`
//! declaration. The declaration then flows through the ordinary
//! declaration → hook → toolset path. This is an ingress transform (the
//! pipeline context exposes the prompt read-only), so a consumer calls
//! [`FusionAliasConfig::apply`] while building the request.
//!
//! The model is *nudged* toward the tool via the system prompt rather than
//! forced via `tool_choice`: the server-tool loop reuses `tool_choice` across
//! turns, so forcing it would re-trigger the deliberation every iteration.
//!
//! Reference: <https://openrouter.ai/docs/guides/features/server-tools/fusion>

use super::config::{FUSION_TOOL, FusionSettings};
use crate::error::BitrouterError;
use crate::language_model::types::{Prompt, ProviderMetadata, Tool};

const DEFAULT_FUSION_ALIAS: &str = "bitrouter/fusion";

fn conflicts_with_router_address(alias: &str) -> bool {
    alias.starts_with('@')
        || alias.starts_with("bitrouter:")
        || (alias.starts_with("bitrouter/") && alias != DEFAULT_FUSION_ALIAS)
}

/// The defaults the alias expands to (the "Quality" preset by default).
#[derive(Clone, Debug)]
pub struct FusionAliasConfig {
    /// The model slug that triggers the alias (e.g. `bitrouter/fusion`).
    pub alias: String,
    /// The model the alias resolves the request to.
    pub outer_model: String,
    /// Default panel models.
    pub panel: Vec<String>,
    /// Default judge model.
    pub judge: String,
    /// Optional dedicated synthesizer model.
    pub synthesizer: Option<String>,
    /// Provider web tools forwarded to every panel member (web_search/fetch),
    /// in provider-namespaced declaration form.
    pub web_tools: Vec<serde_json::Value>,
}

impl FusionAliasConfig {
    /// Reject an alias that would run before and shadow preset or BitRouter
    /// model-address resolution. The official `bitrouter/fusion` alias is the
    /// only transform allowed to claim the reserved namespace.
    pub fn validate_settings(settings: &FusionSettings) -> crate::Result<()> {
        let alias = settings.alias.as_deref().unwrap_or(DEFAULT_FUSION_ALIAS);
        if conflicts_with_router_address(alias) {
            return Err(BitrouterError::bad_request(format!(
                "server_tools.fusion.alias '{alias}' conflicts with reserved model addressing; \
                 use '{DEFAULT_FUSION_ALIAS}' or a non-reserved model name"
            )));
        }
        Ok(())
    }

    /// Build an alias config from the `server_tools.fusion` settings, applying
    /// defaults: alias = `bitrouter/fusion`; outer model = configured, else the
    /// judge, else the first panel model; judge = configured, else the first
    /// panel model, else the outer model; panel = configured, else the outer
    /// model alone. Returns `None` when nothing identifies an outer model (no
    /// outer_model, judge, or panel) — i.e. Fusion is effectively unconfigured.
    pub fn from_settings(settings: &FusionSettings) -> Option<Self> {
        // Keep direct SDK construction safe as well as app assembly. The app
        // calls `validate_settings` first so operators receive the diagnostic;
        // this guard prevents callers that only use this Option-returning
        // convenience from constructing a transform that can hijack routing.
        Self::validate_settings(settings).ok()?;
        let outer_model = settings
            .outer_model
            .clone()
            .or_else(|| settings.judge.clone())
            .or_else(|| settings.panel.first().cloned())?;
        let judge = settings
            .judge
            .clone()
            .or_else(|| settings.panel.first().cloned())
            .unwrap_or_else(|| outer_model.clone());
        let panel = if settings.panel.is_empty() {
            vec![outer_model.clone()]
        } else {
            settings.panel.clone()
        };
        Some(FusionAliasConfig {
            alias: settings
                .alias
                .clone()
                .unwrap_or_else(|| DEFAULT_FUSION_ALIAS.to_string()),
            outer_model,
            panel,
            judge,
            synthesizer: settings.synthesizer.clone(),
            web_tools: settings.web_tools.clone(),
        })
    }

    /// Rewrite a prompt addressed to the alias: swap in the outer model, inject
    /// the `bitrouter:fusion` declaration, and nudge the model toward it. Returns
    /// `true` when the alias matched and the prompt was rewritten.
    pub fn apply(&self, prompt: &mut Prompt) -> bool {
        if conflicts_with_router_address(&self.alias) || prompt.model != self.alias {
            return false;
        }
        prompt.model = self.outer_model.clone();
        prompt.tools.push(self.declaration());
        let nudge = "This request uses multi-model deliberation. Call the `fusion` \
                     tool once with the user's question as `prompt`, then write your \
                     final answer grounded in the returned analysis.";
        prompt.system = Some(match prompt.system.take() {
            Some(existing) => format!("{existing}\n\n{nudge}"),
            None => nudge.to_string(),
        });
        true
    }

    fn declaration(&self) -> Tool {
        let panel: Vec<serde_json::Value> = self
            .panel
            .iter()
            .map(|m| serde_json::json!({ "model": m, "tools": self.web_tools }))
            .collect();
        let mut args = serde_json::json!({
            "panel": panel,
            "judge": { "model": self.judge },
        });
        if let Some(synth) = &self.synthesizer {
            args["synthesizer"] = serde_json::json!(synth);
        }
        Tool::ProviderDefined {
            // Named `fusion` (not the namespaced form) so the loop's inject step
            // strips this declaration before the upstream call.
            id: "bitrouter.fusion".to_string(),
            name: FUSION_TOOL.to_string(),
            args,
            provider_metadata: ProviderMetadata::new(),
        }
    }
}

impl crate::app::PromptTransform for FusionAliasConfig {
    fn apply(&self, prompt: &mut Prompt) {
        // Discard the matched flag; the server applies every transform and a
        // non-matching one is a no-op.
        FusionAliasConfig::apply(self, prompt);
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::{FusionConfig, FusionSettings};
    use super::*;
    use crate::language_model::types::{GenerationParams, ProviderMetadata};

    fn prompt_with_model(model: &str) -> Prompt {
        Prompt {
            model: model.to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn sample_cfg() -> FusionAliasConfig {
        FusionAliasConfig {
            alias: "bitrouter/fusion".to_string(),
            outer_model: "anthropic/claude-opus-4.8".to_string(),
            panel: vec![
                "anthropic/claude-opus-4.8".to_string(),
                "openai/gpt-latest".to_string(),
            ],
            judge: "anthropic/claude-opus-4.8".to_string(),
            synthesizer: None,
            web_tools: vec![serde_json::json!({
                "type": "anthropic:web_search_20250305", "name": "web_search"
            })],
        }
    }

    #[test]
    fn rewrites_alias_and_injects_a_parseable_declaration() {
        let cfg = sample_cfg();
        let mut prompt = prompt_with_model("bitrouter/fusion");
        assert!(cfg.apply(&mut prompt));
        assert_eq!(prompt.model, "anthropic/claude-opus-4.8");

        // The injected declaration is named `fusion` so the loop strips it
        // before the upstream call, and it parses back into a FusionConfig.
        let decl = prompt
            .tools
            .iter()
            .find(|t| t.name() == "fusion")
            .expect("fusion declaration injected")
            .clone();
        let parsed = FusionConfig::from_tool(&decl, "anthropic/claude-opus-4.8").unwrap();
        assert_eq!(parsed.panel.len(), 2);
        assert_eq!(parsed.judge.model, "anthropic/claude-opus-4.8");
        // The per-member web tool rides along.
        assert_eq!(parsed.panel[0].tools.len(), 1);

        // System nudges the model toward the fusion tool.
        assert!(
            prompt
                .system
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains("fusion")
        );
    }

    #[test]
    fn preserves_an_existing_system_prompt() {
        let cfg = sample_cfg();
        let mut prompt = prompt_with_model("bitrouter/fusion");
        prompt.system = Some("Be terse.".to_string());
        cfg.apply(&mut prompt);
        let system = prompt.system.unwrap();
        assert!(system.starts_with("Be terse."));
        assert!(system.to_lowercase().contains("fusion"));
    }

    #[test]
    fn leaves_non_alias_requests_untouched() {
        let cfg = sample_cfg();
        let mut prompt = prompt_with_model("anthropic/claude-opus-4.8");
        assert!(!cfg.apply(&mut prompt));
        assert!(prompt.tools.is_empty());
        assert!(prompt.system.is_none());
        assert_eq!(prompt.model, "anthropic/claude-opus-4.8");
    }

    #[test]
    fn built_from_settings_with_defaults() {
        // Only a panel is set; outer model and judge default sensibly.
        let settings = FusionSettings {
            panel: vec!["a/1".to_string(), "b/2".to_string()],
            judge: None,
            synthesizer: None,
            alias: None,
            outer_model: None,
            web_tools: Vec::new(),
        };
        let cfg = FusionAliasConfig::from_settings(&settings).expect("enabled");
        assert_eq!(cfg.alias, "bitrouter/fusion");
        assert_eq!(cfg.outer_model, "a/1");
        assert_eq!(cfg.judge, "a/1");
        assert_eq!(cfg.panel, vec!["a/1".to_string(), "b/2".to_string()]);
    }

    #[test]
    fn from_settings_is_none_when_nothing_configured() {
        assert!(FusionAliasConfig::from_settings(&FusionSettings::default()).is_none());
    }

    #[test]
    fn configured_aliases_cannot_claim_reserved_route_addresses() {
        for alias in [
            "bitrouter/auto",
            "bitrouter/unknown",
            "bitrouter:auto",
            "@auto",
        ] {
            let settings = FusionSettings {
                alias: Some(alias.to_string()),
                outer_model: Some("anthropic/claude-opus-4.8".to_string()),
                ..Default::default()
            };

            let error = FusionAliasConfig::validate_settings(&settings).unwrap_err();
            assert_eq!(error.status(), 400);
            assert!(error.to_string().contains(alias));
            assert!(
                FusionAliasConfig::from_settings(&settings).is_none(),
                "reserved alias {alias} must not create an ingress transform"
            );
        }
    }

    #[test]
    fn ingress_transform_defensively_refuses_a_reserved_alias_collision() {
        let mut cfg = sample_cfg();
        cfg.alias = "bitrouter/auto".to_string();
        let mut prompt = prompt_with_model("bitrouter/auto");

        assert!(!cfg.apply(&mut prompt));
        assert_eq!(prompt.model, "bitrouter/auto");
        assert!(prompt.tools.is_empty());
        assert!(prompt.system.is_none());
    }

    #[test]
    fn forwards_synthesizer_when_configured() {
        let mut cfg = sample_cfg();
        cfg.synthesizer = Some("openai/gpt-latest".to_string());
        let mut prompt = prompt_with_model("bitrouter/fusion");
        cfg.apply(&mut prompt);
        let decl = prompt
            .tools
            .iter()
            .find(|t| t.name() == "fusion")
            .unwrap()
            .clone();
        let parsed = FusionConfig::from_tool(&decl, "x").unwrap();
        assert_eq!(parsed.synthesizer.as_deref(), Some("openai/gpt-latest"));
    }
}
