//! Pure numerical work consumes H-acquired data; it cannot read or publish.
use super::{Registration, identity::ReadIdentity};
use crate::{
    core::{
        eval::compiler::EvalEvidenceSnapshot,
        optimization::controller::{
            self, OptimizationOptions, OptimizationStep, OptimizationStepInput,
        },
    },
    policy_lock::PolicyLock,
    storage::operation_budget::Error,
    tenancy::{
        experiment::{Profile, store::cohort::input::SourceFence},
        resources::execution::RetainedExecutionRevision,
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::OwnerKind,
    configuration::{Configuration, LearningMode, ResourceRef},
    identity::{AuthorityRevision, OperationId},
    operation::ErrorCode,
};
use std::sync::Arc;
use tokio::time::Instant;

pub(in crate::tenancy) fn profile(
    runtime: &RetainedExecutionRevision,
    reference: &ResourceRef,
) -> Result<Profile> {
    ensure!(
        runtime.document().learning.mode == LearningMode::Explore,
        Error::Rejected(ErrorCode::CapabilityMismatch)
    );
    let route = runtime.learning_route(reference).map_err(Error::Rejected)?;
    ensure!(
        route.choice_profile().is_none()
            && route.assessment_reference().is_none()
            && route.count_candidate().map_err(Error::Rejected)?.is_none(),
        Error::Rejected(ErrorCode::CapabilityMismatch)
    );
    let policy = route
        .policy()
        .ok_or(Error::Rejected(ErrorCode::CapabilityMismatch))?;
    // A tenant learning registration may retain new tenant revisions, never
    // mint operator-pool successors from a read grant.
    ensure!(
        policy.reference().owner_kind == OwnerKind::Tenant
            && policy.reference().owner_id.as_str() == runtime.tenant_id().as_str(),
        Error::Rejected(ErrorCode::CapabilityMismatch)
    );
    let definition = policy
        .document()
        .policies
        .get(policy.name())
        .context("retained learner policy absent")?;
    let treatment = definition
        .optimization
        .as_ref()
        .and_then(|value| value.active.as_ref())
        .ok_or(Error::Rejected(ErrorCode::CapabilityMismatch))?;
    let config = policy.configuration().map_err(Error::Rejected)?;
    crate::tenancy::trajectory_profile::guarded(&config, policy.document(), policy.name())
        .map_err(|_| Error::Rejected(ErrorCode::CapabilityMismatch))?;
    Ok(Profile {
        revision: runtime.revision_id().clone(),
        route: reference.clone(),
        policy: policy.reference().clone(),
        policy_name: policy.name().to_owned(),
        policy_digest: crate::policy_lock::semantic_digest(policy.document())?,
        definition_digest: crate::storage::evaluation::canonical_digest(&(
            policy.name(),
            definition,
        ))?,
        treatment: treatment.clone(),
    })
}

pub(in crate::tenancy) struct Acquired {
    pub registration: Arc<Registration>,
    pub identity: Arc<ReadIdentity>,
    pub snapshot: EvalEvidenceSnapshot,
    pub fence: SourceFence,
    pub policy: PolicyLock,
    pub document: Configuration,
    pub dependencies: Vec<ResourceRef>,
    pub authority_revision: AuthorityRevision,
    pub sequence: u64,
    pub charged: (usize, usize),
    pub operation: OperationId,
    pub deadline: Instant,
    pub _lease: Arc<crate::tenancy::admission::LearningLease>,
}
pub(in crate::tenancy) struct Calculated {
    pub step: OptimizationStep,
}

pub(in crate::tenancy) fn calculate(acquired: &Acquired) -> Result<Calculated> {
    acquired
        .identity
        .check(&acquired.dependencies)
        .map_err(Error::Rejected)?;
    ensure!(
        Instant::now() < acquired.deadline,
        Error::Rejected(ErrorCode::CapacityExceeded)
    );
    let profile = &acquired.registration.profile;
    let treatment = &profile.treatment;
    let options = OptimizationOptions {
        objective: treatment.gate.objective,
        policy: profile.policy_name.clone(),
        candidate_tier: Some(treatment.challenger_tier.clone()),
        challenger_exposure_ppm: treatment.challenger_exposure_ppm,
        minimum_tasks_per_arm: treatment.gate.minimum_tasks_per_arm,
        maximum_challenger_tasks: treatment.gate.maximum_challenger_tasks,
        minimum_pass_rate_ppm: treatment.gate.minimum_pass_rate_ppm,
        evaluator_config_digest: treatment.gate.evaluator_config_digest.clone(),
    };
    let step = controller::prepare_step(OptimizationStepInput {
        eval: &acquired.snapshot,
        active_policy: &acquired.policy,
        active_policy_digest: &profile.policy_digest,
        policy_name: &profile.policy_name,
        options: &options,
    })?;
    acquired
        .identity
        .check(&acquired.dependencies)
        .map_err(Error::Rejected)?;
    Ok(Calculated { step })
}
