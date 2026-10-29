//! Complete context compilation; historical decision shapes remain readable.

use bitrouter_ai::types::Prompt;

use crate::storage::checkpoint::snapshot::context::view::{
    ContextRepresentation, ContextView, SourceRevision,
};
use crate::storage::checkpoint::snapshot::context::{ContextStore, digest, invalid};
use bitrouter_ai::core::protocol::{CoreError, ErrorCode, HarnessManifest};

impl SourceRevision {
    pub fn capture(
        context_revision: u64,
        signal_revision: u64,
        manifest: &HarnessManifest,
        prompt: &Prompt,
    ) -> Result<Self, CoreError> {
        Ok(Self {
            context_revision,
            signal_revision,
            manifest_sha256: digest(manifest)?,
            prompt_sha256: digest(prompt)?,
        })
    }
}

/// Compile every admitted evidence group without semantic omission or substitution.
pub fn compile_full(
    store: &ContextStore,
    task_id: &str,
    ordered_blocks: Vec<String>,
    original: &Prompt,
    source: SourceRevision,
    hard_limit_bytes: usize,
) -> Result<(ContextView, Prompt), CoreError> {
    let work = store
        .work
        .get(task_id)
        .ok_or_else(|| invalid("context task is absent"))?;
    let mut selected = Vec::new();
    let mut prompt = original.clone();
    prompt.messages.clear();
    for id in &ordered_blocks {
        let block = store
            .evidence
            .get(id)
            .ok_or_else(|| invalid("context source is absent"))?;
        selected.push(ContextRepresentation::Full {
            block_id: id.clone(),
        });
        if work.agent_id != block.source.agent_id {
            prompt.messages.push(super::tasks::render(block)?);
        } else {
            prompt.messages.extend(block.messages.clone());
        }
    }
    crate::gateway::model::history::validate_history(&prompt.messages).map_err(invalid)?;
    let prompt_bytes = serde_json::to_vec(&prompt)
        .map_err(|error| invalid(error.to_string()))?
        .len();
    if prompt_bytes > hard_limit_bytes {
        return Err(CoreError::rejected(
            ErrorCode::NoFeasibleRoute,
            "full context exceeds the prompt byte limit",
        ));
    }
    let mut view = ContextView {
        view_id: String::new(),
        task_id: task_id.into(),
        source,
        required: ordered_blocks.clone(),
        source_blocks: ordered_blocks,
        selected,
        omitted: Vec::new(),
        decision_id: None,
        reason: "explicit_context_only".into(),
        prompt_sha256: digest(&prompt)?,
        prompt_bytes,
    };
    view.view_id = view_identity(&view)?;
    Ok((view, prompt))
}

pub(crate) fn view_identity(view: &ContextView) -> Result<String, CoreError> {
    let mut value = view.clone();
    value.view_id.clear();
    Ok(format!("view_{}", digest(&value)?))
}

pub(crate) fn truncate(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &text[..end]
}
