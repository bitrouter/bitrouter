//! Admitted guarded-policy work uses the authority writer and its existing custody.
mod adapter;
pub(in crate::tenancy) mod recorder;
mod reservation;

use super::{error::Failure, eval::observation::RequiredObservation, execution::TenantExecution};
use crate::{
    storage::scope::{Provenance, Scope},
    trajectory::{
        correlation::{CorrelatedRequest, CorrelationEvidence},
        store::{CorrelateAndBegin, CorrelateAndBeginResult, GuardedRouteResult},
        types::Settlement,
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{identity::ExecutionId, operation::ErrorCode};
use std::sync::Arc;
use tokio::{sync::oneshot, time::Instant};

pub(crate) struct Adapter {
    execution: Arc<TenantExecution>,
    observation: Arc<RequiredObservation>,
    canonicalizer: crate::trajectory::canonical::Canonicalizer,
    observe: bool,
}
pub(crate) struct Recorder {
    execution: Arc<TenantExecution>,
    observation: Arc<RequiredObservation>,
    pending: crate::gateway::observability::decision::PendingEvalDecisionStore,
}

pub(in crate::tenancy) struct Begin {
    pub execution: Arc<TenantExecution>,
    pub observation: Arc<RequiredObservation>,
    pub input: Arc<CorrelateAndBegin>,
    pub evidence: CorrelationEvidence,
    pub observe: bool,
    pub reservation: crate::trajectory::store::owned::headroom::Reservation,
    pub deadline: Instant,
    pub reply: oneshot::Sender<std::result::Result<BeginReply, Failure>>,
}
pub(in crate::tenancy) type BeginReply = (CorrelatedRequest, GuardedRouteResult);
pub(in crate::tenancy) struct Terminal {
    pub execution: Arc<TenantExecution>,
    pub observation: Arc<RequiredObservation>,
    pub start: Arc<StartProof>,
    pub facts: Arc<recorder::Facts>,
    pub deadline: Instant,
    pub reply: oneshot::Sender<std::result::Result<(), Failure>>,
}

/// No Service/Execution Arc: the observation cell must not own its own lease.
/// The actor creates this only after an acknowledged Begin commit.
pub(crate) struct StartProof {
    execution: ExecutionId,
    scope: Scope,
    provenance: Provenance,
    owner: String,
    request: String,
    episode: String,
    observe: bool,
}
impl Begin {
    pub(in crate::tenancy) fn committed(
        &self,
        result: CorrelateAndBeginResult,
    ) -> Result<BeginReply> {
        let (scope, provenance) = Scope::for_execution(&self.execution);
        let proof = Arc::new(StartProof {
            execution: self.execution.id().clone(),
            owner: scope.physical_key(crate::storage::scope::KeyDomain::Owner, "", &[])?,
            scope,
            provenance,
            request: self.input.request_id.clone(),
            episode: result.episode_id.clone(),
            observe: self.observe,
        });
        let guarded = result
            .guarded_route
            .context("committed trajectory start lost route intent")?;
        self.observation.committed_start(proof)?;
        Ok((
            CorrelatedRequest {
                request_id: self.input.request_id.clone(),
                episode_id: result.episode_id,
                source: result.source,
                completeness: result.completeness,
                prior_events: result.prior_events,
                evidence: self.evidence.clone(),
            },
            guarded,
        ))
    }
}
impl StartProof {
    pub(in crate::tenancy) fn matches(&self, execution: &TenantExecution) -> bool {
        let (scope, provenance) = Scope::for_execution(execution);
        self.execution == *execution.id() && self.scope == scope && self.provenance == provenance
    }
    pub(in crate::tenancy) fn request(&self) -> &str {
        &self.request
    }
    pub(in crate::tenancy) fn build(
        &self,
        request: &crate::trajectory::types::StoredRequest,
        events: &[crate::trajectory::types::TrajectoryEvent],
        sequence: u64,
        facts: &recorder::Facts,
    ) -> Result<Settlement> {
        ensure!(
            request.request_id == self.request && request.episode_id == self.episode,
            "trajectory start proof mismatch"
        );
        if self.observe {
            crate::trajectory::settlement::build_from_evidence(
                &self.owner,
                request,
                events,
                sequence,
                facts.evidence.clone(),
                facts.timestamp(request, events)?,
                facts.status.clone(),
            )
        } else {
            crate::trajectory::settlement::build_terminal_from_evidence(
                &self.owner,
                request,
                sequence,
                facts.evidence.clone(),
                facts.timestamp(request, events)?,
                facts.status.clone(),
            )
        }
    }
    pub(crate) fn publication<'a>(
        &self,
        scope: &Scope,
        provenance: &Provenance,
        settlement: &'a Settlement,
    ) -> Result<&'a crate::trajectory::evaluation::TrajectoryEvaluationEnvelope> {
        ensure!(
            self.observe && &self.scope == scope && &self.provenance == provenance,
            "trajectory publication authority mismatch"
        );
        ensure!(
            settlement.event.owner_user_id == self.owner
                && settlement.event.request_id.as_deref() == Some(&self.request)
                && settlement.event.episode_id == self.episode,
            "trajectory publication identity mismatch"
        );
        let envelope = settlement
            .outbox
            .as_ref()
            .and_then(|o| o.payload.evaluation.as_deref())
            .context("trajectory terminal lacks evaluation")?;
        crate::trajectory::evaluation::validate_evaluation_envelope(envelope)?;
        crate::trajectory::evaluation::validate_builtin_trajectory_result(&envelope.result)?;
        ensure!(
            envelope.subject.subject_id == self.episode,
            "trajectory evaluation episode mismatch"
        );
        Ok(envelope)
    }
}

fn error() -> crate::gateway::error::BitrouterError {
    crate::gateway::error::BitrouterError::internal(
        "required tenant trajectory observation is incomplete",
    )
}
fn rejection(
    execution: &TenantExecution,
    code: ErrorCode,
) -> crate::gateway::error::BitrouterError {
    execution.reject(code)
}
