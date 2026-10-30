//! Checked native assessment commands, serialized by the existing native owner.
pub(in crate::tenancy) mod contribution;
pub(in crate::tenancy) mod store;

use crate::{
    acp_trajectory::checkpoint::types::AssessmentRevision,
    core::evolution::scoring::ScoringReceipt,
    harness::resources::managed::session::assessment::NativeRunReview,
    storage::operation_budget::{Error, OperationBudget},
    tenancy::{capture::human::Input, execution::session::Binding},
};
use anyhow::Result;
use bitrouter_ai::tenancy::{
    authority::{GrantScope, Permission},
    operation::ErrorCode,
};
use sea_orm::DatabaseTransaction;
use std::sync::Arc;
use tokio::{
    sync::{OwnedSemaphorePermit, oneshot},
    time::Instant,
};

pub(crate) enum Reply {
    Review(Box<NativeRunReview>),
    Submitted(ScoringReceipt),
    Retracted(AssessmentRevision),
}
pub(crate) struct Request {
    pub(in crate::tenancy) binding: Arc<Binding>,
    input: Input,
    retained:
        crate::tenancy::cache::Pin<crate::tenancy::resources::execution::RetainedExecutionRevision>,
    pub(in crate::tenancy) deadline: Instant,
    _command: OwnedSemaphorePermit,
}
impl Request {
    pub(crate) async fn new(
        binding: Arc<Binding>,
        input: Input,
        deadline: Instant,
        command: OwnedSemaphorePermit,
    ) -> Result<Self> {
        let input = crate::tenancy::capture::human::normalize(
            input,
            binding.descriptor().principal.as_str(),
        )?;
        check(&binding, !matches!(input, Input::Review(_))).map_err(Error::Rejected)?;
        let retained = tokio::time::timeout_at(deadline, binding.authority().host().retained())
            .await
            .map_err(|_| Error::Rejected(ErrorCode::CapacityExceeded))?
            .map_err(|failure| Error::Rejected(failure.0.code))?;
        let value = Self {
            binding,
            input,
            retained,
            deadline,
            _command: command,
        };
        value.check().map_err(Error::Rejected)?;
        Ok(value)
    }
    pub(in crate::tenancy) fn check(&self) -> std::result::Result<(), ErrorCode> {
        if Instant::now() >= self.deadline {
            return Err(ErrorCode::CapacityExceeded);
        }
        check(&self.binding, !matches!(self.input, Input::Review(_)))
    }
    pub(in crate::tenancy) async fn execute(
        &self,
        tx: &DatabaseTransaction,
        budget: &OperationBudget<'_>,
    ) -> Result<(Reply, bool)> {
        self.check().map_err(Error::Rejected)?;
        let result = store::execute(tx, &self.binding, &self.retained, &self.input, budget).await?;
        self.check().map_err(Error::Rejected)?;
        Ok(result)
    }
}

pub(crate) fn check(binding: &Binding, write: bool) -> std::result::Result<(), ErrorCode> {
    crate::tenancy::experiment::queue::check_reader(binding)?;
    binding
        .authority()
        .host()
        .with_authority(binding.authority().dependencies(), |effective| {
            if write && !effective.permits(Permission::EvaluationSubmit) {
                return Err(ErrorCode::Forbidden);
            }
            Ok(())
        })
}

pub(crate) fn bounded(
    input: Input,
) -> std::result::Result<Input, bitrouter_ai::core::protocol::CoreError> {
    crate::tenancy::capture::human::normalize(input, "pre-admission").map_err(|_| {
        bitrouter_ai::core::protocol::CoreError::rejected(
            bitrouter_ai::core::protocol::ErrorCode::LimitExceeded,
            "native assessment input is invalid or oversized",
        )
    })
}

pub(crate) fn deliver(
    binding: &Binding,
    write: bool,
    deadline: Instant,
    reply: oneshot::Sender<std::result::Result<Reply, bitrouter_ai::core::protocol::CoreError>>,
    outcome: std::result::Result<Reply, bitrouter_ai::core::protocol::CoreError>,
) {
    if outcome.is_err() {
        if reply.send(outcome).is_err() {
            tracing::debug!("native assessment waiter disconnected");
        }
        return;
    }
    let mut pending = Some((reply, outcome));
    let checked = check(binding, write).and_then(|()| {
        binding
            .authority()
            .host()
            .with_authority(binding.authority().dependencies(), |effective| {
                if Instant::now() >= deadline
                    || !effective.permits(Permission::ResourceRead)
                    || (write && !effective.permits(Permission::EvaluationSubmit))
                    || !effective.covers(&GrantScope::Tenant {
                        operator_pools: Vec::new(),
                    })
                {
                    return Err(ErrorCode::Forbidden);
                }
                let (reply, value) = pending.take().ok_or(ErrorCode::AuthorityUnavailable)?;
                if reply.send(value).is_err() {
                    tracing::debug!("native assessment waiter disconnected");
                }
                Ok(())
            })
    });
    if checked.is_err()
        && let Some((reply, _)) = pending
    {
        let mut error = bitrouter_ai::core::protocol::CoreError::rejected(
            bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope,
            "native assessment delivery is no longer authorized",
        );
        if write {
            error.commit_status = bitrouter_ai::core::protocol::CommitStatus::Committed;
        }
        if reply.send(Err(error)).is_err() {
            tracing::debug!("native assessment rejection has no waiter");
        }
    }
}
