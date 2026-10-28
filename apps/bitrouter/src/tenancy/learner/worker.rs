//! The assembled host owns and joins this mode of its existing scheduler.
use super::{
    queue::{self, Reply, Work},
    source,
    store::Cursor,
};
use crate::tenancy::Service;
use anyhow::{Result, anyhow};
use bitrouter_ai::tenancy::operation::OperationResult;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct Running(Arc<std::sync::atomic::AtomicBool>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(crate) fn start(
    service: Service,
    gate: crate::daemon_handoff::HandoffGate,
    shutdown: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if service
        .learning_worker
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    let running = Running(service.learning_worker.clone());
    Some(tokio::spawn(async move {
        let _running = running;
        let mut cursor = Cursor::default();
        let mut clock = tokio::time::interval(Duration::from_secs(2));
        clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = shutdown.cancelled() => break, _ = clock.tick() => {} }
            if !service.live.load(Ordering::Acquire) {
                break;
            }
            let Some(_handoff) = gate.admit() else {
                continue;
            };
            if pass(&service, &mut cursor, &shutdown).await.is_err() {
                tracing::debug!("owned learner pass deferred");
            }
        }
    }))
}

/// The daemon and actual-consumer tests share this exact bounded pass.
pub(in crate::tenancy) async fn pass(
    service: &Service,
    cursor: &mut Cursor,
    shutdown: &CancellationToken,
) -> Result<Vec<OperationResult>> {
    let Reply::Scanned(page) = queue::request(
        service,
        Work::Scan(cursor.clone()),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .map_err(|_| anyhow!("owned learning scan unavailable"))?
    else {
        return Err(anyhow!("invalid learner scan reply"));
    };
    *cursor = page.cursor;
    let mut published = Vec::new();
    for registration in page.registrations {
        if shutdown.is_cancelled() {
            break;
        }
        if !registration.input.enabled {
            continue;
        }
        // This is the existing native host semaphore, not a second execution or
        // assignment registry. A pending native cleanup keeps its original slot.
        let stopped = match service.stopped_learner_source() {
            Ok(value) => value,
            Err(_) => continue,
        };
        let started = Instant::now();
        let deadline = started + Duration::from_secs(40);
        let acquired = match queue::request(
            service,
            Work::Acquire {
                registration,
                stopped,
                deadline,
            },
            started + Duration::from_secs(5),
        )
        .await
        {
            Ok(Reply::Acquired(value)) => value,
            _ => continue,
        };
        let owned = acquired.clone();
        let mut compute = tokio::task::spawn_blocking(move || source::calculate(&owned));
        let compute_deadline = (Instant::now() + Duration::from_secs(30)).min(acquired.deadline);
        let result = tokio::time::timeout_at(compute_deadline, &mut compute).await;
        let calculated = match result {
            Ok(Ok(Ok(value))) if !shutdown.is_cancelled() => value,
            Ok(_) => continue,
            Err(_) => {
                // Never release the shared learning lease merely because the
                // response deadline passed. The CPU closure owns Acquired too.
                if tokio::time::timeout(Duration::from_secs(30), &mut compute)
                    .await
                    .is_err()
                {
                    service.cleanup.quarantine();
                    // Dropping this waiter cannot stop spawn_blocking; its exact
                    // source/lease owners survive until the closure really exits.
                    return Err(anyhow!("owned learner compute cleanup unconfirmed"));
                }
                continue;
            }
        };
        #[cfg(all(test, unix))]
        service.learner_test.before_publish().await;
        let storage_deadline = (Instant::now() + Duration::from_secs(5)).min(acquired.deadline);
        match queue::request(
            service,
            Work::Publish {
                acquired,
                calculated: Box::new(calculated),
                shutdown: shutdown.clone(),
            },
            storage_deadline,
        )
        .await
        {
            Ok(Reply::Published(result)) => {
                tracing::info!(operation = %result.operation_id, configuration = ?result.resource_revision,
                    "owned learned configuration retained; activation requires current configuration CAS");
                published.push(result);
            }
            _ => continue,
        }
    }
    Ok(published)
}
