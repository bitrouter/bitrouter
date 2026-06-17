//! The Sub-agent server tool: delegate a self-contained task to a (typically
//! cheaper / faster) worker model mid-generation. Server-tool name: `subagent`.
//!
//! The worker model is fixed by the declaration (it falls back to the parent
//! model); the calling model supplies `task_name` / `task_description` and sees
//! only the worker's final outcome. Generic over a [`NestedRunner`].

use std::sync::Arc;

use async_trait::async_trait;

use super::declarations::{SUBAGENT_TOOL, ServerToolDeclarations, forwarded_tools};
use super::nested::{NestedRequest, NestedRunner};
use crate::error::Result;
use crate::language_model::server_tools::toolset::{RouterToolset, ToolContext};
use crate::language_model::types::{ProviderMetadata, Tool, ToolResultOutput};

/// A [`RouterToolset`] exposing the `subagent` server tool.
pub struct SubAgentToolset {
    runner: Arc<dyn NestedRunner>,
}

impl SubAgentToolset {
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
impl RouterToolset for SubAgentToolset {
    async fn list_tools(&self, ctx: &ToolContext) -> Result<Vec<Tool>> {
        let advertise =
            ServerToolDeclarations::from_context(ctx).is_some_and(|d| d.subagent.is_some());
        if !advertise {
            return Ok(Vec::new());
        }
        Ok(vec![Tool::Function {
            name: SUBAGENT_TOOL.to_string(),
            description: Some(
                "Delegate a self-contained task to a focused worker model. Put \
                 everything the worker needs in `task_description` (it sees no \
                 other context); the worker's final result is returned."
                    .to_string(),
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_name": { "type": "string", "description": "Short identifier for the task." },
                    "task_description": { "type": "string", "description": "Full, self-contained task: context, inputs, expected output." }
                },
                "required": ["task_name", "task_description"],
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
            return Ok(error_output("subagent was not declared on this request"));
        };
        let config = decls.subagent.clone().unwrap_or_default();
        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
        let task_name = args
            .get("task_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let Some(task_description) = args.get("task_description").and_then(|v| v.as_str()) else {
            return Ok(error_output(
                "subagent call is missing required `task_description`",
            ));
        };
        // Worker model fixed by the declaration; falls back to the parent model.
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| decls.parent_model.clone());

        let request = NestedRequest {
            model,
            system: config.instructions.clone(),
            user: task_description.to_string(),
            tools: forwarded_tools(&config.tools),
            response_format: None,
        };
        match self.runner.run(request, ctx).await {
            Ok(out) => Ok(ToolResultOutput::Json {
                value: serde_json::json!({
                    "status": "ok",
                    "model": out.model,
                    "task_name": task_name,
                    "outcome": out.text,
                }),
            }),
            Err(err) => Ok(error_output(err)),
        }
    }

    fn owns(&self, name: &str) -> bool {
        name.rsplit([':', '.']).next().unwrap_or(name) == SUBAGENT_TOOL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::server_tools::declarations::{
        SubAgentConfig, declarations_plugin_id,
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
                text: "worker result".to_string(),
                usage: Default::default(),
            })
        }
    }

    fn ctx_with(decls: ServerToolDeclarations) -> ToolContext {
        let mut meta: HashMap<_, _> = HashMap::new();
        meta.insert(
            declarations_plugin_id().clone(),
            serde_json::to_value(decls).unwrap(),
        );
        ToolContext::new(CallerContext::local(), meta)
    }

    #[tokio::test]
    async fn delegates_with_fixed_worker_model() {
        let runner = Arc::new(MockRunner {
            seen: Mutex::new(Vec::new()),
        });
        let ts = SubAgentToolset::new(runner.clone());
        let decls = ServerToolDeclarations {
            subagent: Some(SubAgentConfig {
                model: Some("worker/model".into()),
                instructions: None,
                tools: Vec::new(),
            }),
            parent_model: "parent/model".to_string(),
            ..Default::default()
        };
        let out = ts
            .call_tool(
                "subagent",
                r#"{"task_name":"t","task_description":"do it"}"#,
                &ctx_with(decls),
            )
            .await
            .unwrap();
        assert!(
            matches!(&out, ToolResultOutput::Json { value } if value["status"] == "ok" && value["outcome"] == "worker result")
        );
        assert_eq!(runner.seen.lock().unwrap()[0].model, "worker/model");
    }

    #[tokio::test]
    async fn missing_task_description_is_an_error_result() {
        let ts = SubAgentToolset::new(Arc::new(MockRunner {
            seen: Mutex::new(Vec::new()),
        }));
        let decls = ServerToolDeclarations {
            subagent: Some(SubAgentConfig::default()),
            parent_model: "m".to_string(),
            ..Default::default()
        };
        let out = ts
            .call_tool("subagent", "{}", &ctx_with(decls))
            .await
            .unwrap();
        assert!(matches!(&out, ToolResultOutput::Json { value } if value["status"] == "error"));
    }
}
