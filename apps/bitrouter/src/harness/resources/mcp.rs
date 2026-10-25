//! Harness adaptation of bounded application MCP connections.
//! Tool authority, effect outcomes and instruction provenance stay on the host.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use crate::config::mcp::McpServerConfig;
use crate::gateway::mcp::client::{BoundedMcpClient, CatalogTool, ConnectError, Discovery};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientRequest, ProtocolVersion, ServerResult,
};
use rmcp::service::PeerRequestOptions;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::ResourceError;
use super::sha256;
use bitrouter_ai::harness::resources::MaterialRef;
use bitrouter_ai::harness::resources::McpTool;
use bitrouter_ai::harness::runtime::EffectStatus;

const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// One MCP client principal per harness resource owner, never a gateway pool.
/// Server annotations do not grant local read-only or approval permissions.
#[derive(Default)]
pub struct McpConnections {
    client: BoundedMcpClient,
    pub(super) tools: Vec<McpTool>,
    pub(super) instructions: Vec<MaterialRef>,
    pub(super) instruction_bodies: BTreeMap<String, String>,
}

impl McpConnections {
    #[cfg(all(test, feature = "core"))]
    pub(crate) fn set_discovery_gate(
        &mut self,
        gate: Option<std::sync::Arc<crate::tenancy::hosts::HostGate>>,
    ) {
        self.client.discovery_gate = gate;
    }
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    pub fn validate_catalog(&self) -> Result<(), String> {
        self.client.validate_catalog()
    }

    pub async fn connect(
        workspace: &Path,
        servers: &[McpServerConfig],
        version: ProtocolVersion,
    ) -> Result<Self, ResourceError> {
        BoundedMcpClient::connect(workspace, servers, version)
            .await
            .map(Self::from_client)
            .map_err(resource_error)
    }

    pub async fn list_skills(
        &self,
        server: &str,
        access: &crate::skills::mcp::SkillAccess<'_>,
        cancel: &CancellationToken,
    ) -> Result<bitrouter_ai::harness::skills::McpSkillCatalog, String> {
        self.client.list_skills(server, access, cancel).await
    }

    pub async fn get_skill(
        &self,
        server: &str,
        uri: &str,
        access: &crate::skills::mcp::SkillAccess<'_>,
        cancel: &CancellationToken,
    ) -> Result<bitrouter_ai::harness::skills::HeldMcpSkill, String> {
        self.client.get_skill(server, uri, access, cancel).await
    }

    pub async fn read_skill_file(
        &self,
        held: &bitrouter_ai::harness::skills::HeldMcpSkill,
        access: &crate::skills::mcp::SkillAccess<'_>,
        read: crate::skills::mcp::SkillRead<'_>,
        cancel: &CancellationToken,
    ) -> Result<crate::skills::mcp::VerifiedSkillFile, String> {
        self.client
            .read_skill_file(held, access, read, cancel)
            .await
    }

    pub(crate) async fn connect_in_place(
        &mut self,
        workspace: &Path,
        servers: &[McpServerConfig],
        tool_servers: &std::collections::BTreeSet<String>,
        version: ProtocolVersion,
        cancel: &CancellationToken,
        max_duration: Duration,
        discovery: &Discovery,
    ) -> Result<(), ResourceError> {
        self.client
            .connect_in_place(
                workspace,
                servers,
                tool_servers,
                version,
                cancel,
                max_duration,
                discovery,
            )
            .await
            .map_err(resource_error)?;
        self.refresh_metadata();
        Ok(())
    }

    fn from_client(client: BoundedMcpClient) -> Self {
        let mut connections = Self {
            client,
            ..Self::default()
        };
        connections.refresh_metadata();
        connections
    }

    fn refresh_metadata(&mut self) {
        self.tools = self.client.tools().iter().map(harness_tool).collect();
        self.instructions.clear();
        self.instruction_bodies.clear();
        for instruction in self.client.instructions() {
            let digest = sha256(instruction.text.as_bytes());
            let material_id = format!("mcp_instructions_{}", sha256(instruction.server.as_bytes()));
            self.instruction_bodies
                .insert(material_id.clone(), instruction.text.clone());
            self.instructions.push(MaterialRef {
                material_id,
                version: digest.clone(),
                sha256: digest,
                media_type: "text/plain".into(),
                provenance: format!("mcp_server:{}", instruction.server),
            });
        }
    }

    /// Called only after host permission, durable invocation/start-fence and
    /// ownership checks. A timeout or cancellation after send is an unknown
    /// effect, never an instruction to retry tools/call.
    pub async fn call(
        &self,
        name: &str,
        arguments: &Value,
        output_bytes: u64,
        cancel: &CancellationToken,
    ) -> Result<(EffectStatus, String), String> {
        let (upstream_name, peer) = self.client.tool_peer(name)?;
        if cancel.is_cancelled() {
            return Ok((
                EffectStatus::NotExecuted,
                "cancelled before MCP dispatch".into(),
            ));
        }
        let args = arguments
            .as_object()
            .cloned()
            .ok_or_else(|| reject("MCP arguments must be an object"))?;
        let params = CallToolRequestParams::new(upstream_name.to_owned()).with_arguments(args);
        let deadline = tokio::time::Instant::now() + CALL_TIMEOUT;
        let send = peer.send_request_with_option(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            PeerRequestOptions::no_options(),
        );
        let mut handle = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok((EffectStatus::Unknown, "MCP dispatch interrupted; reconcile the upstream effect".into())),
            _ = tokio::time::sleep_until(deadline) => return Ok((EffectStatus::Unknown, "MCP dispatch timed out; reconcile the upstream effect".into())),
            result = send => match result {
                Ok(handle) => handle,
                Err(_) => return Ok((EffectStatus::Unknown, "MCP dispatch failed; reconcile the upstream effect".into())),
            },
        };
        // Keep the handle while waiting so cancellation retires the request
        // through rmcp's own cancellation API. No progress/subscription options
        // are installed for this single synchronous tools/call.
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            _ = tokio::time::sleep_until(deadline) => None,
            response = &mut handle.rx => Some(response),
        };
        let response = match response {
            Some(Ok(response)) => response,
            Some(Err(_)) => {
                return Ok((
                    EffectStatus::Unknown,
                    "MCP connection ended before an outcome; reconcile the upstream effect".into(),
                ));
            }
            None => {
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    handle.cancel(Some("harness cancellation or deadline".into())),
                )
                .await;
                return Ok((
                    EffectStatus::Unknown,
                    "MCP call cancelled or timed out; reconcile the upstream effect".into(),
                ));
            }
        };
        let result = match response {
            Ok(ServerResult::CallToolResult(result)) => result,
            _ => {
                return Ok((
                    EffectStatus::Unknown,
                    "MCP call did not return a complete outcome; reconcile the upstream effect"
                        .into(),
                ));
            }
        };
        let Ok(output) = serde_json::to_string(&result) else {
            return Ok((
                EffectStatus::Unknown,
                "MCP outcome could not be retained; reconcile the upstream effect".into(),
            ));
        };
        if output.len() as u64 > output_bytes {
            return Ok((
                EffectStatus::Unknown,
                "MCP result exceeds the frozen output allowance; reconcile the upstream effect"
                    .into(),
            ));
        }
        let status = EffectStatus::Completed;
        Ok((status, output))
    }

    #[cfg(all(test, unix, feature = "core"))]
    pub(crate) async fn retained_processes_for_test(&self) -> Vec<u32> {
        self.client.retained_processes_for_test().await
    }

    pub async fn shutdown(&mut self) -> Result<(), String> {
        self.tools.clear();
        self.instructions.clear();
        self.client.shutdown().await
    }
}

fn harness_tool(tool: &CatalogTool) -> McpTool {
    McpTool {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: tool.parameters.clone(),
        effect: EffectStatus::Unknown,
        approval_required: true,
    }
}

fn resource_error(error: ConnectError) -> ResourceError {
    ResourceError {
        message: error.message,
        cleanup_unknown: error.cleanup_unknown,
    }
}

fn reject(message: impl Into<String>) -> String {
    message.into()
}

#[cfg(test)]
mod tests {
    use super::{CatalogTool, EffectStatus, harness_tool};
    use serde_json::json;

    #[test]
    fn shared_catalog_budget_matches_harness_declarations() -> Result<(), serde_json::Error> {
        for description in ["", "quoted \"text\"\n雪"] {
            let tool = CatalogTool {
                name: "fixture__echo".into(),
                description: description.into(),
                parameters: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            };
            let declaration = harness_tool(&tool);
            assert_eq!(
                tool.catalog_bytes()?,
                serde_json::to_vec(&declaration)?.len()
            );
            assert_eq!(declaration.effect, EffectStatus::Unknown);
            assert!(declaration.approval_required);
        }
        Ok(())
    }
}
