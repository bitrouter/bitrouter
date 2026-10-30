//! The existing block assignment algorithm, independent of storage and effects.
use super::*;

/// Values already validated under the caller's original transaction and scope.
pub(crate) struct Input<'a> {
    /// The caller-validated physical capture key, including its actual namespace.
    pub session_key: &'a str,
    pub identity: &'a SessionIdentity,
    pub request_id: &'a str,
    pub context: DecisionContext,
    pub dependencies: &'a RoutingDependencies,
    pub bypass: Option<&'a str>,
    pub watermark: i64,
    pub enrollment: SessionEnrollment,
    pub is_new: bool,
}

/// Randomness and timestamps are explicit inputs. This calculates assignment
/// and intent together; callers must commit both before dispatching any effect.
pub(crate) fn select(
    state: &mut ControlState,
    input: Input<'_>,
    mut next_seed: impl FnMut() -> u64,
    mut now: impl FnMut() -> String,
) -> Result<(SessionEnrollment, RouteIntent)> {
    let Input {
        session_key: key,
        identity,
        request_id,
        context,
        dependencies,
        bypass,
        watermark,
        mut enrollment,
        is_new,
    } = input;
    let current_dependencies = &dependencies.current;
    if is_new && state.mode != EvolutionMode::Off {
        let references = state
            .blocks
            .iter()
            .map(|(id, block)| (id.clone(), block.revision.clone()))
            .collect::<BTreeMap<_, _>>();
        for (id, experiment) in &enrollment.adopted_blocks {
            let block = state
                .blocks
                .get(id)
                .context("adopted block disappeared during admission")?;
            if block.definition.source != identity.source
                || block.experiment_id != *experiment
                || !state.dependencies_match(block)
                || current_dependencies.get(id) != Some(&block.routing_config_digest)
            {
                continue;
            }
            state.next_assignment_sequence = state
                .next_assignment_sequence
                .checked_add(1)
                .context("monitoring sequence overflow")?;
            enrollment.monitoring.insert(
                id.clone(),
                AdoptionMonitoring {
                    monitoring_id: digest(&(experiment, &key, "adoption_monitoring"))?,
                    experiment_id: experiment.clone(),
                    adoption_revision: block.revision.clone(),
                    assignment_sequence: state.next_assignment_sequence,
                    control_generation: state.generation,
                    mode_epoch: state.mode_epoch,
                    reference_blocks: references.clone(),
                    recorded_at: now(),
                },
            );
        }
    }
    if is_new
        && enrollment.admission_reason == "new_session"
        && state.mode != EvolutionMode::Off
        && dependencies.trial_ready
    {
        let references: BTreeMap<_, _> = state
            .blocks
            .iter()
            .map(|(id, block)| (id.clone(), block.revision.clone()))
            .collect();
        let eligible: Vec<_> = state
            .blocks
            .iter()
            .filter(|(_, block)| {
                block.definition.source == identity.source
                    && block.status == BlockStatus::Exploring
                    && !block.batch.closed
                    && state.dependencies_match(block)
                    && current_dependencies.get(&block.definition.block_id)
                        == Some(&block.routing_config_digest)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in eligible {
            let seed = next_seed();
            let Some(allocation) = state.reserve_trial(&id, key, seed)? else {
                continue;
            };
            let block = state
                .blocks
                .get(&id)
                .context("block disappeared during admission")?;
            let assignment = BlockAssignment {
                assignment_id: digest(&(&block.experiment_id, &key))?,
                experiment_id: block.experiment_id.clone(),
                block_revision: block.revision.clone(),
                control_generation: state.generation,
                mode_epoch: state.mode_epoch,
                trial_epoch: state.trial_epoch,
                batch_sequence: allocation.batch_sequence,
                assignment_sequence: allocation.assignment_sequence,
                arm: allocation.arm,
                challenger_propensity_ppm: allocation.challenger_propensity_ppm,
                random_seed: seed,
                reference_blocks: references.clone(),
                recorded_at: now(),
            };
            enrollment.assignments.insert(id, assignment);
        }
    }
    let mut intent = RouteIntent {
        request_id: request_id.into(),
        identity: identity.clone(),
        context: context.clone(),
        selected_route: context.selector.clone(),
        applied_block: None,
        reason: "baseline_passthrough".into(),
        bypass: bypass.map(str::to_owned),
        mode_epoch: state.mode_epoch,
        control_generation: state.generation,
        assignments: enrollment
            .assignments
            .iter()
            .map(|(id, a)| (id.clone(), a.assignment_id.clone()))
            .collect(),
        monitoring: enrollment
            .monitoring
            .iter()
            .map(|(id, m)| (id.clone(), m.monitoring_id.clone()))
            .collect(),
        captured_watermark: watermark,
        recorded_at: now(),
        routing_dependencies: current_dependencies.clone(),
        archived_routing_dependencies: dependencies.archived.clone(),
        trial_ready: dependencies.trial_ready,
    };
    for id in state.blocks.keys() {
        let block = if let Some(experiments) = &enrollment.block_experiments {
            let Some(experiment) = experiments.get(id) else {
                continue;
            };
            state
                .experiment(id, Some(experiment))
                .context("pinned experiment missing")?
        } else if let Some(assignment) = enrollment.assignments.get(id) {
            state
                .experiment(id, Some(&assignment.experiment_id))
                .context("assigned experiment missing")?
        } else if let Some(experiment) = enrollment.adopted_blocks.get(id) {
            state
                .experiment(id, Some(experiment))
                .context("adopted experiment missing")?
        } else {
            state.original_experiment(id)?
        };
        if block.definition.source != identity.source {
            continue;
        }
        let Some(rule) = block.definition.rules.iter().find(|rule| {
            rule.selector == context.selector
                && rule
                    .fingerprint
                    .as_ref()
                    .is_none_or(|fingerprint| fingerprint == &context.fingerprint)
        }) else {
            continue;
        };
        if !dependencies.matches(state, block) || !state.external_dependencies_match(block) {
            intent.reason = "routing_dependency_changed".into();
            continue;
        }
        let (baseline, _) = state.baseline_source(block)?;
        if !dependencies.matches(state, baseline) || !state.external_dependencies_match(baseline) {
            intent.reason = "routing_dependency_changed".into();
            continue;
        }
        let baseline_rule = baseline
            .definition
            .rules
            .iter()
            .find(|r| r.selector == rule.selector && r.fingerprint == rule.fingerprint)
            .context("inherited baseline matcher missing")?;
        let assignment = enrollment
            .assignments
            .get(id)
            .filter(|a| a.experiment_id == block.experiment_id);
        // Adoption affects new sessions. A baseline session already in this
        // experiment keeps its arm through later checkpoints; switching it
        // here would contaminate the control arm with challenger execution.
        let candidate = state.baseline_valid(block)
            && ((block.status == BlockStatus::Adopted
                && (assignment.is_some_and(|a| a.arm == Arm::Challenger)
                    || (assignment.is_none()
                        && enrollment.adopted_blocks.get(id) == Some(&block.experiment_id))))
                || (block.status == BlockStatus::Exploring
                    && state.mode != EvolutionMode::Off
                    && dependencies.trial_ready
                    && assignment.is_some_and(|a| {
                        a.trial_epoch == state.trial_epoch && a.arm == Arm::Challenger
                    })));
        intent.selected_route = if candidate {
            &rule.challenger_route
        } else {
            &baseline_rule.baseline_route
        }
        .clone();
        intent.applied_block = Some(id.clone());
        intent.reason = if block.status == BlockStatus::Adopted && !candidate {
            if assignment.is_some() {
                "retained_experiment_baseline"
            } else {
                "retained_preexisting_baseline"
            }
        } else if block.status == BlockStatus::Adopted {
            "adopted_baseline"
        } else if state.mode == EvolutionMode::Off {
            "evolution_off"
        } else if block.status == BlockStatus::RolledBack {
            "block_rolled_back"
        } else if candidate {
            "sticky_challenger"
        } else {
            "retained_baseline"
        }
        .into();
    }
    if let Some(reason) = bypass {
        intent.selected_route = context.selector;
        intent.applied_block = None;
        intent.reason = reason.into();
    }
    Ok((enrollment, intent))
}
