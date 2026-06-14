//! The native `RouterToolset` that exposes and executes `spawn_subagent`.

use async_trait::async_trait;
use sea_orm::DatabaseConnection;
use serde::Deserialize;
use std::sync::Arc;

use bitrouter_sdk::language_model::server_tools::config::SpawnSubagentConfig;
use bitrouter_sdk::language_model::server_tools::toolset::{RouterToolset, ToolContext};
use bitrouter_sdk::language_model::types::{ProviderMetadata, Tool, ToolResultOutput};
use bitrouter_sdk::{BitrouterError, Result};

use crate::metering::{MeteringStore, TimeWindow};
use crate::policy::{Policy, PolicyStore};
use crate::subagent::acp_client::{WorkerSpawn, drive_once};
use crate::subagent::worker_config::materialize;

/// The tool name the agent calls.
pub const TOOL_NAME: &str = "spawn_subagent";

/// The WIRE model id the daemon (and `PolicyHook`) sees — the part after the
/// first `/`. opencode's `provider/model` reference has its `provider/` prefix
/// stripped before the request leaves the worker, so the policy allowlist must
/// key on this, not the full reference. Must match `worker_config::materialize`'s
/// split (both split on the first `/`).
fn wire_model_id(model: &str) -> &str {
    model.split_once('/').map(|(_, m)| m).unwrap_or(model)
}

/// Native router toolset: mints a capped key, spawns an `opencode acp` worker,
/// drives it, and returns a structured result. Holds the daemon handles it needs
/// (none are available on `ToolContext`).
pub struct SpawnSubagentToolset {
    db: DatabaseConnection,
    policy_store: Arc<PolicyStore>,
    metering: MeteringStore,
    config: SpawnSubagentConfig,
}

#[derive(Deserialize)]
struct SpawnArgs {
    model: String,
    budget_micro_usd: u64,
    task: String,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
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

    async fn run_spawn(&self, args: SpawnArgs) -> Result<ToolResultOutput> {
        // 1. validate
        if !self.config.models.iter().any(|m| m == &args.model) {
            return Ok(ToolResultOutput::ErrorText {
                value: format!(
                    "model '{}' not allowed; choose from {:?}",
                    args.model, self.config.models
                ),
            });
        }
        if args.budget_micro_usd == 0 {
            return Ok(ToolResultOutput::ErrorText {
                value: "budget_micro_usd must be > 0".into(),
            });
        }

        // 2. random suffix for the policy id + temp dir (no DB row — just entropy)
        let unique = crate::auth::keys::generate().hash[..16].to_string();
        let policy_id = format!("pol-{unique}");

        // 3. register the capped policy.
        // Pin `allowed_models` to the WIRE model id — the part after the first
        // `/` — because that's what opencode sends upstream (the `provider/`
        // prefix is opencode-internal and stripped before the request reaches
        // `PolicyHook`). This MUST match `worker_config::materialize`'s split, or
        // every worker call is denied as `ModelNotAllowed`.
        let policy = Policy {
            id: policy_id.clone(),
            allowed_models: Some(vec![wire_model_id(&args.model).to_string()]),
            max_spend_micro_usd: Some(args.budget_micro_usd),
            allowed_tools: args.allowed_tools.clone(),
            ..Default::default()
        };
        self.policy_store.insert_policy(policy)?;

        // 4. mint ONE worker key, bound to the policy that now exists
        let minted = crate::commands::mint_key(&self.db, "subagent-worker", Some(&policy_id))
            .await
            .map_err(|e| BitrouterError::internal(format!("minting worker key: {e}")))?;

        // 5. materialize the worker config + worktree
        let ws = match materialize(&self.config.base_url, &args.model, &minted.secret, &unique) {
            Ok(w) => w,
            Err(e) => {
                return Ok(ToolResultOutput::ErrorText {
                    value: format!("worker config failed: {e}"),
                });
            }
        };

        // 6. drive the worker over ACP
        let spawn = WorkerSpawn {
            command: self.config.command.clone(),
            args: vec!["acp".into(), "--cwd".into(), ws.cwd.clone()],
            env: ws.env.clone(),
        };
        let task = format!(
            "{}\n\n(Work only under {}; use absolute paths.)",
            args.task, ws.cwd
        );
        let outcome = match drive_once(spawn, &task).await {
            Ok(o) => o,
            Err(e) => {
                return Ok(ToolResultOutput::ErrorText {
                    value: format!("subagent failed: {e}"),
                });
            }
        };

        // 7. read actual spend under the worker's key
        let spend: u64 = self
            .metering
            .get_spend(&minted.id, TimeWindow::ThisMonth)
            .await
            .unwrap_or_default();

        // 8. structured result
        Ok(ToolResultOutput::Json {
            value: serde_json::json!({
                "final_message": outcome.final_message,
                "files_touched": outcome.tool_calls,
                "spend_micro_usd": spend,
                "budget_micro_usd": args.budget_micro_usd,
                "stop_reason": outcome.stop_reason,
                "capped": spend >= args.budget_micro_usd,
            }),
        })
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
        arguments: &str,
        _ctx: &ToolContext,
    ) -> Result<ToolResultOutput> {
        let args: SpawnArgs = match serde_json::from_str(arguments) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolResultOutput::ErrorText {
                    value: format!("invalid arguments: {e}"),
                });
            }
        };
        self.run_spawn(args).await
    }

    fn owns(&self, name: &str) -> bool {
        name == TOOL_NAME
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::caller::CallerContext;

    #[test]
    fn wire_model_id_strips_provider_prefix() {
        // The policy allowlist must match what opencode sends on the wire.
        assert_eq!(wire_model_id("bitrouter/z-ai/glm-5.1"), "z-ai/glm-5.1");
        assert_eq!(wire_model_id("bitrouter/kimi-k2.6"), "kimi-k2.6");
        // No provider prefix → unchanged.
        assert_eq!(wire_model_id("m1"), "m1");
    }

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

    fn ctx() -> ToolContext {
        ToolContext::new(CallerContext::local(), Default::default())
    }

    #[tokio::test]
    async fn advertises_spawn_subagent_and_owns_it() {
        let ts = fixture().await;
        let tools = ts.list_tools(&ctx()).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), TOOL_NAME);
        assert!(ts.owns(TOOL_NAME));
        assert!(!ts.owns("something_else"));
    }

    #[tokio::test]
    async fn rejects_model_outside_allowlist() {
        let ts = fixture().await; // allowlist = ["m1"]
        let args =
            serde_json::json!({ "model": "evil/model", "budget_micro_usd": 1000, "task": "x" })
                .to_string();
        let out = ts.call_tool(TOOL_NAME, &args, &ctx()).await.unwrap();
        match out {
            ToolResultOutput::ErrorText { value } => assert!(value.contains("not allowed")),
            _ => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn rejects_zero_budget() {
        let ts = fixture().await;
        let args =
            serde_json::json!({ "model": "m1", "budget_micro_usd": 0, "task": "x" }).to_string();
        let out = ts.call_tool(TOOL_NAME, &args, &ctx()).await.unwrap();
        match out {
            ToolResultOutput::ErrorText { value } => assert!(value.contains("budget")),
            _ => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn registers_policy_and_mints_key_before_spawn() {
        let db = crate::db::connect("sqlite::memory:").await.unwrap();
        crate::db::run_migrations(&db).await.unwrap();
        let store = Arc::new(PolicyStore::new());
        let ts = SpawnSubagentToolset::new(
            db.clone(),
            store.clone(),
            MeteringStore::new(db),
            SpawnSubagentConfig {
                command: "definitely-not-a-real-binary-xyz".into(),
                models: vec!["m1".into()],
                ..Default::default()
            },
        );
        let args =
            serde_json::json!({ "model": "m1", "budget_micro_usd": 4242, "task": "x" }).to_string();
        let _ = ts.call_tool(TOOL_NAME, &args, &ctx()).await.unwrap(); // spawn fails → ErrorText, fine
        assert!(store.len() >= 1, "policy registered before spawn");
    }
}
