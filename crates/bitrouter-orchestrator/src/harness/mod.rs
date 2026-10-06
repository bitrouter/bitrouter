//! Workspace-scoped project instructions, MCP and skills for the native runtime.
//! AGENTS.md augments model instructions; resource discovery never grants permission.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bitrouter_sdk::language_model::{Tool, ToolResultOutput};
use bitrouter_sdk::mcp::transport::McpServerConfig;
use rmcp::model::ProtocolVersion;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent::ToolMode;
use crate::store::EffectStatus;

pub mod instructions;
pub mod mcp;
pub mod skills;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ResourceError {
    pub message: String,
    pub cleanup_unknown: bool,
}

impl From<String> for ResourceError {
    fn from(message: String) -> Self {
        Self {
            message,
            cleanup_unknown: false,
        }
    }
}

/// Selected by the execution host. Clients cannot inject transports or roots.
#[derive(Clone)]
pub struct HarnessConfig {
    pub servers: Vec<McpServerConfig>,
    pub protocol: ProtocolVersion,
    pub skill_roots: Vec<PathBuf>,
    pub instructions: instructions::InstructionConfig,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            protocol: ProtocolVersion::LATEST,
            skill_roots: Vec::new(),
            instructions: instructions::InstructionConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialRef {
    pub material_id: String,
    pub version: String,
    pub sha256: String,
    pub media_type: String,
    pub provenance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub effect: EffectStatus,
    pub approval_required: bool,
}

impl McpTool {
    pub(crate) fn declaration(&self) -> Tool {
        Tool::Function {
            name: self.name.clone(),
            description: Some(self.description.clone()),
            parameters: self.parameters.clone(),
            strict: Some(false),
            provider_metadata: Default::default(),
        }
    }
}

/// Frozen at execution preparation and committed before the first model call.
/// No credentials or skill bodies are part of this public inventory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessInventory {
    pub binding_digest: String,
    pub tools: Vec<McpTool>,
    pub instructions: Vec<MaterialRef>,
    pub skills: Vec<skills::SkillMetadata>,
    pub skill_problems: Vec<String>,
}

impl HarnessInventory {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.binding_digest.len() != 64
            || !self.binding_digest.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err("invalid harness binding digest".into());
        }
        if serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > 256 * 1024 {
            return Err("harness inventory exceeds 256 KiB".into());
        }
        let mut names = std::collections::BTreeSet::new();
        for tool in &self.tools {
            validate_id(&tool.name)?;
            if !names.insert(&tool.name)
                || !tool.approval_required
                || tool.effect != EffectStatus::Unknown
            {
                return Err("invalid harness tool authority".into());
            }
        }
        Ok(())
    }
}

/// Connections live only for one active execution pass, after admission. They
/// are closed before settlement; cold history/open operations never create one.
pub(crate) struct HarnessResources {
    pub(crate) inventory: HarnessInventory,
    mcp: Mutex<mcp::McpConnections>,
}

impl HarnessResources {
    pub(crate) async fn discover(
        workspace: &Path,
        config: &HarnessConfig,
        mode: ToolMode,
        cancel: &CancellationToken,
        max_duration: Duration,
    ) -> Result<Self, ResourceError> {
        let started = std::time::Instant::now();
        let mut roots = vec![workspace.to_path_buf()];
        roots.extend(config.skill_roots.clone());
        let catalog = tokio::task::spawn_blocking(move || skills::SkillCatalog::discover(&roots))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        // Unknown effects are not exposed in read-only mode, regardless of MCP hints.
        let servers = if mode == ToolMode::ReadOnly {
            &[][..]
        } else {
            config.servers.as_slice()
        };
        let mut connections = mcp::McpConnections::connect_cancellable(
            workspace,
            servers,
            config.protocol.clone(),
            cancel,
            max_duration.saturating_sub(started.elapsed()),
        )
        .await?;
        let encoded = serde_json::to_value((
            workspace,
            servers,
            config.protocol.to_string(),
            config.skill_roots.as_slice(),
            mode,
        ))
        .map_err(|e| e.to_string())?;
        let binding_digest =
            sha256(&serde_json::to_vec(&canonical(encoded)).map_err(|e| e.to_string())?);
        let inventory = HarnessInventory {
            binding_digest,
            tools: connections.tools.clone(),
            instructions: connections.instructions.clone(),
            skills: catalog.skills,
            skill_problems: catalog.problems,
        };
        if let Err(error) = inventory.validate() {
            return match connections.shutdown().await {
                Ok(()) => Err(error.into()),
                Err(cleanup) => Err(ResourceError {
                    message: format!("{error}; {cleanup}"),
                    cleanup_unknown: true,
                }),
            };
        }
        Ok(Self {
            inventory,
            mcp: Mutex::new(connections),
        })
    }

    pub(crate) async fn validate_catalog(&self) -> Result<(), String> {
        self.mcp.lock().await.validate_catalog()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.inventory.tools.iter().any(|tool| tool.name == name)
    }

    pub(crate) fn validate_call(&self, name: &str, arguments: &str) -> Result<(), String> {
        if !self.contains(name) {
            return Err("MCP tool is not in the frozen inventory".into());
        }
        let args: Value = serde_json::from_str(arguments).map_err(|e| e.to_string())?;
        if !args.is_object() {
            return Err("MCP arguments must be an object".into());
        }
        Ok(())
    }

    pub(crate) async fn execute(
        &self,
        name: &str,
        arguments: &str,
        cancel: &CancellationToken,
    ) -> (ToolResultOutput, EffectStatus) {
        let args = match serde_json::from_str(arguments) {
            Ok(value) => value,
            Err(error) => {
                return (
                    ToolResultOutput::ErrorJson {
                        value: serde_json::json!({"error":error.to_string()}),
                    },
                    EffectStatus::NotExecuted,
                );
            }
        };
        match self
            .mcp
            .lock()
            .await
            .call(name, &args, 32 * 1024, cancel)
            .await
        {
            Ok((effect, output)) => match serde_json::from_str::<Value>(&output) {
                Ok(value) if effect == EffectStatus::Completed && value["isError"] != true => {
                    (ToolResultOutput::Json { value }, effect)
                }
                Ok(value) => (ToolResultOutput::ErrorJson { value }, effect),
                Err(_) => (
                    ToolResultOutput::ErrorJson {
                        value: serde_json::json!({"error":output}),
                    },
                    effect,
                ),
            },
            Err(error) => (
                ToolResultOutput::ErrorJson {
                    value: serde_json::json!({"error":error}),
                },
                EffectStatus::NotExecuted,
            ),
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(5), async {
            self.mcp.lock().await.shutdown().await
        })
        .await
        .map_err(|_| "MCP cleanup timed out; execution remains blocked".to_string())?
    }
}

fn canonical(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<_, _> = map
                .into_iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
        other => other,
    }
}

pub(super) fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn validate_id(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(
            "MCP names must contain 1-64 ASCII letters, digits, underscores or hyphens".into(),
        );
    }
    Ok(())
}
