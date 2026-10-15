//! Deterministic reconstruction of authorized execution context. Current
//! work, instructions and required materials survive. Preparation-added
//! messages require a separate dependency contract and prevent reconstruction.
//! Completion alone never makes evidence optional. Original history remains in
//! the rejected step's immutable snapshot.

use crate::storage::checkpoint::snapshot::reconstruction::PortableReasoningRecord;

use crate::gateway::pipeline::native::NativePlan;
use crate::gateway::pipeline::native_projection::responses_reasoning;
use bitrouter_ai::types::{Message, Prompt, Role};
use serde::Serialize;

use crate::storage::checkpoint::snapshot::{ModelStep, SessionSnapshot};

use super::session::selected_materials;
use crate::storage::checkpoint::sha256;
use bitrouter_ai::core::protocol::{ContextMode, CoreError, ErrorCode};

pub(crate) struct RebuiltContext {
    pub prompt: Prompt,
    pub history: Vec<Message>,
    pub removed_messages: usize,
    pub removed_settled_messages: usize,
    pub portable_reasoning: Option<PortableReasoningRecord>,
}

impl RebuiltContext {
    pub fn strategy(&self) -> &'static str {
        if self.portable_reasoning.is_some() {
            crate::storage::checkpoint::snapshot::context::PORTABLE_RESPONSES_REASONING
        } else {
            "explicitly_optional_history_v1"
        }
    }
}

pub(crate) fn candidate(
    state: &SessionSnapshot,
    agent_id: &str,
    step: &ModelStep,
    plan: &NativePlan,
) -> Result<RebuiltContext, CoreError> {
    let agent = state
        .agents
        .get(agent_id)
        .ok_or_else(|| invalid("unknown rebuild agent"))?;
    let turn = agent
        .turn
        .as_ref()
        .ok_or_else(|| invalid("unknown rebuild turn"))?;
    if turn.input.routing.context != ContextMode::Auto {
        return Err(invalid(
            "fixed context mode does not permit automatic reconstruction",
        ));
    }
    let materials = selected_materials(state, &turn.input)?;
    if agent.history != step.input_history || step.materials != materials {
        return Err(invalid("context reconstruction source changed"));
    }
    step.context.validate_prepared(&plan.prompt)?;
    if state.manifest.required_features.iter().any(|feature| {
        feature == crate::storage::checkpoint::snapshot::context::PORTABLE_RESPONSES_REASONING
    }) && !state
        .context_store
        .executions
        .get(&step.step_id)
        .and_then(|execution| execution.routing.as_ref())
        .is_some_and(|routing| routing.strategy == bitrouter_ai::routing::ContextStrategy::Preserve)
        && step.decision.as_ref().is_some_and(|decision| {
            decision.routes.iter().any(|route| {
                route.rejection_reasons == ["protocol_incompatible:private_context_target_mismatch"]
            })
        })
        && let Some(history) =
            responses_reasoning(&step.input_history).map_err(|error| invalid(error.to_string()))?
        && let Some(view) = responses_reasoning(&plan.prompt.messages)
            .map_err(|error| invalid(error.to_string()))?
    {
        let start = turn
            .history_start
            .filter(|start| *start <= agent.history.len())
            .ok_or_else(|| invalid("portable context has no valid history boundary"))?;
        crate::gateway::model::history::validate_history(&history.messages).map_err(invalid)?;
        crate::gateway::model::history::validate_history(&view.messages).map_err(invalid)?;
        let mut prompt = plan.prompt.clone();
        prompt.messages = view.messages;
        return Ok(RebuiltContext {
            prompt,
            history: history.messages,
            removed_messages: history.record.removed_messages.len(),
            removed_settled_messages: history
                .record
                .removed_messages
                .iter()
                .filter(|index| **index < start)
                .count(),
            portable_reasoning: Some(PortableReasoningRecord {
                history: history.record,
                prompt: view.record,
            }),
        });
    }
    if materials.is_empty() || materials.iter().any(|material| material.content.is_none()) {
        return Err(invalid(
            "context reconstruction requires available versioned required material",
        ));
    }
    let start = turn
        .history_start
        .filter(|start| *start > 0 && *start <= agent.history.len())
        .ok_or_else(|| invalid("no settled history boundary is available for reconstruction"))?;
    let optional = turn
        .input
        .discardable_history
        .as_ref()
        .ok_or_else(|| invalid("task has not declared any settled history discardable"))?;
    if optional.history_sha256 != digest(&agent.history[..start])? {
        return Err(invalid(
            "discardable history declaration does not match the task's source history",
        ));
    }
    if optional.message_indices.is_empty()
        || optional
            .message_indices
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        || optional.message_indices.iter().any(|index| {
            *index >= start || !matches!(agent.history[*index].role, Role::Assistant | Role::Tool)
        })
    {
        return Err(invalid(
            "discardable indices must identify ordered settled assistant/tool messages",
        ));
    }
    let required_source_len = step.materials.len() + step.input_history.len();
    if required_source_len != step.context.message_sha256.len() {
        return Err(invalid(
            "context reconstruction source manifest is inconsistent",
        ));
    }
    if plan.prompt.messages.len() != required_source_len {
        return Err(invalid(
            "preparation added context outside the task's history declaration",
        ));
    }
    // Dropping a complete settled call/result batch cannot break an active pair.
    crate::gateway::model::history::validate_history(&agent.history[..start]).map_err(invalid)?;
    crate::gateway::model::history::validate_history(&agent.history[start..]).map_err(invalid)?;
    let removable = agent
        .history
        .iter()
        .enumerate()
        .map(|(index, _)| optional.message_indices.binary_search(&index).is_ok())
        .collect::<Vec<_>>();
    let removed_messages = removable.iter().filter(|remove| **remove).count();
    if removed_messages == 0 {
        return Err(invalid(
            "required context cannot be reduced at the settled history boundary",
        ));
    }
    let history = agent
        .history
        .iter()
        .zip(&removable)
        .filter(|(_, remove)| !**remove)
        .map(|(message, _)| message.clone())
        .collect::<Vec<_>>();
    crate::gateway::model::history::validate_history(&history).map_err(invalid)?;
    let mut source_index = 0;
    let mut messages = Vec::new();
    for message in &plan.prompt.messages {
        let commitment = digest(message)?;
        let matched = step.context.message_sha256.get(source_index) == Some(&commitment);
        let remove = matched
            && source_index
                .checked_sub(step.materials.len())
                .and_then(|index| removable.get(index))
                .copied()
                .unwrap_or(false);
        if matched {
            source_index += 1;
        }
        if !remove {
            messages.push(message.clone());
        }
    }
    if source_index != required_source_len {
        return Err(invalid(
            "prepared context no longer contains the committed source",
        ));
    }
    // Removing optional history must still leave complete call/result pairs.
    crate::gateway::model::history::validate_history(&messages).map_err(invalid)?;
    let mut prompt = plan.prompt.clone();
    prompt.messages = messages;
    Ok(RebuiltContext {
        prompt,
        history,
        removed_messages,
        removed_settled_messages: removed_messages,
        portable_reasoning: None,
    })
}

pub(crate) fn digest(value: &(impl Serialize + ?Sized)) -> Result<String, CoreError> {
    serde_json::to_vec(value)
        .map(|bytes| sha256(&bytes))
        .map_err(|error| CoreError::rejected(ErrorCode::CheckpointConflict, error.to_string()))
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::NoFeasibleRoute, message)
}
