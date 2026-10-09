//! Native workspace resources behind the managed core's execution boundary.
//! This adapter owns no model loop and never interprets collaboration tools.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use bitrouter_ai::types::{Tool, ToolResultOutput};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{HarnessConfig, HarnessResources, ResourceError, instructions};
use crate::agent::ToolMode;
use crate::core::checkpoint::sha256;
use crate::core::protocol::{
    CoreError, ErrorCode, HarnessManifest, HarnessTool, Limits, MaterialRef, OwnershipGrant,
    SignalUpdate, ToolEffect, ToolExecute, ToolOutcome, ToolResult,
};
use crate::store::EffectStatus;
use crate::tools::WorkspaceTools;

pub mod session;
pub mod store;

/// Frozen resources for one native execution owner. The caller must durably
/// admit starts, serialize them with fences, and hold workspace exclusion before
/// calling `execute`. Resource discovery does not authorize an effect.
pub struct NativeResources {
    tools: WorkspaceTools,
    resources: HarnessResources,
    signals: SignalUpdate,
}

impl NativeResources {
    /// Native tool output includes bounded structured metadata and JSON escaping;
    /// a native file read is larger than the default remote tool allowance.
    pub fn limits() -> Limits {
        Limits {
            input_bytes: 2 * 1024 * 1024,
            checkpoint_bytes: 64 * 1024 * 1024,
            unacknowledged_bytes: 96 * 1024 * 1024,
            ..Limits::default()
        }
    }

    pub async fn discover(
        workspace: &Path,
        mode: ToolMode,
        config: &HarnessConfig,
        cancel: &CancellationToken,
    ) -> Result<Self, ResourceError> {
        let tools = WorkspaceTools::new(workspace, mode).map_err(|error| error.to_string())?;
        let root = tools.root().to_path_buf();
        let instruction_config = config.instructions.clone();
        let startup = tokio::task::spawn_blocking(move || {
            instructions::InstructionSnapshot::load(&root, &root, &instruction_config)
        })
        .await
        .map_err(|error| error.to_string())??;
        let resources =
            HarnessResources::discover(tools.root(), config, mode, cancel, Duration::from_secs(30))
                .await?;
        let prepared = Self::prepare(&tools, &resources, &startup);
        match prepared {
            Ok(signals) => Ok(Self {
                tools,
                resources,
                signals,
            }),
            Err(error) => {
                let cleanup = resources.shutdown().await;
                Err(ResourceError {
                    message: match &cleanup {
                        Ok(()) => error,
                        Err(cleanup) => format!("{error}; {cleanup}"),
                    },
                    cleanup_unknown: cleanup.is_err(),
                })
            }
        }
    }

    fn prepare(
        tools: &WorkspaceTools,
        resources: &HarnessResources,
        startup: &instructions::InstructionSnapshot,
    ) -> Result<SignalUpdate, String> {
        let mut declarations = Vec::new();
        for tool in tools.declarations() {
            let Tool::Function {
                name,
                description,
                parameters,
                ..
            } = tool
            else {
                return Err("native workspace tool has no function declaration".into());
            };
            let effect = if WorkspaceTools::read_only(&name) {
                ToolEffect::Read
            } else if name == "shell" {
                ToolEffect::Shell
            } else {
                ToolEffect::Write
            };
            declarations.push(HarnessTool {
                name,
                description: description.unwrap_or_default(),
                parameters,
                effect,
                approval_required: effect != ToolEffect::Read,
            });
        }
        declarations.extend(resources.inventory.tools.iter().map(|tool| HarnessTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
            effect: ToolEffect::Unknown,
            approval_required: true,
        }));
        let workspace = tools.root().to_str().ok_or("workspace must be UTF-8")?;
        let manifest = HarnessManifest {
            tool_manifest_digest: HarnessManifest::digest(&declarations)
                .map_err(|error| error.to_string())?,
            tools: declarations,
            workspace_id: format!("workspace_{}", sha256(workspace.as_bytes())),
            workspace_revision: None,
            permission_revision: 1,
            max_tool_output_bytes: 512 * 1024,
            artifact_quota_bytes: 32 * 1024 * 1024,
            max_artifact_chunk_bytes: 32 * 1024,
            required_features: vec![],
        };
        let mut materials: Vec<_> = resources
            .inventory
            .instructions
            .iter()
            .chain(
                resources
                    .inventory
                    .skills
                    .iter()
                    .map(|skill| &skill.material),
            )
            .map(|material| MaterialRef {
                material_id: material.material_id.clone(),
                version: material.version.clone(),
                sha256: material.sha256.clone(),
                media_type: material.media_type.clone(),
                provenance: material.provenance.clone(),
                required: false,
                artifact: None,
                content: None,
            })
            .collect();
        let body = format!("{}\n\n{}", instructions::POLICY, startup.body);
        let digest = sha256(body.as_bytes());
        materials.push(MaterialRef {
            material_id: "native_startup_instructions".into(),
            version: digest.clone(),
            sha256: digest,
            media_type: "text/markdown".into(),
            provenance: "native_startup_instruction_snapshot".into(),
            required: true,
            artifact: None,
            content: Some(body),
        });
        let signals = SignalUpdate {
            signal_revision: 1,
            observed_at: crate::store::owner_time_ms()?.to_string(),
            scope: manifest.workspace_id.clone(),
            source: "native_harness".into(),
            workspace_revision: None,
            manifest,
            materials,
            facts: BTreeMap::from([
                ("skill_catalog".into(), json!(resources.inventory.skills)),
                (
                    "skill_problems".into(),
                    json!(resources.inventory.skill_problems),
                ),
                ("startup_instructions".into(), json!(startup.materials)),
                ("instruction_warnings".into(), json!(startup.warnings)),
            ]),
        };
        // Leave room for the operation envelope before any task is accepted.
        if serde_json::to_vec(&signals)
            .map_err(|error| error.to_string())?
            .len() as u64
            > Self::limits().input_bytes.saturating_sub(4096)
        {
            return Err("native inventory exceeds managed control capacity".into());
        }
        Ok(signals)
    }

    pub fn manifest(&self) -> &HarnessManifest {
        &self.signals.manifest
    }

    pub fn signals(&self, grant: &OwnershipGrant, revision: u64) -> SignalUpdate {
        let mut update = self.signals.clone();
        update.scope = grant.session_id.clone();
        update.source = grant.harness_id.clone();
        update.signal_revision = revision;
        update
    }

    pub async fn material(&self, id: &str, version: &str) -> Result<MaterialRef, String> {
        let mut material = self
            .signals
            .materials
            .iter()
            .find(|material| material.material_id == id && material.version == version)
            .cloned()
            .ok_or("material is not in the frozen native inventory")?;
        if material.content.is_none() {
            let body = if let Some(path) = material.provenance.strip_prefix("local_skill:") {
                let path = std::path::PathBuf::from(path);
                tokio::task::spawn_blocking(move || {
                    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
                    if !file
                        .metadata()
                        .map_err(|error| error.to_string())?
                        .is_file()
                    {
                        return Err("skill must remain a regular file".into());
                    }
                    let mut bytes = Vec::new();
                    file.take(256 * 1024 + 1)
                        .read_to_end(&mut bytes)
                        .map_err(|error| error.to_string())?;
                    if bytes.len() > 256 * 1024 {
                        return Err("skill exceeds 256 KiB".into());
                    }
                    String::from_utf8(bytes).map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| error.to_string())??
            } else {
                self.resources
                    .instruction_body(id)
                    .await
                    .ok_or("MCP instruction body unavailable")?
            };
            if sha256(body.as_bytes()) != material.sha256 {
                return Err("native material changed since inventory discovery".into());
            }
            material.content = Some(body);
        }
        Ok(material)
    }

    /// Execute only after the host has committed a matching start record and
    /// approval. This validation supplements, rather than replaces, that ledger.
    pub async fn execute(
        &self,
        command: &ToolExecute,
        cancel: &CancellationToken,
    ) -> Result<ToolResult, CoreError> {
        let manifest = &self.signals.manifest;
        let invalid = |message| CoreError::rejected(ErrorCode::InvalidToolResult, message);
        if command.tool_manifest_digest != manifest.tool_manifest_digest
            || command.workspace_id != manifest.workspace_id
            || command.permission_revision != manifest.permission_revision
            || !manifest.tools.iter().any(|tool| tool.name == command.tool)
        {
            return Err(invalid(
                "native tool does not match the authorized inventory",
            ));
        }
        let limits = command
            .result_limits
            .ok_or_else(|| invalid("legacy native tool requires explicit result admission"))?;
        if limits.output_bytes < manifest.max_tool_output_bytes {
            return Err(invalid("native tool result allowance is too small"));
        }
        let arguments = serde_json::to_string(&command.arguments)
            .map_err(|_| invalid("invalid tool arguments"))?;
        let (output, effect) = if self.resources.contains(&command.tool) {
            self.resources
                .execute(&command.tool, &arguments, cancel)
                .await
        } else {
            self.tools
                .execute_with_effect(
                    &command.tool,
                    &arguments,
                    cancel,
                    &command.invocation_id,
                    None,
                )
                .await
        };
        let status = match effect {
            EffectStatus::Unknown => ToolOutcome::EffectUnknown,
            EffectStatus::NotExecuted => ToolOutcome::NotExecuted,
            EffectStatus::Completed => match &output {
                ToolResultOutput::ErrorJson { .. } | ToolResultOutput::ErrorText { .. } => {
                    ToolOutcome::Failed
                }
                _ => ToolOutcome::Succeeded,
            },
        };
        let result = ToolResult {
            invocation_id: command.invocation_id.clone(),
            attempt_id: command.attempt_id.clone(),
            status,
            output: serde_json::to_string(&output)
                .map_err(|_| invalid("tool result serialization failed"))?,
            evidence: vec![],
            workspace_revision: None,
        };
        limits.validate_result(&result)?;
        Ok(result)
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.resources.shutdown().await
    }
}
