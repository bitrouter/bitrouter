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

use super::config::{DEFAULT_MAX_STEPS, FUSION_TOOL};
use crate::language_model::types::{Prompt, ProviderMetadata, Tool};

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
    /// Rewrite a prompt addressed to the alias: swap in the outer model, inject
    /// the `bitrouter:fusion` declaration, and nudge the model toward it. Returns
    /// `true` when the alias matched and the prompt was rewritten.
    pub fn apply(&self, prompt: &mut Prompt) -> bool {
        if prompt.model != self.alias {
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
            "max_steps": DEFAULT_MAX_STEPS,
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

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::config::FusionConfig;
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
    fn forwards_synthesizer_when_configured() {
        let mut cfg = sample_cfg();
        cfg.synthesizer = Some("openai/gpt-latest".to_string());
        let mut prompt = prompt_with_model("bitrouter/fusion");
        cfg.apply(&mut prompt);
        let decl = prompt.tools.iter().find(|t| t.name() == "fusion").unwrap().clone();
        let parsed = FusionConfig::from_tool(&decl, "x").unwrap();
        assert_eq!(parsed.synthesizer.as_deref(), Some("openai/gpt-latest"));
    }
}
