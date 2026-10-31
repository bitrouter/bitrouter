//! A bounded ordinary-lane request; the actor retains both owners after admission.
use super::Profile;
use crate::{
    harness::resources::managed::store::model::Origin,
    tenancy::{actor, error::Failure, execution::TenantExecution},
};
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::sync::Arc;
use tokio::{
    sync::{OwnedSemaphorePermit, oneshot},
    time::Instant,
};

pub(in crate::tenancy) struct Job {
    pub execution: Arc<TenantExecution>,
    pub origin: Arc<Origin>,
    pub profile: Profile,
    pub deadline: Instant,
    pub reply: oneshot::Sender<Result<(), Failure>>,
    pub permit: OwnedSemaphorePermit,
}

pub(in crate::tenancy) async fn enroll(
    execution: &Arc<TenantExecution>,
    origin: Arc<Origin>,
    profile: Profile,
) -> Result<(), Failure> {
    let service = origin.binding().authority().service().clone();
    execution.revalidate().map_err(Failure::from)?;
    let budget = std::time::Duration::from_secs(5);
    let deadline = Instant::now()
        + execution
            .remaining()
            .map_or(budget, |remaining| remaining.min(budget));
    let read = service
        .reads
        .clone()
        .try_acquire_owned()
        .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
    let permit = tokio::time::timeout_at(deadline, service.commands.reserve())
        .await
        .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
    let (reply, receive) = oneshot::channel();
    {
        let accepting = service
            .accepting
            .lock()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        if !*accepting || service.stop.is_cancelled() || Instant::now() >= deadline {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        execution.revalidate().map_err(Failure::from)?;
        permit.send(actor::Job::Experiment(Box::new(Job {
            execution: execution.clone(),
            origin,
            profile,
            deadline,
            reply,
            permit: read,
        })));
    }
    match tokio::time::timeout_at(deadline, receive).await {
        Ok(Ok(result)) => result,
        // Once sent, the waiter cannot distinguish a late commit from no work.
        _ => Err(ErrorCode::AuthorityUnavailable.into()),
    }
}

pub(in crate::tenancy) struct ReadJob {
    pub binding: Arc<crate::tenancy::execution::session::Binding>,
    pub profile: Profile,
    pub root: String,
    pub deadline: Instant,
    pub reply:
        oneshot::Sender<Result<crate::core::optimization::cohort::CohortAssessment, Failure>>,
    pub permit: OwnedSemaphorePermit,
    pub command: OwnedSemaphorePermit,
}

pub(crate) fn check_reader(
    binding: &crate::tenancy::execution::session::Binding,
) -> Result<(), ErrorCode> {
    use bitrouter_ai::tenancy::authority::{GrantScope, Permission};
    binding.check().map_err(|_| ErrorCode::Forbidden)?;
    let authority = binding.authority();
    let host = authority.host();
    let tenant = authority.storage_identity().1;
    let requested = GrantScope::Tenant {
        operator_pools: Vec::new(),
    };
    let (credential, original_binding) = host.scopes();
    if !crate::tenancy::authority::scope_subset(&requested, credential, tenant)
        || !crate::tenancy::authority::scope_subset(&requested, original_binding, tenant)
    {
        return Err(ErrorCode::Forbidden);
    }
    host.with_authority(authority.dependencies(), |effective| {
        if !effective.permits(Permission::ResourceRead) || !effective.covers(&requested) {
            return Err(ErrorCode::Forbidden);
        }
        Ok(())
    })
}

pub(crate) async fn cohort(
    binding: Arc<crate::tenancy::execution::session::Binding>,
    model: &str,
    root: &str,
    command: OwnedSemaphorePermit,
) -> Result<crate::core::optimization::cohort::CohortAssessment, Failure> {
    use bitrouter_ai::{tenancy::configuration::LearningMode, types::ModelOperation};
    if model.is_empty()
        || model.len() > 512
        || root.len() > 80
        || crate::storage::evaluation::validate_digest(root, "evidence_root").is_err()
    {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    check_reader(&binding).map_err(Failure::from)?;
    let retained = tokio::time::timeout_at(deadline, binding.authority().host().retained())
        .await
        .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))??;
    check_reader(&binding).map_err(Failure::from)?;
    if retained.document().learning.mode != LearningMode::Explore {
        return Err(ErrorCode::CapabilityMismatch.into());
    }
    let route = retained
        .select_native(model, ModelOperation::Generation)
        .map_err(Failure::from)?;
    let policy = route
        .policy()
        .ok_or_else(|| Failure::from(ErrorCode::CapabilityMismatch))?;
    let definition = policy
        .document()
        .policies
        .get(policy.name())
        .ok_or_else(|| Failure::from(ErrorCode::AuthorityUnavailable))?;
    let treatment = definition
        .optimization
        .as_ref()
        .and_then(|value| value.active.as_ref())
        .ok_or_else(|| Failure::from(ErrorCode::CapabilityMismatch))?;
    let profile = Profile {
        revision: binding.descriptor().revision.clone(),
        route: route.reference().clone(),
        policy: policy.reference().clone(),
        policy_name: policy.name().to_owned(),
        treatment: treatment.clone(),
        policy_digest: crate::policy_lock::semantic_digest(policy.document())
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?,
        definition_digest: crate::storage::evaluation::canonical_digest(&(
            policy.name(),
            definition,
        ))
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?,
    };
    drop(retained);
    let service = binding.authority().service().clone();
    let read = service
        .reads
        .clone()
        .try_acquire_owned()
        .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
    let permit = tokio::time::timeout_at(deadline, service.commands.reserve())
        .await
        .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
    let (reply, receive) = oneshot::channel();
    {
        let accepting = service
            .accepting
            .lock()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        if !*accepting || service.stop.is_cancelled() || Instant::now() >= deadline {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        check_reader(&binding).map_err(Failure::from)?;
        permit.send(actor::Job::ExperimentRead(Box::new(ReadJob {
            binding: binding.clone(),
            profile,
            root: root.to_owned(),
            deadline,
            reply,
            permit: read,
            command,
        })));
    }
    let result = tokio::time::timeout_at(deadline, receive)
        .await
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))??;
    check_reader(&binding).map_err(Failure::from)?;
    Ok(result)
}

/// The actual native facade reply is forwarded under the existing authority
/// read guard. An actor reply queued before revocation is not a delivery grant.
pub(crate) fn deliver(
    binding: &crate::tenancy::execution::session::Binding,
    reply: oneshot::Sender<
        Result<
            crate::core::optimization::cohort::CohortAssessment,
            bitrouter_ai::core::protocol::CoreError,
        >,
    >,
    outcome: Result<
        crate::core::optimization::cohort::CohortAssessment,
        bitrouter_ai::core::protocol::CoreError,
    >,
) {
    use bitrouter_ai::tenancy::authority::{GrantScope, Permission};
    if outcome.is_err() {
        if reply.send(outcome).is_err() {
            tracing::debug!("native cohort waiter disconnected");
        }
        return;
    }
    let mut pending = Some((reply, outcome));
    let checked = check_reader(binding).and_then(|()| {
        binding
            .authority()
            .host()
            .with_authority(binding.authority().dependencies(), |effective| {
                if !effective.permits(Permission::ResourceRead)
                    || !effective.covers(&GrantScope::Tenant {
                        operator_pools: Vec::new(),
                    })
                {
                    return Err(ErrorCode::Forbidden);
                }
                let (reply, outcome) = pending.take().ok_or(ErrorCode::AuthorityUnavailable)?;
                if reply.send(outcome).is_err() {
                    tracing::debug!("native cohort waiter disconnected");
                }
                Ok(())
            })
    });
    if let Err(_code) = checked
        && let Some((reply, _)) = pending
    {
        let error = bitrouter_ai::core::protocol::CoreError::rejected(
            bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope,
            "native cohort authority changed before delivery",
        );
        if reply.send(Err(error)).is_err() {
            tracing::debug!("denied native cohort waiter disconnected");
        }
    }
}
