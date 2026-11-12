//! One disposable optional task owned by the existing control loop. There is
//! no queue, resident worker, receipt participant or financial pending state.
use super::Worker;
use crate::tenancy::{
    admission::Admission,
    ownership::Lease,
    receipts::{owner::Owner, provider_technical::storage, store::format::Terminal},
};
use futures::FutureExt;
use std::{future::pending, sync::Arc};
use tokio::{task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;

mod connection;
#[cfg(test)]
pub(in crate::tenancy) mod tests;

struct Charge {
    admission: Arc<Admission>,
    logical: u64,
    physical: u64,
}
impl Charge {
    // The caller must have positively closed every possible writer. Unknown
    // row presence keeps the one logical charge; it cannot fund another write.
    fn settled(self, retain: bool, disable: bool) -> Report {
        let counted = self
            .admission
            .finish_optional_provider_write(self.logical, self.physical, retain)
            .is_ok();
        // Resource closure and optional bookkeeping are separate facts. A
        // refused counter settlement disables diagnostics, not main shutdown.
        Report {
            closed: true,
            disable: disable || !counted,
        }
    }
}
// No Drop refund: cancellation/panic says nothing about a queued SQL command.

struct Report {
    closed: bool,
    disable: bool,
}
#[derive(Default)]
pub(super) struct Slot {
    task: Option<JoinHandle<Report>>,
    cancel: CancellationToken,
    disabled: bool,
    unresolved: bool,
}
impl Slot {
    pub(super) fn preempt(&self) {
        self.cancel.cancel();
    }
    pub(super) fn offer(&mut self, worker: &Worker, owner: &Arc<Owner>) {
        let Some((terminal, facts)) = owner.take_provider_technical() else {
            return;
        };
        #[cfg(test)]
        if let Some(view) = facts.operation_view(&terminal) {
            worker.technical_io.captured(&view);
        }
        if self.disabled || self.task.is_some() {
            return;
        }
        let Some(lease) = worker
            .owner
            .as_ref()
            .filter(|lease| lease.exclusive())
            .cloned()
        else {
            return;
        };
        self.cancel = CancellationToken::new();
        let cancel = self.cancel.clone();
        let admission = worker.admission.clone();
        let cleanup = owner.cleanup_deadline();
        #[cfg(test)]
        let test = worker.technical_io.clone();
        self.task = Some(tokio::spawn(async move {
            #[cfg(test)]
            let id = terminal.facts().execution.clone();
            // Keep the same acquisition alive even through an unwinding future.
            let result = std::panic::AssertUnwindSafe(write(
                lease,
                admission,
                terminal,
                facts,
                cancel,
                cleanup,
                #[cfg(test)]
                test.clone(),
            ))
            .catch_unwind()
            .await;
            let report = result.unwrap_or(Report {
                closed: false,
                disable: true,
            });
            #[cfg(test)]
            test.completed(id, report.closed);
            report
        }));
    }
    pub(super) async fn finished(&mut self) -> Result<(), tokio::task::JoinError> {
        let result = match &mut self.task {
            Some(task) => task.await,
            None => pending().await,
        };
        self.task = None;
        match result {
            Ok(report) => {
                self.disabled |= report.disable;
                if !report.closed {
                    // The main queues remain usable. Only successful shutdown
                    // is withheld if actual optional driver closure is unknown.
                    self.disabled = true;
                    self.unresolved = true;
                }
                Ok(())
            }
            Err(error) => {
                self.disabled = true;
                self.unresolved = true;
                Err(error)
            }
        }
    }
    pub(super) async fn finish(&mut self) {
        self.preempt();
        while self.task.is_some() {
            if self.finished().await.is_err() {
                tracing::warn!("optional driver join unconfirmed; owner retained");
            }
        }
        if self.unresolved {
            pending::<()>().await;
        }
    }
}

async fn close(db: connection::Connection) -> bool {
    matches!(
        std::panic::AssertUnwindSafe(db.close())
            .catch_unwind()
            .await,
        Ok(true)
    )
}

async fn write(
    lease: Lease,
    admission: Arc<Admission>,
    terminal: Arc<Terminal>,
    facts: crate::tenancy::receipts::provider_technical::Pending,
    cancel: CancellationToken,
    cleanup: Instant,
    #[cfg(test)] test: Arc<tests::State>,
) -> Report {
    // Scope wait, connection and INSERT all share ONE existing optional bound.
    let deadline = cleanup.min(Instant::now() + crate::tenancy::MANAGEMENT_TIMEOUT);
    let Some(facts) = facts.finish(&terminal, deadline, &cancel).await else {
        return Report {
            closed: true,
            disable: false,
        };
    };
    if cancel.is_cancelled() || Instant::now() >= deadline {
        return Report {
            closed: true,
            disable: false,
        };
    }
    let Ok(prepared) = storage::Prepared::new_v2(&facts) else {
        return Report {
            closed: true,
            disable: false,
        };
    };
    let Ok(encoded) = u64::try_from(prepared.json.len()) else {
        return Report {
            closed: true,
            disable: false,
        };
    };
    let Some(logical) = encoded.checked_add(crate::tenancy::receipts::capacity::ROW_OVERHEAD)
    else {
        return Report {
            closed: true,
            disable: false,
        };
    };
    let Ok(physical) = crate::tenancy::storage::estimate(encoded) else {
        return Report {
            closed: true,
            disable: false,
        };
    };
    if admission
        .reserve_optional_provider_write(logical, physical)
        .is_err()
    {
        return Report {
            closed: true,
            disable: false,
        };
    }
    let charge = Charge {
        admission,
        logical,
        physical,
    };
    if cancel.is_cancelled() {
        return charge.settled(false, false);
    }
    #[cfg(not(test))]
    let readonly = false;
    #[cfg(test)]
    let readonly = test.readonly_install();
    let db = match connection::Connection::open(lease.clone(), readonly).await {
        Ok(db) => db,
        Err(_) => {
            return Report {
                closed: false,
                disable: true,
            };
        }
    };
    let mut insert_polled = false;
    let work = async {
        db.interrupt_when(Some(cancel.clone()), deadline).await?;
        crate::db::migration::provider_technical::install(&db).await?;
        #[cfg(test)]
        test.at(tests::Point::BeforeInsert, &cancel).await;
        if let Some(old) = storage::read_prepared(&db, &terminal).await? {
            anyhow::ensure!(
                old.version == prepared.version && old.json == prepared.json,
                "conflicting optional provider facts"
            );
            return Ok(false);
        }
        #[cfg(test)]
        db.driver_gate(&test, &cancel).await?;
        insert_polled = true;
        let inserted = storage::insert_new(&db, &terminal, &prepared).await?;
        #[cfg(test)]
        test.at(tests::Point::AfterInsert, &cancel).await;
        Ok::<bool, anyhow::Error>(inserted)
    };
    let outcome = {
        let caught = std::panic::AssertUnwindSafe(work).catch_unwind();
        tokio::pin!(caught);
        tokio::select! {biased;
            ()=cancel.cancelled()=>None,
            ()=tokio::time::sleep_until(deadline)=>None,
            result=&mut caught=>Some(result),
        }
    };
    // Even success is not released until this exact disposable connection is closed.
    // close() waits for queued driver commands; a dropped SQL future alone
    // is never an acknowledgement that the underlying operation stopped.
    cancel.cancel();
    if !close(db).await {
        return Report {
            closed: false,
            disable: true,
        };
    }
    if let Some(Ok(Ok(inserted))) = outcome {
        return charge.settled(inserted, false);
    }
    if !insert_polled {
        // A normal preempt/deadline lost only this sample: the exact driver is
        // closed and no child INSERT was polled. A work error or unwind still
        // disables optional capture; unknown writes keep the readback path below.
        return charge.settled(false, outcome.is_some());
    }
    // One readback, AFTER writer closure. This never retries insertion or
    // inference, grants a receipt ACK, or involves the financial Proof.
    let read = match connection::Connection::open(lease.clone(), true).await {
        Ok(read) => read,
        Err(_) => {
            return Report {
                closed: false,
                disable: true,
            };
        }
    };
    let observed = tokio::time::timeout_at(cleanup, async {
        read.interrupt_when(None, cleanup).await?;
        storage::read_prepared(&read, &terminal).await
    })
    .await;
    let present = match observed {
        Ok(Ok(Some(facts))) if facts.version == prepared.version && facts.json == prepared.json => {
            Some(true)
        }
        Ok(Ok(None)) => Some(false),
        _ => None,
    };
    if !close(read).await {
        return Report {
            closed: false,
            disable: true,
        };
    }
    let unknown = present.is_none();
    charge.settled(present.unwrap_or(true), unknown)
}
