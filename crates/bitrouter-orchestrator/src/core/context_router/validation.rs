//! Checkpoint validation for immutable evidence and irreversible decision facts.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::planner::ContextRepresentation;
use super::{ContextStore, FEATURE, digest, invalid};
use crate::core::checkpoint::CheckpointPayload;
use crate::core::protocol::CoreError;
use crate::core::session::SessionSnapshot;

pub(crate) fn validate(state: &SessionSnapshot) -> Result<(), CoreError> {
    let store = &state.context_store;
    if !state
        .manifest
        .required_features
        .iter()
        .any(|feature| feature == FEATURE)
        && (!store.work.is_empty()
            || !store.extracts.is_empty()
            || !store.artifacts.is_empty()
            || !store.evidence.is_empty()
            || !store.derived.is_empty()
            || !store.views.is_empty()
            || !store.decisions.is_empty()
            || !store.executions.is_empty())
    {
        return Err(invalid("context state requires explicit view negotiation"));
    }
    for (id, block) in &store.evidence {
        block.validate()?;
        if id != &block.block_id {
            return Err(invalid("evidence map identity differs"));
        }
    }
    for (id, artifact) in &store.artifacts {
        crate::core::checkpoint::validate_digest(&artifact.reference.sha256)?;
        if *id
            != format!(
                "{}:{}",
                artifact.source.task_id, artifact.reference.artifact_id
            )
            || !store.work.contains_key(&artifact.source.task_id)
        {
            return Err(invalid("tool artifact has no immutable task binding"));
        }
    }
    for (id, work) in &store.work {
        if id != &work.task_id
            || work.text.is_empty()
            || work
                .evidence
                .iter()
                .any(|id| !store.evidence.contains_key(id))
            || work.recalled.iter().any(|id| !work.evidence.contains(id))
            || work.shared_evidence.iter().any(|id| {
                !work.evidence.contains(id)
                    || store
                        .evidence
                        .get(id)
                        .is_none_or(|block| !super::tasks::portable(block))
            })
            || work
                .result_evidence
                .iter()
                .any(|id| !store.evidence.contains_key(id))
            || work.last_view_id.as_ref().is_some_and(|id| {
                store
                    .views
                    .get(id)
                    .is_none_or(|view| view.task_id != work.task_id)
            })
        {
            return Err(invalid("task evidence inventory is invalid"));
        }
    }
    for (id, summary) in &store.derived {
        let expected = format!(
            "summary_{}",
            digest(&(&summary.task_sha256, &summary.source_blocks, &summary.text))?
        );
        if id != &summary.artifact_id
            || *id != expected
            || summary.source_blocks.is_empty()
            || summary
                .source_blocks
                .iter()
                .any(|id| store.evidence.get(id).is_none_or(|block| block.protected))
        {
            return Err(invalid("derived evidence identity or source changed"));
        }
    }
    for (id, extract) in &store.extracts {
        store.extract(&extract.block_id, &extract.spans)?;
        if id != &extract.extract_id
            || *id
                != format!(
                    "extract_{}",
                    digest(&(&extract.task_sha256, &extract.block_id, &extract.spans))?
                )
        {
            return Err(invalid("source extract commitment changed"));
        }
    }
    for (id, receipt) in &store.decisions {
        receipt
            .request
            .validate()
            .map_err(|error| invalid(error.to_string()))?;
        receipt.policy.validate().map_err(invalid)?;
        if let Some(pricing) = &receipt.pricing {
            pricing.validate().map_err(invalid)?;
        }
        let work = store
            .work
            .get(&receipt.task_id)
            .ok_or_else(|| invalid("decision task is absent"))?;
        if id != &receipt.decision_id
            || receipt.request_sha256 != digest(&receipt.request)?
            || receipt.run_id != work.run_id
            || receipt.agent_id != work.agent_id
            || receipt.candidates.task_id != work.task_id
            || receipt.response_limit_bytes == 0
            || receipt.candidates.models.len() > 16
            || receipt
                .candidates
                .models
                .values()
                .any(|model| !receipt.policy.generation_models.contains(model))
            || receipt
                .candidates
                .ordered_blocks
                .iter()
                .any(|id| !work.evidence.contains(id))
            || receipt
                .candidates
                .required
                .iter()
                .any(|id| !receipt.candidates.ordered_blocks.contains(id))
            || receipt.view_ids.iter().any(|id| {
                store
                    .views
                    .get(id)
                    .is_none_or(|view| view.decision_id.as_ref() != Some(&receipt.decision_id))
            })
        {
            return Err(invalid("decision intent or task binding is invalid"));
        }
        if let Some(outcome) = &receipt.outcome {
            if serde_json::to_vec(outcome)
                .map_err(|error| invalid(error.to_string()))?
                .len()
                > receipt.response_limit_bytes
            {
                return Err(invalid("decision outcome exceeds its frozen allowance"));
            }
            if let Ok(response) = outcome {
                response
                    .validate(&receipt.request)
                    .map_err(|error| invalid(error.to_string()))?;
            }
        }
    }
    for (id, view) in &store.views {
        let work = store
            .work
            .get(&view.task_id)
            .ok_or_else(|| invalid("view task is absent"))?;
        let expected = super::planner::view_identity(view)?;
        if id != &view.view_id || *id != expected {
            return Err(invalid("context view commitment changed"));
        }
        let mut full = BTreeSet::new();
        let mut represented = BTreeSet::new();
        let mut selected_order = Vec::new();
        for representation in &view.selected {
            match representation {
                ContextRepresentation::Full { block_id } => {
                    full.insert(block_id);
                    represented.insert(block_id);
                    selected_order.push(block_id);
                }
                ContextRepresentation::Extract { block_id, spans } => {
                    store.extract(block_id, spans)?;
                    represented.insert(block_id);
                    selected_order.push(block_id);
                }
                ContextRepresentation::Summary { artifact_id } => {
                    let summary = store
                        .derived
                        .get(artifact_id)
                        .ok_or_else(|| invalid("view summary is absent"))?;
                    represented.extend(summary.source_blocks.iter());
                    selected_order.extend(summary.source_blocks.iter());
                }
            }
        }
        let expected_order: Vec<_> = view
            .source_blocks
            .iter()
            .filter(|id| !view.omitted.contains(id))
            .collect();
        if selected_order != expected_order
            || view.source_blocks.iter().any(|id| {
                store
                    .evidence
                    .get(id)
                    .is_none_or(|block| block.protected && !full.contains(id))
            })
            || view.required.iter().any(|id| !full.contains(id))
            || represented.iter().any(|id| !work.evidence.contains(id))
            || view.omitted.iter().any(|id| {
                represented.contains(id)
                    || store.evidence.get(id).is_none_or(|block| block.protected)
            })
            || represented.iter().any(|id| {
                store.evidence.get(*id).is_some_and(|block| block.protected) && !full.contains(id)
            })
        {
            return Err(invalid(
                "view removed required evidence or has invalid sources",
            ));
        }
        if let Some(decision_id) = &view.decision_id {
            let receipt = store
                .decisions
                .get(decision_id)
                .ok_or_else(|| invalid("view decision is absent"))?;
            if receipt.task_id != view.task_id
                || receipt.source != view.source
                || receipt.candidates.ordered_blocks != view.source_blocks
                || receipt.candidates.required.iter().collect::<Vec<_>>()
                    != view.required.iter().collect::<Vec<_>>()
                || receipt.stale
                || !receipt.view_ids.contains(id)
            {
                return Err(invalid("view is not bound to its acknowledged decision"));
            }
            if receipt.outcome.as_ref().is_none_or(Result::is_err) && !view.omitted.is_empty() {
                return Err(invalid(
                    "failed or interrupted decision cannot omit evidence",
                ));
            }
        } else if !view.omitted.is_empty() {
            return Err(invalid("context omission has no decision authority"));
        }
    }
    for (id, execution) in &store.executions {
        if id != &execution.step_id
            || store.views.get(&execution.view_id).is_none_or(|view| {
                view.task_id != execution.task_id || view.decision_id != execution.decision_id
            })
        {
            return Err(invalid("execution is not bound to a context view"));
        }
        if let Some(routing) = &execution.routing {
            let receipt = execution
                .decision_id
                .as_ref()
                .and_then(|id| store.decisions.get(id));
            if routing.candidates.len() > 32
                || routing.candidates.is_empty()
                || !routing.candidates.iter().any(|candidate| {
                    candidate.model == routing.selected_model
                        && candidate.context == routing.selected_context
                })
                || receipt.is_none_or(|receipt| {
                    !receipt
                        .candidates
                        .models
                        .values()
                        .any(|model| model.model == routing.selected_model)
                })
                || execution
                    .model
                    .as_ref()
                    .is_some_and(|model| model.original_model != routing.selected_model)
                || (receipt
                    .is_some_and(|receipt| receipt.outcome.as_ref().is_none_or(Result::is_err))
                    && routing.selected_model != routing.requested_model)
            {
                return Err(invalid(
                    "model/context selection differs from its frozen candidates or SDK binding",
                ));
            }
        }
    }
    Ok(())
}

/// Only hashes of immutable facts are retained while walking a journal tail.
#[derive(Clone, Default)]
pub(crate) struct History {
    immutable: BTreeMap<String, String>,
}

pub(crate) fn validate_history(
    payload: &CheckpointPayload,
    history: &mut History,
) -> Result<(), CoreError> {
    let store: ContextStore = match payload.checkpoint.state.get("context_store") {
        Some(value) => {
            serde_json::from_value(value.clone()).map_err(|error| invalid(error.to_string()))?
        }
        None => ContextStore::default(),
    };
    let mut current = BTreeMap::new();
    for (id, artifact) in &store.artifacts {
        remember(&mut current, "artifact", id, artifact)?;
    }
    for (id, block) in &store.evidence {
        remember(&mut current, "block", id, block)?;
    }
    for (id, summary) in &store.derived {
        remember(&mut current, "summary", id, summary)?;
    }
    for (id, extract) in &store.extracts {
        remember(&mut current, "extract", id, extract)?;
    }
    for (id, view) in &store.views {
        remember(&mut current, "view", id, view)?;
    }
    for (id, work) in &store.work {
        remember(
            &mut current,
            "task",
            id,
            &(
                &work.task_id,
                &work.run_id,
                &work.agent_id,
                &work.parent_task_id,
                &work.text,
                &work.acceptance_criteria,
            ),
        )?;
    }
    for (id, receipt) in &store.decisions {
        let mut intent = receipt.clone();
        intent.outcome = None;
        intent.elapsed_ms = None;
        intent.stale = false;
        intent.view_ids.clear();
        remember(&mut current, "decision", id, &intent)?;
        if let Some(outcome) = &receipt.outcome {
            remember(
                &mut current,
                "outcome",
                id,
                &(outcome, receipt.elapsed_ms, receipt.stale),
            )?;
        }
    }
    for (id, execution) in &store.executions {
        let mut intent = execution.clone();
        intent.model = None;
        remember(&mut current, "execution", id, &intent)?;
        if let Some(model) = &execution.model {
            remember(&mut current, "model", id, model)?;
        }
    }
    if history
        .immutable
        .iter()
        .any(|(id, digest)| current.get(id) != Some(digest))
    {
        return Err(invalid(
            "context journal removed or rewrote an immutable fact",
        ));
    }
    history.immutable = current;
    Ok(())
}

fn remember(
    map: &mut BTreeMap<String, String>,
    kind: &str,
    id: &str,
    value: &impl Serialize,
) -> Result<(), CoreError> {
    map.insert(format!("{kind}/{id}"), digest(value)?);
    Ok(())
}
