//! Rebuild from actual scoped capture, selected assessment and strict receipts.
//! Stored descriptors locate data; only the original live Binding authorizes H.
use super::*;
use crate::{
    acp_trajectory::checkpoint::owned::assessment,
    core::evolution::{bandit, learning::LearningReport, rubric::RubricEvaluation, scoring},
    gateway::routing::blocks::{allocation::Arm, control::BlockStatus},
    storage::scope::Scope,
    tenancy::{
        learning::{history, store as paid},
        receipts::store as receipts,
    },
};
use bitrouter_ai::tenancy::{
    execution::{Outcome, ReceiptState},
    identity::ExecutionId,
};
use std::collections::{BTreeSet, HashSet};

fn association(ns: &Namespace<'_>, enrollment: &Enrollment) -> Result<Association> {
    let current = ns.binding.association().descriptor();
    let expected = &enrollment.descriptor;
    ensure!(
        expected.installation == current.installation
            && expected.tenant == current.tenant
            && expected.principal == current.principal
            && expected.source == current.source
            && expected.revision == current.revision
            && expected.model == current.model
            && expected.mode == current.mode
            && expected.block_link.is_some(),
        "Block enrollment source namespace changed"
    );
    let (scope, provenance) = Scope::for_host(ns.binding.authority());
    let value = Association::new(
        scope,
        provenance,
        expected.controller.clone(),
        expected.source.clone(),
        expected.revision.clone(),
        expected.model.clone(),
        expected.mode,
    )?
    .with_block_link(
        expected
            .block_link
            .clone()
            .context("Block source link absent")?,
    )?;
    ensure!(
        value.descriptor() == expected,
        "Block enrollment descriptor changed"
    );
    Ok(value)
}

struct Facts {
    observation: bandit::Observation,
    resolved: bool,
    reasons: Vec<String>,
}

async fn latency(
    tx: &DatabaseTransaction,
    a: &Association,
    prefix: &crate::harness::trajectory::owned::prefix::Prefix,
    budget: &OperationBudget<'_>,
) -> Result<Option<u64>> {
    let refs: BTreeMap<_, _> = prefix
        .references
        .iter()
        .map(|reference| (reference.sequence, reference))
        .collect();
    ensure!(
        refs.len() == prefix.references.len() && refs.len() <= MAX_EVENTS,
        "Block latency references changed"
    );
    let rows = io::rows(
        tx,
        a.events()
            .filter(events::Column::Sequence.is_in(refs.keys().copied()))
            .order_by_asc(events::Column::Sequence),
        refs.len(),
        MAX_CAPTURE_BYTES,
        budget,
    )
    .await?;
    ensure!(rows.len() == refs.len(), "Block latency prefix absent");
    let mut projected = Vec::with_capacity(rows.len());
    for row in rows {
        a.validate(
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
        )?;
        let expected = refs
            .get(&row.sequence)
            .context("Block latency reference absent")?;
        ensure!(
            !row.replay
                && crate::harness::trajectory::owned::prefix::reference(&row)? == **expected,
            "Block latency source changed"
        );
        budget.charge(0, row.event_json.len())?;
        projected.push(crate::acp_trajectory::CanonicalEvent {
            node_id: expected.node_id(),
            sequence: row.sequence,
            captured_at: row.captured_at,
            event: serde_json::from_str(&row.event_json)?,
        });
    }
    crate::core::evolution::learning::prompt_active_ms(&projected)
}

async fn observation(
    tx: &DatabaseTransaction,
    ns: &Namespace<'_>,
    enrollment: &Enrollment,
    sequence: u64,
    arm: Arm,
    intents: &[Selection],
    budget: &OperationBudget<'_>,
) -> Result<Facts> {
    let a = association(ns, enrollment)?;
    let session = a.session_in(tx, &enrollment.native, budget).await?;
    ensure!(
        session.owner == enrollment.value.identity.owner
            && session.source == enrollment.value.identity.source
            && session.native_session_id == enrollment.value.identity.native_session_id,
        "Block enrollment session identity changed"
    );
    let head = paid::head(tx, &a, &session.session_key, budget).await?;
    let selected = head.as_ref().and_then(|row| row.revision_id.as_deref());
    let mut facts = Facts {
        observation: bandit::Observation {
            session_key: session.session_key.clone(),
            assignment_sequence: sequence,
            // Owned capture forbids ancestry; each real physical session is a family.
            family_id: session.session_key.clone(),
            revision: String::new(),
            measurement_contract: ns
                .binding
                .block_profile()
                .context("Block profile absent")?
                .definition
                .measurement_contract
                .clone(),
            arm,
            quality: None,
            total_cost_micro_usd: None,
            latency_ms: None,
            severe_violation: false,
        },
        resolved: false,
        reasons: Vec::new(),
    };
    let mut receipt_fences = Vec::new();
    let mut complete = true;
    let mut present = false;
    let block_id = &ns
        .binding
        .block_profile()
        .context("Block profile absent")?
        .definition
        .block_id;
    let mut matched = false;
    // This retained profile can start a separately billed judge, including
    // one whose answer loses a Human-head CAS. Subject-token pricing cannot
    // establish a whole-family cost. Keep it unknown until a complete paid
    // cost projection exists; a missing/unselected job is never a free judge.
    let mut token_cost = if ns.binding.assessment().is_some() {
        facts
            .reasons
            .push("paid_assessment_cost_unavailable".into());
        None
    } else {
        Some(0_u64)
    };
    for intent in intents
        .iter()
        .filter(|intent| intent.session == session.session_key)
    {
        present = true;
        matched |= intent.intent.applied_block.as_ref() == Some(block_id);
        ensure!(
            intent.descriptor == enrollment.descriptor
                && intent.native == enrollment.native
                && intent.intent.identity == enrollment.value.identity
                && intent.intent.captured_watermark <= session.head,
            "Block intent parent changed"
        );
        let execution: ExecutionId = intent.intent.request_id.clone().try_into()?;
        let Some(candidate) =
            receipts::block_candidate_in(tx, ns.binding, &execution, budget).await?
        else {
            complete = false;
            token_cost = None;
            receipt_fences.push((execution, None));
            continue;
        };
        let receipt = &candidate.receipt;
        ensure!(
            receipt.installation_id.as_str() == enrollment.descriptor.installation
                && receipt.tenant_id.as_str() == enrollment.descriptor.tenant
                && receipt.principal_id.as_str() == enrollment.descriptor.principal
                && receipt.execution_id == execution
                && receipt.correlation_id == intent.correlation
                && receipt.execution_revision.as_str() == enrollment.descriptor.revision,
            "Block receipt identity changed"
        );
        for attempt in &receipt.attempts {
            if let Some(target) = &attempt.target {
                ensure!(
                    ns.binding
                        .block_receipt_target(&intent.intent.selected_route, target),
                    "Block receipt escaped its selected retained arm"
                );
            }
        }
        if !intent.token_pricing_eligible
            || candidate.has_auxiliary()
            || receipt.attempts.is_empty()
        {
            token_cost = None;
        } else {
            for attempt in &receipt.attempts {
                token_cost = token_cost.and_then(|sum| {
                    sum.checked_add(
                        ns.binding
                            .block_token_cost(&intent.intent.selected_route, attempt)?,
                    )
                });
            }
        }
        complete &= receipt.state == ReceiptState::Committed
            && receipt.outcome == Outcome::Completed
            && !candidate.auxiliary_unknown()
            && !candidate.auxiliary_unclean()
            && receipt.usage.complete;
        receipt_fences.push((execution, Some(candidate.learning_digest().to_owned())));
    }
    if !matched {
        facts.reasons.push("block_rule_not_observed".into());
    }
    if !present || !complete {
        facts
            .reasons
            .push("strict_receipt_coverage_incomplete".into());
    }
    let prefix = a.stopped_prefix_in(tx, &enrollment.native, budget).await?;
    let mut assessment_fence = None;
    if let (Some(prefix), Some(id)) = (&prefix, selected) {
        let revision = history::revision(tx, &a, &session.session_key, id, budget)
            .await?
            .context("Block effective revision absent")?;
        ensure!(
            revision.selected_on_submission,
            "Block effective revision was never selected"
        );
        let (key, summary, watermark, packet) = assessment::review_in(
            tx,
            &a,
            &enrollment.native,
            Some(&revision.input.checkpoint_id),
            budget,
        )
        .await?;
        ensure!(
            key == session.session_key && watermark == session.head,
            "Block assessment parent changed"
        );
        let current =
            summary.watermark == prefix.watermark && summary.prefix_digest == prefix.digest()?;
        let paid_closure = if revision.input.source
            == crate::acp_trajectory::checkpoint::types::AssessmentSource::Agentic
        {
            let profile = ns
                .binding
                .assessment()
                .context("Block paid binding absent")?;
            Some(
                paid::capture_terminal::validate_revision(
                    tx,
                    &a,
                    &profile.route,
                    &session.session_key,
                    &packet,
                    &revision,
                    budget,
                )
                .await?,
            )
        } else {
            None
        };
        let paid_complete = paid_closure.as_ref().is_none_or(|closure| closure.complete);
        if !paid_complete {
            facts
                .reasons
                .push("paid_receipt_coverage_incomplete".into());
        }
        if current {
            facts.resolved = true;
            if present && complete {
                facts.observation.total_cost_micro_usd = token_cost;
                facts.observation.latency_ms = latency(tx, &a, prefix, budget).await?;
            }
            if let Some(content) = &revision.input.assessment {
                let rubric: RubricEvaluation = serde_json::from_str(&content.explanation)?;
                let contract = scoring::measurement_contract(
                    revision.input.source,
                    &revision.input.evaluator_id,
                    &revision.input.evaluator_version,
                )?;
                let normalized = rubric.assessment(&packet, &contract)?;
                ensure!(
                    digest(&normalized)? == digest(content)?,
                    "Block rubric citation or normalization changed"
                );
                facts.observation.measurement_contract = contract;
                facts.observation.severe_violation = rubric.severe_violation;
                if present && complete && matched && paid_complete {
                    facts.observation.quality = rubric.aggregate(&packet)?.complete_score();
                }
            } else {
                facts.reasons.push("effective_assessment_retracted".into());
            }
        } else {
            facts.reasons.push("effective_assessment_stale".into());
        }
        assessment_fence = Some((
            revision.revision_id,
            revision.input_digest,
            summary.prefix_digest,
            paid_closure.map(|closure| closure.digest),
        ));
    } else {
        facts.reasons.push("stopped_assessment_unavailable".into());
    }
    // This is the retained model-token tariff only for an extension/tool-free
    // canonical input and a receipt with no auxiliary work. Unknown dimensions,
    // ambiguous protocol tariffs or any other effect keep cost unavailable.
    if facts.observation.total_cost_micro_usd.is_none() {
        facts.reasons.push("owned_token_tariff_unavailable".into());
    }
    if facts.observation.latency_ms.is_none() {
        facts.reasons.push("prompt_interval_unavailable".into());
    }
    if facts.observation.measurement_contract
        != ns
            .binding
            .block_profile()
            .context("Block profile absent")?
            .definition
            .measurement_contract
    {
        facts.reasons.push("measurement_contract_changed".into());
    }
    facts.observation.revision = digest(&(
        &enrollment.descriptor,
        session.head,
        selected,
        prefix.as_ref().map(|prefix| prefix.digest()).transpose()?,
        assessment_fence,
        receipt_fences,
    ))?;
    Ok(facts)
}

pub(super) async fn refresh(
    tx: &DatabaseTransaction,
    ns: &Namespace<'_>,
    profile: &Profile,
    control: &mut Control,
    budget: &OperationBudget<'_>,
) -> Result<bool> {
    let rows = io::rows(
        tx,
        ns.query(ENROLLMENT).order_by_asc(records::Column::RecordId),
        MAX_ENROLLMENTS,
        8 * 1024 * 1024,
        budget,
    )
    .await?;
    if rows.is_empty() {
        return Ok(false);
    }
    let raw = io::rows(
        tx,
        ns.query(INTENT).order_by_asc(records::Column::RecordId),
        MAX_INTENTS,
        8 * 1024 * 1024,
        budget,
    )
    .await?;
    let mut intents = Vec::with_capacity(raw.len());
    for row in raw {
        let intent: Selection = ns.decode(&row, budget)?;
        ensure!(
            row.record_id == ns.id(INTENT, &intent.intent.request_id)?
                && row.kind == INTENT
                && row.session_key.as_deref() == Some(intent.session.as_str())
                && row.revision == 0,
            "Block intent record identity changed"
        );
        intents.push(intent);
    }
    let block = control
        .state
        .blocks
        .get(&profile.definition.block_id)
        .context("Block control absent")?;
    let mut observations = bandit::EffectiveObservations::default();
    let mut monitoring_observations = bandit::EffectiveObservations::default();
    let mut resolved = BTreeSet::new();
    let mut unavailable = BTreeMap::new();
    let mut keys = HashSet::new();
    for row in rows {
        let enrollment: Enrollment = ns.decode(&row, budget)?;
        let a = association(ns, &enrollment)?;
        let key = a.session_key(&enrollment.native)?;
        ensure!(
            row.record_id == ns.id(ENROLLMENT, &key)?
                && row.kind == ENROLLMENT
                && row.session_key.as_deref() == Some(key.as_str())
                && row.revision == 0
                && keys.insert(key.clone()),
            "Block enrollment identity changed"
        );
        let assigned = enrollment
            .value
            .assignments
            .get(&profile.definition.block_id)
            .filter(|assignment| assignment.experiment_id == block.experiment_id);
        let monitored = enrollment
            .value
            .monitoring
            .get(&profile.definition.block_id)
            .filter(|assignment| assignment.experiment_id == block.experiment_id);
        ensure!(
            assigned.is_none() || monitored.is_none(),
            "Block has duplicate trial and monitoring membership"
        );
        let (sequence, arm, monitoring) = match (assigned, monitored) {
            (Some(assignment), None) => (assignment.assignment_sequence, assignment.arm, false),
            (None, Some(assignment)) => (assignment.assignment_sequence, Arm::Challenger, true),
            _ => continue,
        };
        let facts = observation(tx, ns, &enrollment, sequence, arm, &intents, budget).await?;
        if facts.resolved {
            resolved.insert(key.clone());
        }
        if !facts.reasons.is_empty() {
            unavailable.insert(key, facts.reasons);
        }
        if monitoring {
            monitoring_observations.replace(facts.observation)?;
        } else {
            observations.replace(facts.observation)?;
        }
    }
    ensure!(
        intents.iter().all(|intent| keys.contains(&intent.session)),
        "Block intent has no enrollment"
    );
    let plan = bandit::plan(
        &block.definition.bandit,
        &observations,
        &block.definition.measurement_contract,
        Some(block.last_exposure_ppm),
        bandit::trial_seed(&block.experiment_id)?,
    )?;
    let monitoring_seed = digest(&(bandit::MONITOR_VERSION, &block.experiment_id))?;
    let monitoring = bandit::monitor(
        &block.definition.bandit,
        &monitoring_observations,
        &block.definition.measurement_contract,
        u64::from_str_radix(
            monitoring_seed.get(..16).context("Block monitoring seed")?,
            16,
        )?,
    )?;
    let cohort_resolved = !block.batch.members.is_empty()
        && block.batch.members.keys().all(|key| resolved.contains(key));
    let mut report = LearningReport {
        block_id: profile.definition.block_id.clone(),
        experiment_id: block.experiment_id.clone(),
        block_status: block.status,
        block_revision: block.revision.clone(),
        publications: Vec::new(),
        archived: false,
        minimum_families_per_arm: Some(block.definition.bandit.minimum_families_per_arm),
        plan: plan.clone(),
        observations,
        monitoring_observations,
        monitoring: monitoring.clone(),
        cohort_resolved,
        unavailable,
        published: false,
    };
    // Pure calculations used the same locked H snapshot; recheck original live
    // authority/deadline before publication. Other writer namespaces never enter.
    budget.charge(0, io::json_len(&report, 1024 * 1024)?)?;
    budget.check()?;
    let changed = if block.status == BlockStatus::RolledBack {
        false
    } else if block.status == BlockStatus::Adopted && monitoring.rollback {
        control
            .state
            .apply_monitoring(&profile.definition.block_id, &monitoring)?
    } else {
        control
            .state
            .apply_plan(&profile.definition.block_id, plan, cohort_resolved)?
    };
    ensure!(control.state.publications.len() <= 128, Error::Capacity);
    report.published = changed;
    let block = control
        .state
        .blocks
        .get(&profile.definition.block_id)
        .context("Block disappeared")?;
    report.block_status = block.status;
    report.block_revision = block.revision.clone();
    // Existing publication records are bounded before copying into a report.
    budget.charge(0, io::json_len(&control.state.publications, 256 * 1024)?)?;
    report.publications = control.state.publications.clone();
    let previous = control.report.as_ref().map(digest).transpose()?;
    let current = digest(&report)?;
    control.report = Some(report);
    budget.check()?;
    Ok(changed || previous.as_deref() != Some(current.as_str()))
}
