//! Narrow registry-to-existing-actor port. It holds no Service or SQL pool.
pub(in crate::tenancy) mod bootstrap;
pub(crate) mod codec;
pub(crate) mod store;

use super::super::{
    actor::Job as ActorJob, error::Failure, execution::http_continuation::Access,
    transaction::Proof,
};
use crate::{
    continuation::tenant::state::{State, Ticket},
    gateway::pipeline::{
        executor::tenancy::http_continuation::Proof as TerminalProof,
        settlement::RequiredFinalizationReceipt,
    },
};
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::sync::{Arc, Weak};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::Instant,
};

pub(crate) struct Port {
    sender: mpsc::Sender<ActorJob>,
    origin: Weak<Proof>,
    reads: Arc<Semaphore>,
    pub state: Arc<State>,
}
impl Port {
    pub(in crate::tenancy) fn new(
        sender: mpsc::Sender<ActorJob>,
        origin: &Arc<Proof>,
        reads: Arc<Semaphore>,
        state: Arc<State>,
    ) -> Arc<Self> {
        Arc::new(Self {
            sender,
            origin: Arc::downgrade(origin),
            reads,
            state,
        })
    }
    fn available(&self, deadline: Instant) -> Result<(), ErrorCode> {
        if Instant::now() >= deadline {
            return Err(ErrorCode::CapacityExceeded);
        }
        let origin = self
            .origin
            .upgrade()
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        if matches!(
            origin.disposition(),
            super::super::transaction::Disposition::Ambiguous
        ) {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        Ok(())
    }
    async fn send(&self, work: Work, deadline: Instant) -> Result<Reply, ErrorCode> {
        self.available(deadline)?;
        let (reply, receive) = oneshot::channel();
        self.sender
            .try_send(ActorJob::Continuation(Box::new(Job {
                origin: self.origin.clone(),
                deadline,
                work,
                reply,
            })))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ErrorCode::CapacityExceeded,
                mpsc::error::TrySendError::Closed(_) => ErrorCode::AuthorityUnavailable,
            })?;
        tokio::time::timeout_at(deadline, receive)
            .await
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .map_err(|error| error.0.code)
    }
    pub(crate) async fn resolve(
        &self,
        access: Access,
        public_id: String,
    ) -> Result<store::Resolution, ErrorCode> {
        if self.state.closed() {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let permit = self
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        let deadline = access.deadline();
        match self
            .send(
                Work::Resolve {
                    access: Box::new(access),
                    public_id,
                    _permit: permit,
                },
                deadline,
            )
            .await?
        {
            Reply::Resolved(value) => Ok(value),
            _ => Err(ErrorCode::AuthorityUnavailable),
        }
    }
    pub(crate) async fn prepare(
        &self,
        ticket: Ticket,
        proof: Arc<TerminalProof>,
        receipt: &RequiredFinalizationReceipt,
        deadline: Instant,
    ) -> Result<bool, ErrorCode> {
        self.available(deadline)?;
        self.state.queue(&ticket, receipt)?;
        // Admission is synchronously decided so a failed enqueue releases only
        // this generation; once sent, timeout/drop cannot release its marker.
        let (reply, receive) = oneshot::channel();
        let job = Job {
            origin: self.origin.clone(),
            deadline,
            work: Work::Prepare {
                ticket: ticket.clone(),
                proof,
            },
            reply,
        };
        if let Err(error) = self.sender.try_send(ActorJob::Continuation(Box::new(job))) {
            self.state.not_admitted(&ticket);
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => ErrorCode::CapacityExceeded,
                mpsc::error::TrySendError::Closed(_) => ErrorCode::AuthorityUnavailable,
            });
        }
        #[cfg(test)]
        if self.state.take_lost_reply() {
            // Lose the real oneshot receiver after successful mailbox admission.
            // The actor/SQL path is unchanged and still owns this generation.
            drop(receive);
            self.state.abandon(&ticket);
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let result = tokio::time::timeout_at(deadline, receive)
            .await
            .map_err(|_| ErrorCode::AuthorityUnavailable)
            .and_then(|reply| reply.map_err(|_| ErrorCode::AuthorityUnavailable))
            .and_then(|reply| reply.map_err(|e| e.0.code));
        if result.is_err() {
            self.state.abandon(&ticket);
        }
        match result? {
            Reply::Prepared(inserted) => Ok(inserted),
            _ => Err(ErrorCode::AuthorityUnavailable),
        }
    }
    pub(crate) async fn advance(
        &self,
        ticket: Ticket,
        step: Step,
        deadline: Instant,
    ) -> Result<(), ErrorCode> {
        match self.send(Work::Advance { ticket, step }, deadline).await? {
            Reply::Unit => Ok(()),
            _ => Err(ErrorCode::AuthorityUnavailable),
        }
    }
    /// An actor reply, not marker absence, proves this generation has no
    /// outstanding publication custody. The caller keeps its identity lock.
    pub(crate) async fn compensate_confirmed(
        &self,
        ticket: Ticket,
        deadline: Instant,
    ) -> Result<Box<Compensated>, ErrorCode> {
        match self.send(Work::Compensate { ticket }, deadline).await? {
            Reply::Compensated(acknowledged) => Ok(acknowledged),
            _ => Err(ErrorCode::AuthorityUnavailable),
        }
    }
    pub(crate) async fn compensate(
        &self,
        ticket: Ticket,
        deadline: Instant,
    ) -> Result<(), ErrorCode> {
        self.compensate_confirmed(ticket, deadline)
            .await
            .map(|_| ())
    }
    pub(crate) async fn maintain(
        &self,
        scan: store::Scan,
        cursor: store::Cursor,
        deadline: Instant,
    ) -> Result<(store::Cursor, bool), ErrorCode> {
        match self.send(Work::Maintain { scan, cursor }, deadline).await? {
            Reply::Maintained(cursor, more) => Ok((cursor, more)),
            _ => Err(ErrorCode::AuthorityUnavailable),
        }
    }
    pub(crate) async fn close_producer(&self, deadline: Instant) -> Result<(), ErrorCode> {
        self.state.close();
        self.available(deadline)?;
        // The acknowledged barrier is monotonic: no later prepare can pass
        // the closed marker gate. Repeated shutdown (including a lost waiter)
        // must not require another message after the owning actor has joined.
        if self.state.barrier() {
            return Ok(());
        }
        match self.send(Work::CloseProducer, deadline).await? {
            Reply::Unit => Ok(()),
            _ => Err(ErrorCode::AuthorityUnavailable),
        }
    }
}
/// Constructed only on the actor's acknowledged compensation return path.
/// This discharges publication custody; it cannot authorize work or delivery.
pub(crate) struct Compensated {
    ticket: Ticket,
    origin: Weak<Proof>,
}
impl Compensated {
    pub(in crate::tenancy) fn acknowledged(ticket: Ticket, origin: Weak<Proof>) -> Self {
        Self { ticket, origin }
    }
    pub(crate) fn matches(&self, port: &Port, ticket: &Ticket) -> bool {
        Weak::ptr_eq(&self.origin, &port.origin)
            && self.ticket.owner == ticket.owner
            && self.ticket.public_id == ticket.public_id
            && self.ticket.physical == ticket.physical
            && self.ticket.generation == ticket.generation
            && self.ticket.producer == ticket.producer
            && self.ticket.execution == ticket.execution
            && self.ticket.delivery == ticket.delivery
    }
}
#[derive(Clone, Copy)]
pub(crate) enum Step {
    Delivering,
    Active,
    Renew,
}
pub(in crate::tenancy) struct Job {
    pub origin: Weak<Proof>,
    pub deadline: Instant,
    pub work: Work,
    pub reply: oneshot::Sender<Result<Reply, Failure>>,
}
pub(in crate::tenancy) enum Work {
    Resolve {
        access: Box<Access>,
        public_id: String,
        _permit: OwnedSemaphorePermit,
    },
    Prepare {
        ticket: Ticket,
        proof: Arc<TerminalProof>,
    },
    Advance {
        ticket: Ticket,
        step: Step,
    },
    Compensate {
        ticket: Ticket,
    },
    Maintain {
        scan: store::Scan,
        cursor: store::Cursor,
    },
    CloseProducer,
}
impl Work {
    pub fn read(&self) -> bool {
        matches!(self, Self::Resolve { .. })
    }
    pub fn preparation(&self) -> Option<&Ticket> {
        match self {
            Self::Prepare { ticket, .. } => Some(ticket),
            _ => None,
        }
    }
    pub fn ticket(&self) -> Option<&Ticket> {
        match self {
            Self::Prepare { ticket, .. }
            | Self::Advance { ticket, .. }
            | Self::Compensate { ticket } => Some(ticket),
            _ => None,
        }
    }
}
pub(in crate::tenancy) enum Reply {
    Resolved(store::Resolution),
    Prepared(bool),
    Compensated(Box<Compensated>),
    Unit,
    Maintained(store::Cursor, bool),
}
