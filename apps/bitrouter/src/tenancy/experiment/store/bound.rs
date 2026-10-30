use super::*;
use crate::{tenancy::experiment::BoundEpisode, trajectory::store::CorrelateAndBegin};

pub(in crate::tenancy) async fn begin(
    tx: &DatabaseTransaction,
    execution: &TenantExecution,
    witness: &Witness,
    input: &CorrelateAndBegin,
    budget: &OperationBudget<'_>,
) -> Result<BoundEpisode> {
    ensure!(
        witness.matches(execution),
        "native trajectory witness belongs to another execution"
    );
    let (scope, provenance) = Scope::for_execution(execution);
    let id = scope.physical_key(
        KeyDomain::NativeExperimentStep,
        "",
        &[execution.id().as_str()],
    )?;
    let row = records::point(tx, &scope, &id, &[STEP_PENDING], budget)
        .await?
        .context("native trajectory has no pending enrollment")?;
    let previous: Step = records::decode(&row, &row.scope_id, row.session_key.as_deref())?;
    let (_, unit) = records::linked_unit(tx, &scope, &row, &previous, budget).await?;
    ensure!(
        previous.execution == *execution.id()
            && previous.facts == *witness.origin.facts()
            && unit.profile == witness.unit.profile
            && unit.assignment == *witness.assignment()
            && input.new_episode_id == unit.episode
            && input.request_id.len() <= 256,
        "native trajectory enrollment closure mismatch"
    );
    let guard = input
        .guarded_route
        .as_ref()
        .context("bound experiment has no route intent")?;
    let selected_tier = match &guard.experiment {
        Some(assignment) => {
            ensure!(
                assignment == &unit.assignment
                    && guard.policy_name == unit.profile.policy_name
                    && guard.policy_digest == unit.profile.policy_digest
                    && guard.route_projection == unit.profile.treatment.target_request_key,
                "native route treatment changed"
            );
            let tier = match assignment.arm {
                ExperimentArm::Control => &unit.profile.treatment.champion_tier,
                ExperimentArm::Challenger => &unit.profile.treatment.challenger_tier,
            };
            ensure!(
                guard.candidate_tier.as_deref() == Some(tier),
                "native assigned tier changed"
            );
            Some(tier.clone())
        }
        None => None,
    };
    let mut next = previous.clone();
    if let Some(request) = &previous.request {
        ensure!(
            request == &input.request_id,
            "native step cannot start another trajectory request"
        );
    } else {
        next.request = Some(input.request_id.clone());
        records::advance(tx, &scope, &provenance, &row, &previous, &next, budget).await?;
    }
    Ok(BoundEpisode {
        episode: unit.episode,
        selected_tier,
    })
}
