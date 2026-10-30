//! Existing registry markers shared with H. No sender, registry or execution
//! back-reference is retained by the actor's marker state.
use super::super::{PendingPublication, PendingPublicationPhase};
use crate::{
    gateway::pipeline::settlement::RequiredFinalizationReceipt,
    tenancy::continuation::http::codec::{HttpCrypto, Owner, timestamp},
};
use bitrouter_ai::tenancy::{identity::ExecutionId, operation::ErrorCode};
use chrono::{TimeDelta, Utc};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone)]
pub(crate) struct Ticket {
    pub owner: Owner,
    pub public_id: String,
    pub physical: String,
    pub generation: String,
    pub producer: String,
    pub execution: ExecutionId,
    pub delivery: u64,
}
#[derive(Clone)]
pub(in crate::continuation) struct Publication {
    pub ticket: Ticket,
    pub queued: bool,
    pub abandoned: bool,
}
pub(crate) struct State {
    pub(in crate::continuation) pending: Arc<Mutex<HashMap<u64, PendingPublication>>>,
    pub(in crate::continuation) notify: Arc<tokio::sync::Notify>,
    crypto: OnceLock<Option<HttpCrypto>>,
    pub retention: TimeDelta,
    pub prune: usize,
    pub(in crate::continuation) producer: String,
    closed: AtomicBool,
    barrier: AtomicBool,
    #[cfg(test)]
    lose_prepare_reply: AtomicBool,
}
impl State {
    pub(crate) fn new(
        config: &crate::config::ContinuationConfig,
        producer: String,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            config.retention_days > 0 && (1..=10_000).contains(&config.prune_batch_size),
            "invalid continuation settings"
        );
        let retention = TimeDelta::try_days(i64::from(config.retention_days))
            .ok_or_else(|| anyhow::anyhow!("invalid retention duration"))?;
        let horizon = Utc::now()
            .checked_add_signed(retention)
            .and_then(|t| t.checked_add_signed(retention))
            .ok_or_else(|| anyhow::anyhow!("invalid continuation horizon"))?;
        crate::tenancy::continuation::http::codec::time(&timestamp(horizon))?;
        Ok(Arc::new(Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            notify: Arc::new(tokio::sync::Notify::new()),
            crypto: OnceLock::new(),
            retention,
            prune: config.prune_batch_size.min(32),
            producer,
            closed: AtomicBool::new(false),
            barrier: AtomicBool::new(false),
            #[cfg(test)]
            lose_prepare_reply: AtomicBool::new(false),
        }))
    }
    pub(crate) fn initialize(&self, crypto: Option<HttpCrypto>) -> Result<(), ErrorCode> {
        self.crypto
            .set(crypto)
            .map_err(|_| ErrorCode::AuthorityUnavailable)
    }
    pub(crate) fn crypto(&self) -> Result<&HttpCrypto, ErrorCode> {
        self.crypto
            .get()
            .and_then(Option::as_ref)
            .ok_or(ErrorCode::AuthorityUnavailable)
    }
    pub(crate) fn enabled(&self) -> bool {
        self.crypto.get().is_some_and(Option::is_some)
    }
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    pub(crate) fn acknowledge_barrier(&self) {
        self.barrier.store(true, Ordering::Release);
    }
    pub(crate) fn barrier(&self) -> bool {
        self.barrier.load(Ordering::Acquire)
    }
    pub(crate) fn reserve(
        &self,
        owner: Owner,
        execution: ExecutionId,
        public_id: String,
        delivery: u64,
    ) -> Result<Ticket, ErrorCode> {
        let physical = self
            .crypto()?
            .identity(&owner, &public_id)
            .map_err(|_| ErrorCode::InvalidRequest)?;
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if self.closed() || pending.len() >= super::super::MAX_PENDING_PUBLICATIONS {
            return Err(ErrorCode::CapacityExceeded);
        }
        if pending.contains_key(&delivery) {
            return Err(ErrorCode::InvalidRequest);
        }
        let ticket = Ticket {
            owner,
            execution,
            public_id,
            physical: physical.clone(),
            generation: uuid::Uuid::new_v4().to_string(),
            producer: self.producer.clone(),
            delivery,
        };
        pending.insert(
            delivery,
            PendingPublication {
                continuation_identity: physical,
                generation: ticket.generation.clone(),
                instance_id: ticket.producer.clone(),
                lease_until: timestamp(Utc::now() + TimeDelta::seconds(30)),
                receipt: None,
                phase: PendingPublicationPhase::Reserved,
                tenant: Some(Publication {
                    ticket: ticket.clone(),
                    queued: false,
                    abandoned: false,
                }),
            },
        );
        Ok(ticket)
    }
    /// Run before try_send, under the same marker lock used by abandonment.
    pub(crate) fn queue(
        &self,
        ticket: &Ticket,
        receipt: &RequiredFinalizationReceipt,
    ) -> Result<(), ErrorCode> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let row = pending
            .get_mut(&ticket.delivery)
            .filter(|row| row.generation == ticket.generation)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        let tenant = row.tenant.as_mut().ok_or(ErrorCode::AuthorityUnavailable)?;
        if self.closed() || tenant.queued || tenant.abandoned {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        tenant.queued = true;
        row.receipt = Some(receipt.clone());
        Ok(())
    }
    /// A failed enqueue has no actor custody. A failed/abandoned reply does.
    pub(crate) fn not_admitted(&self, ticket: &Ticket) {
        self.release(ticket);
    }
    pub(crate) fn abandon(&self, ticket: &Ticket) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(row) = pending
            .get_mut(&ticket.delivery)
            .filter(|row| row.generation == ticket.generation)
        {
            let Some(tenant) = &mut row.tenant else {
                return;
            };
            if !tenant.queued {
                pending.remove(&ticket.delivery);
            } else {
                tenant.abandoned = true;
                if row.phase != PendingPublicationPhase::Reserved {
                    row.phase = PendingPublicationPhase::Reconcile;
                }
            }
        }
        drop(pending);
        self.notify.notify_waiters();
    }
    /// Called immediately after acknowledged SQL commit, before deadline checks
    /// or reply. Poison recovery preserves custody; it never manufactures clean.
    pub(crate) fn committed(&self, ticket: &Ticket, lease: &str) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(row) = pending
            .get_mut(&ticket.delivery)
            .filter(|row| row.generation == ticket.generation)
        {
            row.lease_until = lease.to_owned();
            row.phase = if row.tenant.as_ref().is_some_and(|p| p.abandoned) {
                PendingPublicationPhase::Reconcile
            } else {
                PendingPublicationPhase::Prepared
            };
        }
        drop(pending);
        self.notify.notify_waiters();
    }
    pub(crate) fn release(&self, ticket: &Ticket) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending
            .get(&ticket.delivery)
            .is_some_and(|row| row.generation == ticket.generation)
        {
            pending.remove(&ticket.delivery);
        }
        drop(pending);
        self.notify.notify_waiters();
    }
    pub(crate) fn matches(&self, ticket: &Ticket) -> bool {
        self.pending.lock().is_ok_and(|pending| {
            pending.get(&ticket.delivery).is_some_and(|row| {
                row.generation == ticket.generation
                    && row.instance_id == ticket.producer
                    && row.continuation_identity == ticket.physical
                    && row
                        .tenant
                        .as_ref()
                        .is_some_and(|p| p.ticket.owner == ticket.owner)
            })
        })
    }
    pub(crate) fn hides(&self, physical: &str) -> Result<bool, ErrorCode> {
        Ok(self
            .pending
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .values()
            .any(|row| row.continuation_identity == physical))
    }
    pub(crate) fn has_generation(
        &self,
        physical: &str,
        generation: &str,
    ) -> Result<bool, ErrorCode> {
        Ok(self
            .pending
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .values()
            .any(|row| row.continuation_identity == physical && row.generation == generation))
    }
    #[cfg(test)]
    pub(crate) fn lose_next_prepare_reply(&self) {
        self.lose_prepare_reply.store(true, Ordering::Release);
    }
    #[cfg(test)]
    pub(crate) fn take_lost_reply(&self) -> bool {
        self.lose_prepare_reply.swap(false, Ordering::AcqRel)
    }
    #[cfg(test)]
    pub(crate) fn pending_tickets(&self) -> Result<Vec<Ticket>, ErrorCode> {
        Ok(self
            .pending
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .values()
            .filter_map(|p| p.tenant.as_ref().map(|p| p.ticket.clone()))
            .collect())
    }
    pub(crate) fn pending_for(&self, execution: &ExecutionId) -> Result<bool, ErrorCode> {
        Ok(self
            .pending
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .values()
            .any(|row| {
                row.tenant
                    .as_ref()
                    .is_some_and(|p| &p.ticket.execution == execution)
            }))
    }
}
/// Dropping a waiter or a disconnected pipeline only marks abandonment. The
/// actor alone releases a queued reservation after known no-write/rollback.
pub(crate) struct Reservation {
    pub state: Arc<State>,
    pub ticket: Ticket,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.state.abandon(&self.ticket);
    }
}
