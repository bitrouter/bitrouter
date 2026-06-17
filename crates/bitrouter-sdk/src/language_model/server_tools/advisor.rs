//! The Advisor server tool: consult a (typically stronger) model mid-generation
//! for guidance. Server-tool name: `advisor`.
//!
//! Generic over a [`NestedRunner`] so each deployment supplies its own (a custom
//! runner can add identity + metering). Advertised only when the request
//! declared an advisor; the model calls it with a self-contained `prompt` and
//! receives the advice to act on.

use std::sync::Arc;

use async_trait::async_trait;

use super::declarations::{ADVISOR_TOOL, ServerToolDeclarations, forwarded_tools};
use super::nested::{NestedRequest, NestedRunner};
use crate::error::Result;
use crate::language_model::server_tools::toolset::{RouterToolset, ToolContext};
use crate::language_model::types::{ProviderMetadata, Tool, ToolResultOutput};

/// A [`RouterToolset`] exposing the `advisor` server tool.
pub struct AdvisorToolset {
    runner: Arc<dyn NestedRunner>,
}

impl AdvisorToolset {
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
impl RouterToolset for AdvisorToolset {
    async fn list_tools(&self, ctx: &ToolContext) -> Result<Vec<Tool>> {
        let advertise =
            ServerToolDeclarations::from_context(ctx).is_some_and(|d| d.advisor.is_some());
        if !advertise {
            return Ok(Vec::new());
        }
        Ok(vec![Tool::Function {
            name: ADVISOR_TOOL.to_string(),
            description: Some(
                "Consult a stronger advisor model for guidance mid-task. Put a \
                 clear, self-contained question in `prompt`; the advice is \
                 returned for you to act on."
                    .to_string(),
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "The question for the advisor." },
                    "model": { "type": "string", "description": "Optional advisor model override." }
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
        let Some(decls) = ServerToolDeclarations::from_context(ctx) else {
            return Ok(error_output("advisor was not declared on this request"));
        };
        let config = decls.advisor.clone().unwrap_or_default();
        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
        let Some(prompt) = args.get("prompt").and_then(|v| v.as_str()) else {
            return Ok(error_output("advisor call is missing required `prompt`"));
        };
        // Model precedence: tool-def pin → call arg → parent model.
        let model = config
            .model
            .clone()
            .or_else(|| {
                args.get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| decls.parent_model.clone());

        let request = NestedRequest {
            model,
            system: config.instructions.clone(),
            user: prompt.to_string(),
            tools: forwarded_tools(&config.tools),
            response_format: None,
        };
        match self.runner.run(request, ctx).await {
            Ok(out) => Ok(ToolResultOutput::Json {
                value: serde_json::json!({ "status": "ok", "model": out.model, "advice": out.text }),
            }),
            Err(err) => Ok(error_output(err)),
        }
    }

    fn owns(&self, name: &str) -> bool {
        // Tail-match so a namespaced declaration (`bitrouter:advisor`) is also
        // recognised — the loop strips owned provider-defined declarations.
        name.rsplit([':', '.']).next().unwrap_or(name) == ADVISOR_TOOL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::server_tools::declarations::{
        AdvisorConfig, declarations_plugin_id,
    };
    use crate::language_model::server_tools::nested::NestedOutcome;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct MockRunner {
        seen: Mutex<Vec<NestedRequest>>,
    }
    #[async_trait]
    impl NestedRunner for MockRunner {
        async fn run(
            &self,
            request: NestedRequest,
            _ctx: &ToolContext,
        ) -> std::result::Result<NestedOutcome, String> {
            let model = request.model.clone();
            self.seen.lock().unwrap().push(request);
            Ok(NestedOutcome {
                model,
                text: "advice text".to_string(),
                usage: Default::default(),
            })
        }
    }

    fn ctx_with(decls: Option<ServerToolDeclarations>) -> ToolContext {
        let mut meta: HashMap<_, _> = HashMap::new();
        if let Some(d) = decls {
            meta.insert(
                declarations_plugin_id().clone(),
                serde_json::to_value(d).unwrap(),
            );
        }
        ToolContext::new(CallerContext::local(), meta)
    }

    #[tokio::test]
    async fn advertises_only_when_declared() {
        let ts = AdvisorToolset::new(Arc::new(MockRunner {
            seen: Mutex::new(Vec::new()),
        }));
        assert!(ts.list_tools(&ctx_with(None)).await.unwrap().is_empty());
        let decls = ServerToolDeclarations {
            advisor: Some(AdvisorConfig::default()),
            parent_model: "m".to_string(),
            ..Default::default()
        };
        let tools = ts.list_tools(&ctx_with(Some(decls))).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "advisor");
        assert!(ts.owns("advisor"));
        assert!(ts.owns("bitrouter:advisor"));
        assert!(!ts.owns("subagent"));
    }

    #[tokio::test]
    async fn pins_model_then_falls_back_to_parent() {
        let runner = Arc::new(MockRunner {
            seen: Mutex::new(Vec::new()),
        });
        let ts = AdvisorToolset::new(runner.clone());
        let decls = ServerToolDeclarations {
            advisor: Some(AdvisorConfig {
                model: None,
                instructions: Some("be terse".into()),
                tools: vec![serde_json::json!({
                    "type": "anthropic:web_search_20250305", "name": "web_search"
                })],
            }),
            parent_model: "parent/model".to_string(),
            ..Default::default()
        };
        let out = ts
            .call_tool(
                "advisor",
                r#"{"prompt":"is X safe?"}"#,
                &ctx_with(Some(decls)),
            )
            .await
            .unwrap();
        assert!(
            matches!(&out, ToolResultOutput::Json { value } if value["status"] == "ok" && value["advice"] == "advice text")
        );
        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen[0].model, "parent/model");
        assert_eq!(seen[0].system.as_deref(), Some("be terse"));
        assert_eq!(seen[0].user, "is X safe?");
        assert_eq!(seen[0].tools.len(), 1);
        assert_eq!(seen[0].tools[0].name(), "web_search");
    }

    #[tokio::test]
    async fn call_arg_model_overrides_when_unpinned() {
        let runner = Arc::new(MockRunner {
            seen: Mutex::new(Vec::new()),
        });
        let ts = AdvisorToolset::new(runner.clone());
        let decls = ServerToolDeclarations {
            advisor: Some(AdvisorConfig::default()),
            parent_model: "parent/model".to_string(),
            ..Default::default()
        };
        ts.call_tool(
            "advisor",
            r#"{"prompt":"q","model":"override/m"}"#,
            &ctx_with(Some(decls)),
        )
        .await
        .unwrap();
        assert_eq!(runner.seen.lock().unwrap()[0].model, "override/m");
    }

    #[tokio::test]
    async fn missing_prompt_is_an_error_result() {
        let ts = AdvisorToolset::new(Arc::new(MockRunner {
            seen: Mutex::new(Vec::new()),
        }));
        let decls = ServerToolDeclarations {
            advisor: Some(AdvisorConfig::default()),
            parent_model: "m".to_string(),
            ..Default::default()
        };
        let out = ts
            .call_tool("advisor", "{}", &ctx_with(Some(decls)))
            .await
            .unwrap();
        assert!(matches!(&out, ToolResultOutput::Json { value } if value["status"] == "error"));
    }
}
