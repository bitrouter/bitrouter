//! Finite model/view planning. Suitability is semantic; capacity, cost and
//! switching decisions are deterministic. Prices and byte/token conversion
//! are local planning assumptions, never observed billing or cache evidence.

use crate::language_model::{Prompt, native::NativePlan};
use serde::{Deserialize, Serialize};

use std::collections::BTreeSet;

use crate::error::{BitrouterError, Result};

use super::ContextCapability;

#[derive(Clone, Debug, Serialize, Deserialize)]
/// One assessed model/view combination and its explicit planning estimates.
pub struct Candidate {
    /// Configured model selector.
    pub model: String,
    /// Host-provided view identifier.
    pub context: String,
    /// Serialized canonical prompt size.
    pub prompt_bytes: usize,
    /// Whether the frozen policy admitted this model action.
    pub policy_admitted: bool,
    /// Whether declared capacity, authority and semantic thresholds admit it.
    pub eligible: bool,
    /// Planning estimate at four bytes per token, not measured provider usage.
    pub estimated_tokens: u64,
    /// Estimated uncached input and reserved output cost.
    pub estimated_cost_microusd: Option<u64>,
    /// Exact shared bytes with the previous same-model prompt prefix.
    pub exact_prefix_bytes: usize,
    /// Estimated cost plus configured switching and prefix penalties.
    pub objective_microusd: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Replayable choice and all considered alternatives.
pub struct Selection {
    /// Caller-supplied conservative model selector.
    pub requested_model: String,
    /// Model selector bound by this plan.
    pub selected_model: String,
    /// View identifier bound by this plan.
    pub selected_context: String,
    /// Commitment to the exact canonical prompt selected for generation.
    pub prompt_digest: String,
    /// Policy-owned context treatment, distinct from granted capabilities.
    pub strategy: super::ContextStrategy,
    /// Stable explanation of the selection rule.
    pub reason: String,
    /// Candidate estimates including rejected alternatives.
    pub candidates: Vec<Candidate>,
}

/// One policy-admitted model and its semantic assessment. Provider answers
/// are decoded by the classifier adapter before entering the planner.
#[derive(Clone, Debug)]
pub struct AdmittedModel {
    /// Model facts from the frozen policy/catalog snapshot.
    pub model: Model,
    /// Admission granted by the frozen policy, independent of classifier confidence.
    pub admitted: bool,
}

/// Deterministic planning constraints, independent of classifier wire format.
#[derive(Clone, Copy, Debug)]
pub struct CostPolicy {
    /// Improvement required to leave a feasible previous binding.
    pub minimum_savings_fraction: f64,
    /// Planning penalty for changing the model selector.
    pub model_switch_penalty_microusd: u64,
    /// Planning penalty for lost exact prefix bytes, charged per KiB.
    pub prefix_loss_penalty_microusd_per_kib: u64,
}

/// One materialized view offered by an authorized context owner.
#[derive(Clone, Debug)]
pub struct View {
    /// Stable view identity assigned by the context owner.
    pub id: String,
    /// Complete canonical generation input.
    pub prompt: Prompt,
    /// Operations needed to produce this view from its immutable source.
    pub requires: BTreeSet<ContextCapability>,
}

/// The selected prompt and its source view index, plus auditable alternatives.
#[derive(Clone, Debug)]
pub struct Plan {
    /// Index of the selected entry in the caller's offered views.
    pub view_index: usize,
    /// Complete canonical generation input.
    pub prompt: Prompt,
    /// Frozen selection evidence.
    pub selection: Selection,
}

/// Frozen policy, previous execution and context authority for one choice.
pub struct Options<'a> {
    /// Context treatment admitted by the policy action.
    pub strategy: super::ContextStrategy,
    /// Local planning constraints and declared model prices.
    pub policy: CostPolicy,
    /// Previous admitted generation plan used to measure switching.
    pub previous: Option<&'a NativePlan>,
    /// Host admission bound for any selected prompt.
    pub hard_limit_bytes: usize,
    /// Authority admitted by the context owner, never inferred from text.
    pub capabilities: &'a BTreeSet<ContextCapability>,
}

/// Choose among the same bounded model/view candidates for every input source.
/// The caller owns admission and persistence; no entry-point identity is read.
pub fn select(
    models: &[AdmittedModel],
    requested_model: &str,
    views: &[View],
    options: Options<'_>,
) -> Result<Plan> {
    if models.is_empty() || models.len() > 16 || views.is_empty() || views.len() > 32 {
        return Err(BitrouterError::bad_request(
            "invalid routing candidate bounds",
        ));
    }
    let policy = options.policy;
    if !policy.minimum_savings_fraction.is_finite()
        || !(0.0..1.0).contains(&policy.minimum_savings_fraction)
        || options.hard_limit_bytes == 0
        || models.iter().any(|candidate| {
            candidate.model.model.trim().is_empty() || candidate.model.max_prompt_bytes == 0
        })
        || models
            .iter()
            .map(|candidate| &candidate.model.model)
            .collect::<BTreeSet<_>>()
            .len()
            != models.len()
        || views.iter().any(|view| view.id.trim().is_empty())
        || views
            .iter()
            .map(|view| &view.id)
            .collect::<BTreeSet<_>>()
            .len()
            != views.len()
    {
        return Err(BitrouterError::bad_request(
            "invalid routing policy or candidates",
        ));
    }
    let previous_model = options
        .previous
        .map_or(requested_model, |plan| plan.effective_model.as_str());
    let previous_bytes = options
        .previous
        .map(|plan| prefix(&plan.prompt))
        .transpose()?;
    let mut candidates = Vec::new();
    let mut bindings = Vec::new();
    for assessed in models {
        let model = &assessed.model;
        let admitted = assessed.admitted;
        for (index, view) in views.iter().enumerate() {
            let mut prompt = view.prompt.clone();
            prompt.model.clone_from(&model.model);
            let prompt_bytes = serde_json::to_vec(&prompt)
                .map_err(|error| BitrouterError::internal(error.to_string()))?
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
                context: view.id.clone(),
                prompt_bytes,
                policy_admitted: admitted,
                eligible: (options.strategy == super::ContextStrategy::Evidence
                    || view.requires.is_empty())
                    && view.requires.is_subset(options.capabilities)
                    && prompt_bytes <= model.max_prompt_bytes
                    && prompt_bytes <= options.hard_limit_bytes
                    && admitted,
                estimated_tokens,
                estimated_cost_microusd,
                exact_prefix_bytes,
                objective_microusd: estimated_cost_microusd.map(|cost| {
                    cost.saturating_add(prefix_penalty)
                        .saturating_add(switch_penalty)
                }),
            });
            bindings.push((index, prompt));
        }
    }
    let baseline = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.eligible)
        .filter(|(_, candidate)| candidate.model == previous_model)
        .max_by_key(|(index, candidate)| {
            (
                candidate.exact_prefix_bytes,
                views[bindings[*index].0].requires.is_empty(),
            )
        })
        .map(|(index, _)| index);
    let best = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.eligible)
        .min_by_key(|(index, candidate)| {
            (
                candidate.objective_microusd.is_none(),
                candidate.objective_microusd,
                candidate.prompt_bytes,
                *index,
            )
        })
        .map(|(index, _)| index);
    let (chosen, reason) = match (baseline, best) {
        (Some(baseline), Some(best))
            if within_margin(
                &candidates[best],
                &candidates[baseline],
                options.policy.minimum_savings_fraction,
            ) =>
        {
            (baseline, "retained_within_switch_margin")
        }
        (_, Some(best)) => (best, "lowest_feasible_model_view_cost"),
        _ => {
            return Err(BitrouterError::bad_request(
                "no policy-admitted model/context candidate satisfies routing constraints",
            ));
        }
    };
    let (view_index, prompt) = bindings.swap_remove(chosen);
    let selection = Selection {
        requested_model: requested_model.into(),
        strategy: options.strategy,
        selected_model: candidates[chosen].model.clone(),
        selected_context: candidates[chosen].context.clone(),
        prompt_digest: super::assessment::digest(&prompt)
            .map_err(|error| BitrouterError::internal(error.message))?,
        reason: reason.into(),
        candidates,
    };
    Ok(Plan {
        view_index,
        prompt,
        selection,
    })
}

fn prefix(prompt: &Prompt) -> Result<Vec<u8>> {
    serde_json::to_vec(&(
        &prompt.system,
        &prompt.system_provider_metadata,
        &prompt.tools,
        &prompt.messages,
    ))
    .map_err(|error| BitrouterError::internal(error.to_string()))
}

fn cost(model: &Model, input: u64, output: u64) -> Option<u64> {
    let amount = (u128::from(input) * u128::from(model.input_microusd_per_million?))
        .saturating_add(u128::from(output) * u128::from(model.output_microusd_per_million?))
        .div_ceil(1_000_000);
    Some(u64::try_from(amount).unwrap_or(u64::MAX))
}

fn within_margin(best: &Candidate, baseline: &Candidate, margin: f64) -> bool {
    // Prices and byte counts are different units. Never compare one to the other.
    match (best.objective_microusd, baseline.objective_microusd) {
        (Some(best), Some(baseline)) => best as f64 >= baseline as f64 * (1.0 - margin),
        (None, None) => best.prompt_bytes as f64 >= baseline.prompt_bytes as f64 * (1.0 - margin),
        _ => false,
    }
}

/// Operator-declared planning facts. Provider capacity and permission are still
/// independently checked by the SDK after selecting the concrete prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// A normal SDK route selector, never provider credentials.
    pub model: String,
    /// Conservative local byte admission limit, not a measured token window.
    pub max_prompt_bytes: usize,
    /// Estimated micro-USD per million uncached input tokens.
    pub input_microusd_per_million: Option<u64>,
    /// Estimated micro-USD per million output tokens.
    pub output_microusd_per_million: Option<u64>,
}

impl crate::event::PipelineEvent for Selection {
    fn event_name(&self) -> &'static str {
        "routing.selected"
    }
}
