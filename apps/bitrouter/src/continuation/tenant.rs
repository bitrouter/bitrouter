//! Tenant mode of the existing registry/runtime; no separate worker or cache.
pub(crate) mod runtime;
pub(crate) mod state;
use super::*;
use crate::tenancy::continuation::http::{
    Port, Step,
    store::{Cursor, Scan},
};

pub(super) struct Backend {
    pub port: Arc<Port>,
    cursors: Mutex<[Cursor; 3]>,
}
impl ContinuationRegistry {
    pub(crate) fn for_tenant(port: Arc<Port>) -> Result<Self> {
        // The public historical database() diagnostic keeps its return type.
        // This variant holds no live SQL connection or ambient Legacy key.
        Ok(Self {
            inner: Arc::new(ContinuationRegistryInner {
                db: DatabaseConnection::Disconnected,
                keys: ContinuationKeySource(Arc::new(ContinuationKeySourceInner::Unavailable)),
                retention: port.state.retention,
                prune_batch_size: port.state.prune as u64,
                instance_id: port.state.producer.clone(),
                pending_capacity: MAX_PENDING_PUBLICATIONS,
                pending_publications: port.state.pending.clone(),
                pending_bind_locks: Arc::new(Mutex::new(HashMap::new())),
                reconciler: ReconcilerControl {
                    notify: port.state.notify.clone(),
                    task: Mutex::new(None),
                    pending_cursor: Mutex::new(None),
                    stale_offset: Mutex::new(0),
                    owned: Some(owned::Control::default()),
                },
                tenant: Some(Backend {
                    port,
                    cursors: Mutex::new(Default::default()),
                }),
                #[cfg(feature = "multi-instance")]
                shared: None,
                #[cfg(test)]
                ambiguous_insert_fault: Arc::new(Mutex::new(None)),
                #[cfg(test)]
                maintenance_fault: Arc::new(Mutex::new(None)),
            }),
        })
    }
    fn tenant_backend(&self) -> Result<&Backend> {
        self.inner
            .tenant
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("tenant continuation backend unavailable"))
    }
    pub(super) async fn tenant_reconcile(&self) -> bool {
        let mut error = self.tenant_reconcile_pending().await;
        let Ok(backend) = self.tenant_backend() else {
            return true;
        };
        for (index, scan) in [Scan::Stale, Scan::Expired, Scan::Purge]
            .into_iter()
            .enumerate()
        {
            let cursor = match backend.cursors.lock() {
                Ok(c) => c[index].clone(),
                Err(_) => return true,
            };
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            match backend.port.maintain(scan, cursor, deadline).await {
                Ok((next, _)) => match backend.cursors.lock() {
                    Ok(mut c) => c[index] = next,
                    Err(_) => return true,
                },
                Err(_) => error = true,
            }
        }
        error
    }
    pub(super) async fn tenant_reconcile_pending(&self) -> bool {
        let Ok(backend) = self.tenant_backend() else {
            return true;
        };
        let pending = {
            let Ok(pending) = self.inner.pending_publications.lock() else {
                return true;
            };
            let mut eligible = pending
                .iter()
                .filter(|(_, p)| p.phase != PendingPublicationPhase::Reserved)
                .map(|(id, p)| (*id, p.clone()))
                .collect::<Vec<_>>();
            eligible.sort_unstable_by_key(|(id, _)| *id);
            let Ok(mut cursor) = self.inner.reconciler.pending_cursor.lock() else {
                return true;
            };
            let start = cursor.map_or(0, |previous| {
                eligible.partition_point(|(id, _)| *id <= previous)
            });
            let selected = eligible
                .iter()
                .skip(start)
                .chain(eligible.iter().take(start))
                .take(RECONCILIATION_BATCH_SIZE)
                .cloned()
                .collect::<Vec<_>>();
            *cursor = selected.last().map(|(id, _)| *id);
            selected
        };
        let mut error = false;
        for (id, snapshot) in pending {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let lock = self.pending_identity_lock(&snapshot.continuation_identity);
            let Ok(_guard) = tokio::time::timeout_at(deadline, lock.lock()).await else {
                error = true;
                continue;
            };
            let Some(current) = self.pending_publication(id, snapshot.receipt.as_ref()) else {
                continue;
            };
            let Some(owned) = current.tenant else {
                error = true;
                continue;
            };
            let result = if current.phase == PendingPublicationPhase::Reconcile {
                backend.port.compensate(owned.ticket, deadline).await
            } else {
                backend
                    .port
                    .advance(owned.ticket, Step::Renew, deadline)
                    .await
            };
            error |= result.is_err();
        }
        error
    }
}
