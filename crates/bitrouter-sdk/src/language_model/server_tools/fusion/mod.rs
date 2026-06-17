//! Fusion: a multi-model deliberation server tool. A panel of models answers a
//! prompt in parallel, a judge compares (not merges) their answers into
//! structured analysis, and the calling model writes the final answer from it.
//!
//! Reference design (behavior modeled after OpenRouter Fusion):
//! <https://openrouter.ai/docs/guides/features/server-tools/fusion>

pub mod alias;
pub mod config;
pub mod declarations;
pub mod engine;
pub mod judge;

use std::sync::Arc;

use async_trait::async_trait;

use self::config::{FUSION_TOOL, FusionConfig, is_fusion_name};
use self::engine::run_fusion;
use crate::error::Result;
use crate::language_model::server_tools::nested::NestedRunner;
use crate::language_model::server_tools::toolset::{RouterToolset, ToolContext};
use crate::language_model::types::{ProviderMetadata, Tool, ToolResultOutput};

/// The `bitrouter:fusion` server tool, generic over a [`NestedRunner`] so each
/// deployment supplies its own (a custom runner can add identity + metering).
///
/// Advertise it only at the outermost loop: the engine runs nested completions,
/// and a deployment must ensure panel members cannot recursively invoke Fusion
/// (the reference wiring runs them on a loop-less sub-pipeline).
pub struct FusionToolset {
    runner: Arc<dyn NestedRunner>,
}

impl FusionToolset {
    /// Build the toolset over a nested-completion runner.
    pub fn new(runner: Arc<dyn NestedRunner>) -> Self {
        Self { runner }
    }
}

fn error_output(message: impl Into<String>) -> ToolResultOutput {
    ToolResultOutput::Json {
        value: serde_json::json!({ "status": "error", "error": message.into() }),
    }
}

#[async_trait]
impl RouterToolset for FusionToolset {
    async fn list_tools(&self, ctx: &ToolContext) -> Result<Vec<Tool>> {
        // Advertised only when the request declared Fusion (directly or via the
        // model alias). The declaration hook stashes the resolved config.
        if FusionConfig::from_context(ctx).is_none() {
            return Ok(Vec::new());
        }
        Ok(vec![Tool::Function {
            name: FUSION_TOOL.to_string(),
            description: Some(
                "Deliberate on a prompt with a multi-model panel and a judge. A \
                 panel of models answers in parallel, a judge compares (not \
                 merges) their answers, and you receive structured analysis \
                 (consensus, contradictions, partial_coverage, unique_insights, \
                 blind_spots) to write the final answer from."
                    .to_string(),
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "The question to deliberate on." }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
            strict: None,
            provider_metadata: ProviderMetadata::new(),
        }])
    }

    async fn call_tool(
        &self,
        _name: &str,
        arguments: &str,
        ctx: &ToolContext,
    ) -> Result<ToolResultOutput> {
        let Some(config) = FusionConfig::from_context(ctx) else {
            return Ok(error_output("fusion was not declared on this request"));
        };
        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
        let Some(prompt) = args.get("prompt").and_then(|v| v.as_str()) else {
            return Ok(error_output("fusion call is missing required `prompt`"));
        };
        match run_fusion(&config, self.runner.clone(), prompt, ctx).await {
            Ok(outcome) => {
                // Surface the aggregated nested usage for observability. Each
                // nested completion is also metered individually by whatever
                // settlement recorders the runner's sub-pipeline carries.
                tracing::info!(
                    target: "bitrouter::fusion",
                    panel = config.panel.len(),
                    prompt_tokens = outcome.usage.prompt_tokens,
                    completion_tokens = outcome.usage.completion_tokens,
                    "fusion deliberation complete"
                );
                Ok(outcome.output)
            }
            Err(e) => Ok(error_output(format!("fusion: {e}"))),
        }
    }

    fn owns(&self, name: &str) -> bool {
        is_fusion_name(name)
    }
}

#[cfg(test)]
mod tests {
    use super::config::{FusionConfig, fusion_plugin_id};
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::server_tools::nested::{NestedOutcome, NestedRequest, NestedRunner};
    use crate::language_model::server_tools::toolset::{RouterToolset, ToolContext};
    use crate::language_model::types::ToolResultOutput;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;

    struct StubRunner;
    #[async_trait]
    impl NestedRunner for StubRunner {
        async fn run(
            &self,
            req: NestedRequest,
            _ctx: &ToolContext,
        ) -> std::result::Result<NestedOutcome, String> {
            let text = if req.response_format.is_some() {
                "{\"consensus\":[\"agreed\"]}".to_string()
            } else {
                "an answer".to_string()
            };
            Ok(NestedOutcome {
                model: req.model,
                text,
                usage: Default::default(),
            })
        }
    }

    fn ctx_declared() -> ToolContext {
        let mut meta = HashMap::new();
        meta.insert(
            fusion_plugin_id().clone(),
            serde_json::to_value(FusionConfig::single("m/1")).unwrap(),
        );
        ToolContext::new(CallerContext::local(), meta)
    }
    fn ctx_undeclared() -> ToolContext {
        ToolContext::new(CallerContext::local(), HashMap::new())
    }

    #[tokio::test]
    async fn advertises_fusion_only_when_declared() {
        let ts = FusionToolset::new(Arc::new(StubRunner));
        assert!(ts.list_tools(&ctx_undeclared()).await.unwrap().is_empty());
        let tools = ts.list_tools(&ctx_declared()).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "fusion");
        assert!(ts.owns("fusion"));
        assert!(ts.owns("bitrouter:fusion"));
        assert!(!ts.owns("advisor"));
    }

    #[tokio::test]
    async fn executes_fusion_and_returns_analysis() {
        let ts = FusionToolset::new(Arc::new(StubRunner));
        let out = ts
            .call_tool("fusion", r#"{"prompt":"hi"}"#, &ctx_declared())
            .await
            .unwrap();
        assert!(
            matches!(&out, ToolResultOutput::Json { value } if value["analysis"]["consensus"][0] == "agreed")
        );
    }

    #[tokio::test]
    async fn missing_prompt_is_an_error_result() {
        let ts = FusionToolset::new(Arc::new(StubRunner));
        let out = ts.call_tool("fusion", "{}", &ctx_declared()).await.unwrap();
        assert!(matches!(&out, ToolResultOutput::Json { value } if value["status"] == "error"));
    }

    #[tokio::test]
    async fn undeclared_call_is_an_error_result() {
        let ts = FusionToolset::new(Arc::new(StubRunner));
        let out = ts
            .call_tool("fusion", r#"{"prompt":"hi"}"#, &ctx_undeclared())
            .await
            .unwrap();
        assert!(matches!(&out, ToolResultOutput::Json { value } if value["status"] == "error"));
    }
}
