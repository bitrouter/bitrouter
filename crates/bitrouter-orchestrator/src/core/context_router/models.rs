//! Finite model/view planning. Suitability is semantic; capacity, cost and
//! switching decisions are deterministic. Prices and byte/token conversion
//! are local planning assumptions, never observed billing or cache evidence.

use bitrouter_sdk::decision_model::policy::{DecisionPolicy, GenerationModel};
use bitrouter_sdk::decision_model::types::{Answer, DecisionResponse};
use bitrouter_sdk::language_model::{Prompt, native::NativePlan};
use serde::{Deserialize, Serialize};

use super::planner::{CandidateSet, ContextView};
use super::{digest, invalid};
use crate::core::protocol::CoreError;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub model: String,
    pub context: String,
    pub prompt_bytes: usize,
    pub suitability: Option<f64>,
    pub eligible: bool,
    pub estimated_tokens: u64,
    pub estimated_cost_microusd: u64,
    pub exact_prefix_bytes: usize,
    pub objective_microusd: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Selection {
    pub requested_model: String,
    pub selected_model: String,
    pub selected_context: String,
    pub reason: String,
    pub candidates: Vec<Candidate>,
}

/// Caller-fixed model requests never populate model candidates.
pub(crate) fn candidates(set: &mut CandidateSet, policy: &DecisionPolicy, fallback: &str) {
    if !policy
        .generation_models
        .iter()
        .any(|model| model.model == fallback)
    {
        return;
    }
    set.models = policy
        .generation_models
        .iter()
        .enumerate()
        .map(|(index, model)| (format!("generation_{index:08}"), model.clone()))
        .collect();
}

pub(crate) struct Options<'a> {
    pub policy: &'a DecisionPolicy,
    pub response: Option<&'a DecisionResponse>,
    pub previous: Option<&'a NativePlan>,
    pub hard_limit_bytes: usize,
}

pub(crate) fn select(
    set: &CandidateSet,
    requested_model: &str,
    routed: (ContextView, Prompt),
    full: Option<(ContextView, Prompt)>,
    options: Options<'_>,
) -> Result<(ContextView, Prompt, Option<Selection>), CoreError> {
    if set.models.is_empty() || routed.0.decision_id.is_none() {
        return Ok((routed.0, routed.1, None));
    }
    let previous_model = options
        .previous
        .map_or(requested_model, |plan| plan.original_model.as_str());
    let previous_bytes = options
        .previous
        .map(|plan| prefix(&plan.prompt))
        .transpose()?;
    let mut views = vec![("routed", routed)];
    if let Some(full) = full {
        views.push(("full", full));
    }
    let mut candidates = Vec::new();
    let mut bindings = Vec::new();
    for (question, model) in &set.models {
        let suitability = options
            .response
            .and_then(|response| response.answers.get(question))
            .and_then(|answer| match answer {
                Answer::Noul { noul } => Some(*noul),
                _ => None,
            });
        for (index, (context, (_, original))) in views.iter().enumerate() {
            let mut prompt = original.clone();
            prompt.model.clone_from(&model.model);
            let prompt_bytes = serde_json::to_vec(&prompt)
                .map_err(|error| invalid(error.to_string()))?
                .len();
            let bytes = prefix(&prompt)?;
            let exact_prefix_bytes = if previous_model == model.model {
                previous_bytes.as_ref().map_or(0, |previous| {
                    previous
                        .iter()
                        .zip(&bytes)
                        .take_while(|(left, right)| left == right)
                        .count()
                })
            } else {
                0
            };
            let estimated_tokens = (prompt_bytes as u64).div_ceil(4);
            let estimated_cost_microusd = cost(
                model,
                estimated_tokens,
                u64::from(prompt.params.max_tokens.unwrap_or(4096)),
            );
            let lost_prefix = previous_bytes
                .as_ref()
                .map_or(0, |bytes| bytes.len().saturating_sub(exact_prefix_bytes));
            let prefix_penalty = (lost_prefix as u64)
                .div_ceil(1024)
                .saturating_mul(options.policy.prefix_loss_penalty_microusd_per_kib);
            let switch_penalty = if previous_model == model.model {
                0
            } else {
                options.policy.model_switch_penalty_microusd
            };
            candidates.push(Candidate {
                model: model.model.clone(),
                context: (*context).into(),
                prompt_bytes,
                suitability,
                eligible: prompt_bytes <= model.max_prompt_bytes
                    && prompt_bytes <= options.hard_limit_bytes
                    && suitability.map_or(model.model == requested_model, |probability| {
                        probability >= options.policy.confidence_threshold
                    }),
                estimated_tokens,
                estimated_cost_microusd,
                exact_prefix_bytes,
                objective_microusd: estimated_cost_microusd
                    .saturating_add(prefix_penalty)
                    .saturating_add(switch_penalty),
            });
            bindings.push((index, prompt));
        }
    }
    let baseline = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.eligible)
        .filter(|(_, candidate)| candidate.model == previous_model)
        .max_by_key(|(_, candidate)| (candidate.exact_prefix_bytes, candidate.context == "full"))
        .map(|(index, _)| index);
    let best = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.eligible)
        .min_by_key(|(index, candidate)| (candidate.objective_microusd, *index))
        .map(|(index, _)| index);
    let (chosen, reason) = match (baseline, best) {
        (Some(baseline), Some(best))
            if candidates[best].objective_microusd as f64
                >= candidates[baseline].objective_microusd as f64
                    * (1.0 - options.policy.minimum_savings_fraction) =>
        {
            (baseline, "retained_within_switch_margin")
        }
        (_, Some(best)) => (best, "lowest_feasible_model_view_cost"),
        _ => {
            // An unavailable or inconclusive decision preserves the caller's
            // fallback. It cannot authorize a model outside the candidate set.
            let fallback = candidates
                .iter()
                .enumerate()
                .find(|(_, candidate)| {
                    candidate.model == requested_model
                        && candidate.prompt_bytes <= options.hard_limit_bytes
                        && set.models.values().any(|model| {
                            model.model == requested_model
                                && candidate.prompt_bytes <= model.max_prompt_bytes
                        })
                })
                .map(|(index, _)| index)
                .ok_or_else(|| {
                    CoreError::rejected(
                        crate::core::protocol::ErrorCode::NoFeasibleRoute,
                        "no model/context candidate fits its declared byte limit",
                    )
                })?;
            (fallback, "conservative_model_fallback")
        }
    };
    let (view_index, prompt) = bindings.swap_remove(chosen);
    let (_, (mut view, _)) = views.swap_remove(view_index);
    view.prompt_sha256 = digest(&prompt)?;
    view.prompt_bytes = candidates[chosen].prompt_bytes;
    view.view_id = super::planner::view_identity(&view)?;
    let selection = Selection {
        requested_model: requested_model.into(),
        selected_model: candidates[chosen].model.clone(),
        selected_context: candidates[chosen].context.clone(),
        reason: reason.into(),
        candidates,
    };
    Ok((view, prompt, Some(selection)))
}

fn prefix(prompt: &Prompt) -> Result<Vec<u8>, CoreError> {
    serde_json::to_vec(&(
        &prompt.system,
        &prompt.system_provider_metadata,
        &prompt.tools,
        &prompt.messages,
    ))
    .map_err(|error| invalid(error.to_string()))
}

fn cost(model: &GenerationModel, input: u64, output: u64) -> u64 {
    let amount = (u128::from(input) * u128::from(model.input_microusd_per_million))
        .saturating_add(u128::from(output) * u128::from(model.output_microusd_per_million))
        .div_ceil(1_000_000);
    u64::try_from(amount).unwrap_or(u64::MAX)
}
