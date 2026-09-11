//! A judge sees a frozen, cited ACP prefix, without model prices or routing labels.
//!
//! Protocol references: <https://agentclientprotocol.com/protocol/tool-calls> and
//! <https://agentclientprotocol.com/protocol/terminals>. Only recorded envelopes
//! are projected; these methods are never invoked by evaluation.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use bitrouter_sdk::acp::capture::{CaptureDirection, CaptureKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::acp_trajectory::checkpoint::types::{CheckpointContent, EvidenceCitation};

pub const EVIDENCE_VERSION: &str = "recorded-acp-quality-evidence-v2";

fn legacy_projection_version() -> String {
    "recorded-acp-quality-evidence-v1".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    UserMessage,
    AgentMessage,
    ToolCall,
    ToolObservation,
    FileWrite,
    SessionBoundary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceItem {
    pub citation: EvidenceCitation,
    pub kind: EvidenceKind,
    pub method: String,
    /// Call IDs are scoped to the connection encoded by the original node ID.
    /// Keeping them lets the judge join terminal creation, output and exit
    /// observations without inventing a command from its output alone.
    pub call_id: Option<u64>,
    pub content: Value,
}

/// This packet contains potentially sensitive conversation content. Constructing
/// it is local; sending it is exclusively an explicit evaluator operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidencePacket {
    #[serde(default = "legacy_projection_version")]
    pub projection_version: String,
    pub checkpoint_id: String,
    pub prefix_digest: String,
    pub gaps: Vec<String>,
    pub items: Vec<EvidenceItem>,
}

impl EvidencePacket {
    pub fn from_checkpoint(content: &CheckpointContent) -> Result<Self> {
        let refs: BTreeMap<_, _> = content
            .checkpoint
            .segments
            .iter()
            .flat_map(|s| s.events.iter().chain(&s.setup))
            .map(|r| (r.node_id(), r.digest.clone()))
            .collect();
        let mut items = Vec::new();
        // Content order is the canonical inherited-prefix order, not wall-clock
        // order. Do not deduplicate by text or tool ID across connections.
        for node in &content.events {
            let event = &node.event;
            let update = event.payload.get("update");
            let update_kind = update
                .and_then(|u| u.get("sessionUpdate"))
                .and_then(Value::as_str);
            let (kind, body) = match (event.method.as_str(), event.kind, update_kind) {
                ("session/prompt", CaptureKind::Request, _)
                    if event.direction == CaptureDirection::Client =>
                {
                    (EvidenceKind::UserMessage, event.payload.get("prompt"))
                }
                ("session/update", _, Some("user_message_chunk")) => (
                    EvidenceKind::UserMessage,
                    update.and_then(|u| u.get("content")),
                ),
                ("session/update", _, Some("agent_message_chunk")) => (
                    EvidenceKind::AgentMessage,
                    update.and_then(|u| u.get("content")),
                ),
                ("session/update", _, Some("tool_call")) => {
                    let completed = update
                        .and_then(|u| u.get("status"))
                        .and_then(Value::as_str)
                        .is_some_and(|status| matches!(status, "completed" | "failed"));
                    (
                        if completed {
                            EvidenceKind::ToolObservation
                        } else {
                            EvidenceKind::ToolCall
                        },
                        update,
                    )
                }
                ("session/update", _, Some("tool_call_update")) => {
                    (EvidenceKind::ToolObservation, update)
                }
                ("fs/write_text_file", CaptureKind::Request, _) => {
                    (EvidenceKind::FileWrite, Some(&event.payload))
                }
                (method, CaptureKind::Request, _)
                    if method.starts_with("terminal/") || method.starts_with("fs/") =>
                {
                    (EvidenceKind::ToolCall, Some(&event.payload))
                }
                (method, CaptureKind::Response, _)
                    if method.starts_with("terminal/") || method.starts_with("fs/") =>
                {
                    (EvidenceKind::ToolObservation, Some(&event.payload))
                }
                ("session/prompt", CaptureKind::Response, _) => {
                    (EvidenceKind::SessionBoundary, Some(&event.payload))
                }
                _ => continue,
            };
            let Some(body) = body else { continue };
            let mut body = body.clone();
            // Identity and routing metadata are not quality evidence. Never
            // include setup/provider descriptors or private thought chunks.
            if let Some(object) = body.as_object_mut() {
                object.remove("sessionId");
                object.remove("_meta");
            }
            if kind == EvidenceKind::SessionBoundary {
                body = project_boundary(&body);
            }
            let digest = refs.get(&node.node_id).ok_or_else(|| {
                anyhow::anyhow!("event has no checkpoint reference: {}", node.node_id)
            })?;
            items.push(EvidenceItem {
                citation: EvidenceCitation {
                    node_id: node.node_id.clone(),
                    digest: digest.clone(),
                },
                kind,
                method: event.method.clone(),
                call_id: event.call_id,
                content: body,
            });
        }
        Ok(Self {
            projection_version: EVIDENCE_VERSION.into(),
            checkpoint_id: content.checkpoint.checkpoint_id.clone(),
            prefix_digest: content.checkpoint.prefix_digest.clone(),
            gaps: content.checkpoint.gaps.clone(),
            items,
        })
    }

    pub fn validate_citations(&self, citations: &[EvidenceCitation]) -> Result<()> {
        for citation in citations {
            ensure!(
                self.items
                    .iter()
                    .any(|item| item.citation.node_id == citation.node_id
                        && item.citation.digest == citation.digest),
                "citation is outside the judge's frozen evidence: {}",
                citation.node_id
            );
        }
        Ok(())
    }

    pub fn has_tool_observation(&self, citations: &[EvidenceCitation]) -> bool {
        citations.iter().any(|citation| {
            self.items.iter().any(|item| {
                item.citation.node_id == citation.node_id
                    && item.citation.digest == citation.digest
                    && item.kind == EvidenceKind::ToolObservation
            })
        })
    }
}

/// Only the protocol stop/error is quality evidence. Usage and adapter metadata
/// live inside the result envelope too. Do not recursively strip arbitrary tool
/// results or user text: a task can legitimately concern a field named `model`.
fn project_boundary(payload: &Value) -> Value {
    let mut projected = serde_json::Map::new();
    for (envelope, fields) in [
        ("result", ["stopReason"].as_slice()),
        ("error", ["code", "message"].as_slice()),
    ] {
        if let Some(value) = payload.get(envelope) {
            let selected = fields
                .iter()
                .filter_map(|field| {
                    value
                        .get(field)
                        .map(|value| ((*field).to_owned(), value.clone()))
                })
                .collect();
            projected.insert(envelope.into(), Value::Object(selected));
        }
    }
    Value::Object(projected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn boundary_projection_preserves_failure_without_usage_or_routing_metadata() {
        let failure = json!({"error":{"code":-32000,"message":"Provider unavailable","data":{"model":"private-model"},"_meta":{"cost":42}},"_meta":{"route":"private-route"}});
        assert_eq!(
            project_boundary(&failure),
            json!({"error":{"code":-32000,"message":"Provider unavailable"}})
        );
        assert_eq!(
            project_boundary(&json!({"result":{"_meta":{"model":"private-model"}}})),
            json!({"result":{}})
        );
    }
}
