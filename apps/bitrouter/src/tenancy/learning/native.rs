//! Explicit native paid caller. Source facts never replace the live parent binding.
use super::store::native as sql;
use super::{control, owner, queue, source, store};
use crate::{
    core::evolution::judge,
    storage::operation_budget::OperationBudget,
    tenancy::{
        cache::Pin, execution::session::Binding, experiment::store::completed::Edge,
        resources::execution::RetainedExecutionRevision,
    },
};
use anyhow::{Result, ensure};
use bitrouter_ai::{
    core::protocol::{CommitStatus, CoreError, ErrorCode as CoreCode, OwnershipGrant},
    tenancy::operation::ErrorCode,
};
use sea_orm::DatabaseTransaction;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::time::Instant;

pub(in crate::tenancy) struct Prepare {
    pub binding: Arc<Binding>,
    pub retained: Pin<RetainedExecutionRevision>,
    pub checkpoint: String,
    pub expected: Option<String>,
    pub deadline: Instant,
}
pub(in crate::tenancy) struct PreparedSource {
    pub edge: Edge,
    pub profile: super::Profile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Seed {
    pub descriptor: crate::tenancy::execution::session::Descriptor,
    pub grant: OwnershipGrant,
    pub source: Edge,
}
impl Seed {
    pub(super) fn checked(binding: &Binding, source: &PreparedSource) -> Result<Self> {
        ensure!(
            source.edge.head.execution_epoch == binding.ownership_grant().execution_epoch,
            "native paid original epoch changed"
        );
        Ok(Self {
            descriptor: binding.descriptor().clone(),
            grant: binding.ownership_grant().clone(),
            source: source.edge.clone(),
        })
    }
    pub(super) fn validate(&self, seed: &super::recovery::Seed) -> Result<()> {
        self.grant.validate()?;
        self.source.head.validate()?;
        ensure!(
            self.descriptor.installation == seed.installation
                && self.descriptor.tenant == seed.tenant
                && self.descriptor.principal == seed.principal
                && self.descriptor.revision == seed.configuration
                && self.grant.session_id == self.descriptor.session
                && self.grant.harness_id == self.descriptor.harness
                && self.source.version == 1
                && self.source.unit == seed.session
                && self.source.checkpoint == seed.checkpoint
                && self.source.prefix == seed.prefix_digest
                && self.source.head.execution_epoch == self.grant.execution_epoch,
            "native paid source identity changed"
        );
        Ok(())
    }
}

/// Only the real close callback can persist this data before Registry stop-set ACK.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cleanup {
    pub origin: crate::tenancy::origin::Stamp,
    pub descriptor: crate::tenancy::execution::session::Descriptor,
    pub grant: OwnershipGrant,
    pub head: bitrouter_ai::core::checkpoint::DurableHead,
    pub journal_digest: String,
}

pub(crate) async fn run(
    binding: Arc<Binding>,
    checkpoint: String,
    expected: Option<String>,
    prepare_deadline: Instant,
    permit: tokio::sync::OwnedSemaphorePermit,
    completion_stop: tokio_util::sync::CancellationToken,
) -> std::result::Result<(), CoreError> {
    let authority = source::Binding::Native(binding.clone());
    authority.assessment_check().map_err(rejected)?;
    let retained = tokio::time::timeout_at(prepare_deadline, authority.authority().retained())
        .await
        .map_err(|_| rejected(ErrorCode::CapacityExceeded))?
        .map_err(|e| rejected(e.0.code))?;
    let prepared = queue::prepare_native(Prepare {
        binding,
        retained,
        checkpoint,
        expected,
        deadline: prepare_deadline,
    })
    .await
    .map_err(|e| rejected(e.0.code))?;
    let store::PreparedSource::Native(source) = &prepared.source else {
        return Err(rejected(ErrorCode::AuthorityUnavailable));
    };
    let source_pin = match &authority {
        source::Binding::Native(binding) => binding
            .pin_paid_source(source.edge.clone())
            .map_err(rejected)?,
        source::Binding::Capture(_) => return Err(rejected(ErrorCode::AuthorityUnavailable)),
    };
    let lease = authority
        .service()
        .admission
        .learning(
            authority.authority().identity().1,
            source.profile.concurrent,
        )
        .map_err(rejected)?;
    // Fail-fast child admission never waits holding the native host/learning slot.
    let execution = authority
        .authority()
        .admit_assessment(authority.clone(), &prepared.route)
        .await
        .map_err(|e| rejected(e.0.code))?;
    let receipt = execution
        .receipt_owner()
        .map_err(|_| rejected(ErrorCode::AuthorityUnavailable))?
        .clone();
    let owner = owner::Owner::new(
        authority.clone(),
        prepared.clone(),
        &receipt,
        lease,
        receipt.deadline,
    )
    .map_err(|_| rejected(ErrorCode::AuthorityUnavailable))?;
    owner.retain_native_source(source_pin).map_err(rejected)?;
    // Native accepted jobs require actual claimed startup provenance from first issuance.
    if owner.recovery.is_none() {
        return Err(rejected(ErrorCode::CapabilityMismatch));
    }
    receipt
        .attach_learning(owner.clone())
        .map_err(|_| rejected(ErrorCode::AuthorityUnavailable))?;
    let mut runtime = owner.runtime();
    let accepted = queue::accept(owner.clone())
        .await
        .map_err(|e| failed(e.0.code, owner.acceptance_status()))?;
    drop(permit);
    if !accepted {
        return Ok(());
    }
    let prompt = judge::bounded_prompt(
        prepared.route.resource_id.as_str(),
        &prepared.input,
        16 * 1024 * 1024,
    )
    .map_err(|_| failed(ErrorCode::CapacityExceeded, CommitStatus::Committed))?;
    let response = authority
        .service()
        .execute_admitted_native(
            prompt,
            http::HeaderMap::new(),
            Arc::new(control::Control::new(
                authority.clone(),
                execution.id().clone(),
            )),
            execution,
            None,
        )
        .await;
    let parsed = response.and_then(|response| {
        judge::parse(
            &response,
            prepared.route.resource_id.as_str(),
            &prepared.input,
        )
        .and_then(|output| {
            crate::harness::trajectory::owned::io::json(&output, 512 * 1024)?;
            Ok(output)
        })
        .map_err(|_| {
            crate::gateway::error::BitrouterError::internal("native assessment output rejected")
        })
    });
    if let Ok(output) = &parsed {
        runtime.output(output.clone());
        queue::observe(owner.clone(), output.clone())
            .await
            .map_err(|code| failed(code, CommitStatus::Unknown))?;
    }
    drop(runtime);
    // Only the actual host may stop this result wait, after provider/observation
    // custody has returned. The Registry retains Owner + source pin until its
    // terminal or exact stopped-owner protocol completes; no clean claim here.
    tokio::select! {
        result = owner.wait_terminal() => result.map_err(|code| failed(code, CommitStatus::Unknown))?,
        () = completion_stop.cancelled() => return Err(failed(ErrorCode::AuthorityUnavailable, CommitStatus::Unknown)),
    }
    parsed
        .map(|_| ())
        .map_err(|_| failed(ErrorCode::InvalidRequest, CommitStatus::Committed))
}
fn rejected(code: ErrorCode) -> CoreError {
    failed(code, CommitStatus::NotCommitted)
}
fn failed(code: ErrorCode, status: CommitStatus) -> CoreError {
    let mut error = CoreError::rejected(
        match code {
            ErrorCode::CapacityExceeded => CoreCode::LimitExceeded,
            ErrorCode::AuthorityUnavailable | ErrorCode::StorageUnavailable => {
                CoreCode::CheckpointUnavailable
            }
            _ => CoreCode::UnauthorizedScope,
        },
        "native paid assessment unavailable",
    );
    error.commit_status = status;
    error
}

pub(in crate::tenancy) async fn prepare(
    tx: &DatabaseTransaction,
    request: &Prepare,
    budget: &OperationBudget<'_>,
) -> Result<store::Prepared> {
    sql::prepare(tx, request, budget).await
}
pub(super) async fn accept(
    tx: &DatabaseTransaction,
    binding: &Arc<Binding>,
    p: &store::Prepared,
    execution: &bitrouter_ai::tenancy::identity::ExecutionId,
    seed: Option<&super::recovery::Seed>,
    budget: &OperationBudget<'_>,
) -> Result<Option<store::Status>> {
    sql::accept(tx, binding, p, execution, seed, budget).await
}
pub(super) async fn observe(
    tx: &DatabaseTransaction,
    binding: &Arc<Binding>,
    p: &store::Prepared,
    execution: &bitrouter_ai::tenancy::identity::ExecutionId,
    observation: &owner::Observation,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    sql::terminal::observe(tx, binding, p, execution, observation, budget).await
}
pub(super) async fn finish(
    tx: &DatabaseTransaction,
    binding: &Arc<Binding>,
    p: &store::Prepared,
    execution: &bitrouter_ai::tenancy::identity::ExecutionId,
    output: Option<&judge::JudgeOutput>,
    select: bool,
    budget: &OperationBudget<'_>,
) -> Result<store::Status> {
    sql::terminal::finish(tx, binding, p, execution, output, select, budget).await
}
pub(in crate::tenancy) async fn stopped(
    tx: &DatabaseTransaction,
    proof: &crate::harness::resources::managed::session::owned::CleanupProof,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    sql::cleanup::stopped(tx, proof, budget).await
}

/// Only the real native host's private close proof reaches this queue boundary.
pub(crate) async fn closed(
    proof: Arc<crate::harness::resources::managed::session::owned::CleanupProof>,
) -> std::result::Result<(), ErrorCode> {
    queue::native_stopped(proof).await
}
