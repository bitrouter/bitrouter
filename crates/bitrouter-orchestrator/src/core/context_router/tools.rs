//! Model-facing evidence operations. Authority is checked against the task's
//! admitted evidence inventory, independently of semantic decisions.

use bitrouter_sdk::language_model::types::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::FEATURE;
use crate::core::protocol::{CoreError, ErrorCode};
use crate::core::session::SessionSnapshot;

pub const NAMES: &[&str] = &[
    "context_search",
    "context_recall",
    "context_publish",
    "context_extract",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "name", content = "arguments", deny_unknown_fields)]
pub enum ContextAction {
    #[serde(rename = "context_search")]
    Search { query: String },
    #[serde(rename = "context_recall")]
    Recall { block_ids: Vec<String> },
    #[serde(rename = "context_publish")]
    Publish {
        block_ids: Vec<String>,
        summary: String,
    },
    #[serde(rename = "context_extract")]
    Extract {
        block_id: String,
        spans: Vec<super::evidence::SourceSpan>,
    },
}

pub fn declarations(enabled: bool) -> Vec<Tool> {
    if !enabled {
        return Vec::new();
    }
    let ids = json!({"type":"array","items":{"type":"string"},"minItems":1,"maxItems":16});
    [
        ("context_search", "Search this task's immutable evidence, including material omitted from the current view. Results retain source versions; stale observations need re-reading before edits.", json!({"query":{"type":"string"}}), vec!["query"]),
        ("context_recall", "Pin complete source groups into the next task view. Use block IDs from context_search or a worker completion. Does not change evidence or workspace files.", json!({"block_ids":ids}), vec!["block_ids"]),
        ("context_publish", "Publish a query-specific summary of one optional evidence group you have read. State observed facts, unresolved questions and source limitations. The original source remains recallable.", json!({"block_ids":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":1},"summary":{"type":"string","maxLength":16384}}), vec!["block_ids","summary"]),
        ("context_extract", "Publish exact UTF-8 byte spans from optional evidence as a smaller representation. Message and content indices are zero-based within the evidence group; ends are exclusive. The original tool exchange remains intact.", json!({"block_id":{"type":"string"},"spans":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"object","properties":{"message_index":{"type":"integer","minimum":0},"content_index":{"type":"integer","minimum":0},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":1}},"required":["message_index","content_index","start","end"],"additionalProperties":false}}}), vec!["block_id","spans"]),
    ].into_iter().map(|(name, description, properties, required)| Tool::Function {
        name: name.into(), description: Some(description.into()),
        parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        strict: None, provider_metadata: Default::default(),
    }).collect()
}

pub fn apply(
    state: &mut SessionSnapshot,
    actor_id: &str,
    action: &ContextAction,
) -> Result<Value, CoreError> {
    if !state
        .manifest
        .required_features
        .iter()
        .any(|feature| feature == FEATURE)
    {
        return Err(CoreError::rejected(
            ErrorCode::UnsupportedCapability,
            "context views were not negotiated",
        ));
    }
    let task_id = state
        .agents
        .get(actor_id)
        .and_then(|agent| agent.turn.as_ref())
        .map(|turn| turn.agent_turn_id.clone())
        .ok_or_else(|| super::invalid("context task is absent"))?;
    let work = state
        .context_store
        .work
        .get_mut(&task_id)
        .ok_or_else(|| super::invalid("context task was not catalogued"))?;
    let result = match action {
        ContextAction::Extract { block_id, spans } => {
            if state
                .context_store
                .evidence
                .get(block_id)
                .is_none_or(|block| {
                    block.source.workspace_id != state.manifest.workspace_id
                        || block.source.permission_revision != state.manifest.permission_revision
                })
            {
                return Err(CoreError::rejected(
                    ErrorCode::UnauthorizedScope,
                    "extract source permission changed",
                ));
            }
            let extract_id =
                state
                    .context_store
                    .publish_extract(&task_id, block_id, spans.clone())?;
            Ok(json!({"extract_id":extract_id,"source_block":block_id}))
        }
        ContextAction::Search { query } => {
            if query.is_empty() || query.len() > 1024 {
                return Err(super::invalid("evidence query must be 1 to 1024 bytes"));
            }
            let query = query.to_lowercase();
            let mut entries = Vec::new();
            for block in work
                .evidence
                .iter()
                .filter_map(|id| state.context_store.evidence.get(id))
            {
                if block.source.workspace_id != state.manifest.workspace_id
                    || block.source.permission_revision != state.manifest.permission_revision
                {
                    continue;
                }
                let body = serde_json::to_string(&block.messages)
                    .map_err(|error| super::invalid(error.to_string()))?;
                if query == "*" || body.to_lowercase().contains(&query) {
                    entries.push(json!({"block_id":block.block_id,"source":block.source,
                        "current_workspace":block.source.is_current(&state.manifest),
                        "bytes":block.bytes,"protected":block.protected,
                        "preview":super::planner::truncate(&body, 768)}));
                }
                if entries.len() == 8 {
                    break;
                }
            }
            let artifacts: Vec<_> = state
                .context_store
                .artifacts
                .values()
                .filter(|artifact| {
                    artifact.visible(&state.context_store, &task_id, &state.manifest)
                })
                .take(8)
                .map(|artifact| &artifact.reference)
                .collect();
            Ok(json!({"task_id":task_id,"evidence":entries,"artifacts":artifacts,"limit":8}))
        }
        ContextAction::Recall { block_ids } | ContextAction::Publish { block_ids, .. } => {
            if block_ids.is_empty()
                || block_ids.len() > 16
                || block_ids.iter().any(|id| {
                    !work.evidence.contains(id)
                        || state.context_store.evidence.get(id).is_none_or(|block| {
                            block.source.workspace_id != state.manifest.workspace_id
                                || block.source.permission_revision
                                    != state.manifest.permission_revision
                        })
                })
            {
                return Err(CoreError::rejected(
                    ErrorCode::UnauthorizedScope,
                    "evidence is outside this task's inventory or permission revision",
                ));
            }
            if let ContextAction::Publish { summary, .. } = action {
                let artifact_id = state.context_store.publish_summary(
                    &task_id,
                    block_ids.clone(),
                    summary.clone(),
                )?;
                Ok(json!({"artifact_id":artifact_id,"source_blocks":block_ids}))
            } else {
                for id in block_ids {
                    if !work.recalled.contains(id) {
                        work.recalled.push(id.clone());
                    }
                }
                Ok(json!({"recalled":block_ids,"applies_to_next_view":true}))
            }
        }
    };
    if result.is_ok()
        && !matches!(action, ContextAction::Search { .. })
        && let Some(agent) = state.agents.get_mut(actor_id)
    {
        agent.context_revision = agent
            .context_revision
            .checked_add(1)
            .ok_or_else(|| super::invalid("context revision exhausted"))?;
    }
    result
}
