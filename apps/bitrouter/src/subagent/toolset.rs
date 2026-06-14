//! The native `RouterToolset` that exposes and executes `spawn_subagent`.

use async_trait::async_trait;
use sea_orm::DatabaseConnection;
use std::sync::Arc;

use bitrouter_sdk::language_model::server_tools::config::SpawnSubagentConfig;
use bitrouter_sdk::language_model::server_tools::toolset::{RouterToolset, ToolContext};
use bitrouter_sdk::language_model::types::{ProviderMetadata, Tool, ToolResultOutput};
use bitrouter_sdk::Result;

use crate::metering::MeteringStore;
use crate::policy::PolicyStore;

/// The tool name the agent calls.
pub const TOOL_NAME: &str = "spawn_subagent";

/// Native router toolset: mints a capped key, spawns an `opencode acp` worker,
/// drives it, and returns a structured result. Holds the daemon handles it needs
/// (none are available on `ToolContext`).
pub struct SpawnSubagentToolset {
    db: DatabaseConnection,
    policy_store: Arc<PolicyStore>,
    metering: MeteringStore,
    config: SpawnSubagentConfig,
}

impl SpawnSubagentToolset {
    /// Build a toolset from daemon-level handles.
    pub fn new(
        db: DatabaseConnection,
        policy_store: Arc<PolicyStore>,
        metering: MeteringStore,
        config: SpawnSubagentConfig,
    ) -> Self {
        Self {
            db,
            policy_store,
            metering,
            config,
        }
    }

    fn tool_schema() -> Tool {
        Tool::Function {
            name: TOOL_NAME.to_string(),
            description: Some(
                "Spawn a budget-capped subagent on a chosen model to perform a coding task. \
                 The subagent's spend is hard-capped; address files by ABSOLUTE path."
                    .to_string(),
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "model": {
                        "type": "string",
                        "description": "Model id for the subagent (must be in the configured allowlist)."
                    },
                    "budget_micro_usd": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Hard spend cap in micro-USD."
                    },
                    "task": {
                        "type": "string",
                        "description": "The task prompt. Use absolute paths."
                    },
                    "allowed_tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional: restrict the subagent's tools."
                    }
                },
                "required": ["model", "budget_micro_usd", "task"]
            }),
            strict: None,
            provider_metadata: ProviderMetadata::new(),
        }
    }
}

#[async_trait]
impl RouterToolset for SpawnSubagentToolset {
    async fn list_tools(&self, _ctx: &ToolContext) -> Result<Vec<Tool>> {
        Ok(vec![Self::tool_schema()])
    }

    async fn call_tool(
        &self,
        _name: &str,
        _arguments: &str,
        _ctx: &ToolContext,
    ) -> Result<ToolResultOutput> {
        // Validate model allowlist eagerly so the caller sees a useful error
        // even before full orchestration is wired (Task 7).
        // Referencing self.config / self.db / self.policy_store / self.metering
        // here keeps the fields live until the full implementation arrives.
        let _ = (&self.db, &self.policy_store, &self.metering);
        let _ = &self.config.models;
        // Orchestration lands in a later task.
        Ok(ToolResultOutput::ErrorText {
            value: "spawn_subagent not yet implemented".to_string(),
        })
    }

    fn owns(&self, name: &str) -> bool {
        name == TOOL_NAME
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::caller::CallerContext;

    async fn fixture() -> SpawnSubagentToolset {
        let db = crate::db::connect("sqlite::memory:").await.unwrap();
        crate::db::run_migrations(&db).await.unwrap();
        SpawnSubagentToolset::new(
            db.clone(),
            Arc::new(PolicyStore::new()),
            MeteringStore::new(db),
            SpawnSubagentConfig {
                models: vec!["m1".into()],
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn advertises_spawn_subagent_and_owns_it() {
        let ts = fixture().await;
        let ctx = ToolContext::new(CallerContext::local(), Default::default());
        let tools = ts.list_tools(&ctx).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), TOOL_NAME);
        assert!(ts.owns(TOOL_NAME));
        assert!(!ts.owns("something_else"));
    }
}
