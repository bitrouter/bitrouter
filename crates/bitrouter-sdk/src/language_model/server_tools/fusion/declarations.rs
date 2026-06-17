//! Stage-0 capture of the request's Fusion declaration.
//!
//! The toolset receives only a [`ToolContext`](super::super::toolset::ToolContext)
//! (caller + metadata), not the prompt. This always-run pre-request hook parses
//! the `bitrouter:fusion` declaration off the prompt once, resolves its panel /
//! judge models against the outer request model, and stashes the result on the
//! request context under [`fusion_plugin_id`] for the toolset to read back. Pure
//! observation — it never denies.

use async_trait::async_trait;

use super::config::{FusionConfig, fusion_plugin_id};
use crate::error::Result;
use crate::language_model::context::PipelineContext;
use crate::language_model::hooks::{HookDecision, PreRequestHook};

/// Pre-request hook that stashes the parsed Fusion declaration on the context.
pub struct FusionDeclarationsHook;

#[async_trait]
impl PreRequestHook for FusionDeclarationsHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        let parent_model = ctx.prompt().model.clone();
        let config = ctx
            .prompt()
            .tools
            .iter()
            .find_map(|t| FusionConfig::from_tool(t, &parent_model));
        if let Some(config) = config
            && let Ok(value) = serde_json::to_value(&config)
        {
            ctx.set_metadata(fusion_plugin_id(), value);
        }
        Ok(HookDecision::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::types::{
        GenerationParams, PipelineRequest, Prompt, ProviderMetadata, Tool,
    };

    fn ctx_with(tools: Vec<Tool>, model: &str) -> PipelineContext {
        let prompt = Prompt {
            model: model.to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: Vec::new(),
            tools,
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        PipelineContext::new(PipelineRequest::new(model, CallerContext::local(), prompt))
    }

    fn fusion_decl(args: serde_json::Value) -> Tool {
        Tool::ProviderDefined {
            id: "bitrouter.fusion".to_string(),
            name: "fusion".to_string(),
            args,
            provider_metadata: ProviderMetadata::new(),
        }
    }

    #[tokio::test]
    async fn stashes_resolved_config_with_parent_fallback() {
        let mut ctx = ctx_with(vec![fusion_decl(serde_json::json!({}))], "parent/m");
        let decision = FusionDeclarationsHook.check(&mut ctx).await.unwrap();
        assert!(matches!(decision, HookDecision::Allow));
        let stashed: FusionConfig =
            serde_json::from_value(ctx.get_metadata(fusion_plugin_id()).unwrap().clone()).unwrap();
        assert_eq!(stashed.panel[0].model, "parent/m");
        assert_eq!(stashed.judge.model, "parent/m");
    }

    #[tokio::test]
    async fn no_stash_without_a_declaration() {
        let mut ctx = ctx_with(Vec::new(), "parent/m");
        FusionDeclarationsHook.check(&mut ctx).await.unwrap();
        assert!(ctx.get_metadata(fusion_plugin_id()).is_none());
    }
}
