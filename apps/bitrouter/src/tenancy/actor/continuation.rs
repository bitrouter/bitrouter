//! HTTP publication transitions run in the existing H writer. Cleanup verifies
//! origin and generation, not current caller grants; it cannot admit a send.
use super::{Control, ErrorCode, Failure, Worker};
use crate::{
    continuation::tenant::state::Ticket,
    storage::operation_budget::OperationBudget,
    tenancy::continuation::http::{
        Compensated, Job, Reply, Step, Work,
        codec::{Metadata, Payload, VERSION, time, timestamp},
        store::{self, Cursor, Row, Scan},
    },
};
use bitrouter_ai::tenancy::authority::TenantState;
use chrono::{TimeDelta, Utc};
use std::sync::{Arc, Weak};

impl Worker {
    fn http_error(&self, error: store::Error) -> Failure {
        match error {
            store::Error::Rejected(code) => code.into(),
            store::Error::Integrity => self.unavailable(ErrorCode::AuthorityUnavailable),
            store::Error::Backend => self.unavailable(ErrorCode::StorageUnavailable),
        }
    }
    fn http_origin(&self, job: &Job, control: &Control) -> Result<(), ErrorCode> {
        if !Weak::ptr_eq(&job.origin, &Arc::downgrade(&self.proof)) || control.check_read().is_err()
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        if tokio::time::Instant::now() >= job.deadline {
            return Err(ErrorCode::CapacityExceeded);
        }
        Ok(())
    }
    pub(super) async fn execute_continuation(
        &self,
        job: &Job,
        control: &Control,
    ) -> Result<Reply, Failure> {
        self.http_origin(job, control)?;
        if matches!(job.work, Work::CloseProducer) {
            self.http_continuation.acknowledge_barrier();
            return Ok(Reply::Unit);
        }
        let crypto = self.http_continuation.crypto()?;
        let check = || {
            self.http_origin(job, control)?;
            if let Work::Resolve {
                access,
                public_id: _,
                _permit: _,
            } = &job.work
            {
                let current = self.current().map_err(|e| e.0.code)?;
                access.validate(&current, &self.proof)?;
            }
            // Caller rotation/revocation does not cancel an admitted terminal.
            // Tenant inactivity separately forbids new publication (M0). The
            // same actor serializes this check with lifecycle acknowledgement.
            if let Work::Prepare { ticket, .. }
            | Work::Advance {
                ticket,
                step: Step::Delivering | Step::Active,
            } = &job.work
            {
                let current = self.current().map_err(|error| error.0.code)?;
                if current.installation != *ticket.owner.installation() {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                if !current
                    .tenants
                    .get(ticket.owner.tenant())
                    .is_some_and(|tenant| tenant.state == TenantState::Active)
                {
                    return Err(ErrorCode::TenantInactive);
                }
            }
            Ok(())
        };
        check()?;
        let budget = OperationBudget::new(&check);
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let result = async {
            check()?;
            match &job.work {
                Work::Resolve {
                    access,
                    public_id,
                    _permit: _,
                } => {
                    let id = crypto
                        .identity(access.owner(), public_id)
                        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
                    let result = if self.http_continuation.hides(&id)? {
                        store::Resolution::Missing
                    } else {
                        store::resolve_in(
                            &tx,
                            crypto,
                            access.owner(),
                            public_id,
                            Utc::now(),
                            &budget,
                        )
                        .await
                        .map_err(|e| self.http_error(e))?
                    };
                    Ok((Reply::Resolved(result), false, None))
                }
                Work::Prepare { ticket, proof } => {
                    if self.http_continuation.barrier() || !self.http_continuation.matches(ticket) {
                        return Err(ErrorCode::AuthorityUnavailable.into());
                    }
                    let payload = proof.payload(ticket).map_err(Failure::from)?;
                    if let Some(existing) =
                        store::load(&tx, crypto, &ticket.owner, &ticket.public_id, &budget)
                            .await
                            .map_err(|e| self.http_error(e))?
                    {
                        let retained = existing.open(crypto).map_err(|e| self.http_error(e))?;
                        if existing.metadata.publication_state != "active"
                            || !proof.same_source(&retained)
                        {
                            return Err(ErrorCode::InvalidRequest.into());
                        }
                        return Ok((Reply::Prepared(false), false, Some(Marker::Release(ticket))));
                    }
                    let now = Utc::now();
                    let expires = now
                        .checked_add_signed(self.http_continuation.retention)
                        .ok_or(ErrorCode::CapacityExceeded)?;
                    let purge = expires
                        .checked_add_signed(self.http_continuation.retention)
                        .ok_or(ErrorCode::CapacityExceeded)?;
                    let live = payload
                        .live
                        .as_ref()
                        .ok_or(ErrorCode::AuthorityUnavailable)?;
                    let metadata = Metadata {
                        installation_id: ticket.owner.installation().as_str().into(),
                        tenant_id: ticket.owner.tenant().as_str().into(),
                        creator_principal_id: ticket.owner.principal().as_str().into(),
                        continuation_identity: ticket.physical.clone(),
                        owner_identity: crypto
                            .owner_tag(&ticket.owner)
                            .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?,
                        target_fingerprint: crypto
                            .target_tag(&live.binding)
                            .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?,
                        key_id: crypto.id().into(),
                        cipher_version: VERSION,
                        created_at: timestamp(now),
                        expires_at: timestamp(expires),
                        purge_after: timestamp(purge),
                        publication_state: "provisional".into(),
                        publication_generation: ticket.generation.clone(),
                        publication_instance_id: ticket.producer.clone(),
                        publication_lease_until: timestamp(now + TimeDelta::seconds(30)),
                    };
                    let row =
                        Row::seal(crypto, metadata, &payload).map_err(|e| self.http_error(e))?;
                    store::insert_in(&tx, &row, &budget)
                        .await
                        .map_err(|e| self.http_error(e))?;
                    Ok((
                        Reply::Prepared(true),
                        true,
                        Some(Marker::Committed(
                            ticket,
                            row.metadata.publication_lease_until,
                        )),
                    ))
                }
                Work::Advance { ticket, step } => {
                    if !self.http_continuation.matches(ticket) {
                        return Err(ErrorCode::InvalidRequest.into());
                    }
                    let Some(old) =
                        store::load(&tx, crypto, &ticket.owner, &ticket.public_id, &budget)
                            .await
                            .map_err(|e| self.http_error(e))?
                    else {
                        return Err(self.unavailable(ErrorCode::AuthorityUnavailable));
                    };
                    if !owns(ticket, &old) {
                        return Err(self.unavailable(ErrorCode::AuthorityUnavailable));
                    }
                    let mut metadata = old.metadata.clone();
                    match step {
                        Step::Delivering if metadata.publication_state == "provisional" => {
                            metadata.publication_state = "delivering".into()
                        }
                        Step::Active if metadata.publication_state == "delivering" => {
                            metadata.publication_state = "active".into()
                        }
                        Step::Renew => {}
                        _ => return Err(self.unavailable(ErrorCode::AuthorityUnavailable)),
                    }
                    metadata.publication_lease_until =
                        timestamp(Utc::now() + TimeDelta::seconds(30));
                    let payload = old.open(crypto).map_err(|e| self.http_error(e))?;
                    let new =
                        Row::seal(crypto, metadata, &payload).map_err(|e| self.http_error(e))?;
                    store::replace_in(&tx, &old, &new, &budget)
                        .await
                        .map_err(|e| self.http_error(e))?;
                    Ok((
                        Reply::Unit,
                        true,
                        Some(Marker::Committed(
                            ticket,
                            new.metadata.publication_lease_until,
                        )),
                    ))
                }
                Work::Compensate { ticket } => {
                    if !self.http_continuation.matches(ticket) {
                        return Ok((Reply::Unit, false, None));
                    }
                    let old = store::load(&tx, crypto, &ticket.owner, &ticket.public_id, &budget)
                        .await
                        .map_err(|e| self.http_error(e))?;
                    let mut changed = false;
                    if let Some(old) = old.filter(|row| owns(ticket, row)) {
                        store::delete_in(&tx, &old, &budget)
                            .await
                            .map_err(|e| self.http_error(e))?;
                        changed = true;
                    }
                    Ok((Reply::Unit, changed, Some(Marker::Release(ticket))))
                }
                Work::Maintain { scan, cursor } => {
                    let now = Utc::now();
                    let rows = store::scan_in(&tx, crypto, *scan, cursor, now, &budget)
                        .await
                        .map_err(|e| self.http_error(e))?;
                    let mut next = Cursor::default();
                    let mut changed = false;
                    let more = rows.len() >= self.http_continuation.prune;
                    for old in rows.into_iter().take(self.http_continuation.prune) {
                        check()?;
                        let timestamp = match scan {
                            Scan::Stale => &old.metadata.publication_lease_until,
                            Scan::Expired => &old.metadata.expires_at,
                            Scan::Purge => &old.metadata.purge_after,
                        };
                        next.after = Some((
                            timestamp.clone(),
                            old.metadata.continuation_identity.clone(),
                        ));
                        if self.http_continuation.has_generation(
                            &old.metadata.continuation_identity,
                            &old.metadata.publication_generation,
                        )? {
                            continue;
                        }
                        let payload = old.open(crypto).map_err(|e| self.http_error(e))?;
                        match scan {
                            Scan::Expired
                                if old.metadata.publication_state == "active"
                                    && payload.live.is_some() =>
                            {
                                let tombstone = Payload {
                                    format: payload.format,
                                    owner: payload.owner,
                                    public_id: payload.public_id,
                                    live: None,
                                };
                                let new = Row::seal(crypto, old.metadata.clone(), &tombstone)
                                    .map_err(|e| self.http_error(e))?;
                                store::replace_in(&tx, &old, &new, &budget)
                                    .await
                                    .map_err(|e| self.http_error(e))?;
                                changed = true;
                            }
                            Scan::Stale
                                if old.metadata.publication_state != "active"
                                    && time(&old.metadata.publication_lease_until).map_err(
                                        |_| self.unavailable(ErrorCode::AuthorityUnavailable),
                                    )? <= now =>
                            {
                                store::delete_in(&tx, &old, &budget)
                                    .await
                                    .map_err(|e| self.http_error(e))?;
                                changed = true;
                            }
                            Scan::Purge
                                if time(&old.metadata.purge_after).map_err(|_| {
                                    self.unavailable(ErrorCode::AuthorityUnavailable)
                                })? <= now =>
                            {
                                store::delete_in(&tx, &old, &budget)
                                    .await
                                    .map_err(|e| self.http_error(e))?;
                                changed = true;
                            }
                            _ => {}
                        }
                    }
                    if !more {
                        next.after = None;
                    }
                    Ok((Reply::Maintained(next, more), changed, None))
                }
                Work::CloseProducer => Err(ErrorCode::AuthorityUnavailable.into()),
            }
        }
        .await;
        let (reply, changed, marker) = match result {
            Ok(value) => value,
            Err(error) => {
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                return Err(error);
            }
        };
        // CPU-only payload work also consumes the caller's deadline. A known
        // rollback must settle before releasing any pending publication.
        if let Err(error) = check() {
            control
                .rollback(tx)
                .await
                .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
            return Err(error.into());
        }
        if changed {
            control.commit(tx).await
        } else {
            control.rollback(tx).await
        }
        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        // No fallible check, allocation of a reply, or send may precede this
        // publication of acknowledged custody to the existing pending map.
        if let Some(marker) = marker {
            match marker {
                Marker::Release(ticket) => self.http_continuation.release(ticket),
                Marker::Committed(ticket, lease) => {
                    self.http_continuation.committed(ticket, &lease)
                }
            }
        }
        if tokio::time::Instant::now() >= job.deadline {
            if job.work.read() {
                // The explicit read rollback is acknowledged; expiry is not an
                // ambiguous write and must not fence an otherwise live owner.
                return Err(ErrorCode::CapacityExceeded.into());
            }
            self.proof.fenced();
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        if let Work::Compensate { ticket } = &job.work {
            // This reply is minted only after explicit SQL completion, marker
            // release and deadline checks. Lost receivers cannot use it as proof.
            return Ok(Reply::Compensated(Box::new(Compensated::acknowledged(
                ticket.clone(),
                job.origin.clone(),
            ))));
        }
        Ok(reply)
    }
}
enum Marker<'a> {
    Release(&'a Ticket),
    Committed(&'a Ticket, String),
}
fn owns(ticket: &Ticket, row: &Row) -> bool {
    row.metadata.continuation_identity == ticket.physical
        && row.metadata.publication_generation == ticket.generation
        && row.metadata.publication_instance_id == ticket.producer
}
