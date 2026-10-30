//! Sealed evidence from this Registry's actual joined producers, not stored IDs.
use super::{Completion, Entry, Registry};
use crate::tenancy::learning::{owner::Owner, source::Binding};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
};
use tokio::time::Instant;

/// Only the closed Registry can construct this nonempty exact remaining set.
/// Keeping the owners alive also retains their original leases and proposals.
pub(in crate::tenancy) struct StopSet {
    first: Arc<Owner>,
    rest: Vec<Arc<Owner>>,
}
impl StopSet {
    pub(in crate::tenancy) fn binding(&self) -> &Binding {
        &self.first.binding
    }
    pub(in crate::tenancy) fn owners(&self) -> impl Iterator<Item = &Arc<Owner>> {
        std::iter::once(&self.first).chain(self.rest.iter())
    }
}
impl Registry {
    pub(in crate::tenancy) async fn paid_stop_set(
        &self,
        deadline: Instant,
    ) -> Result<Option<Arc<StopSet>>> {
        self.close();
        // Serialize with ordinary full-drain retries, not with independent
        // producer completion. No registry mutex crosses an await or SQL.
        tokio::time::timeout_at(deadline, async {
            let _draining = self.draining.lock().await;
            let entries: Vec<_> = {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("registry unavailable"))?;
                ensure!(
                    state.closed && state.entries.len() <= super::MAX_ACTIVE,
                    "registry coverage bound"
                );
                state.entries.values().cloned().collect()
            };
            let mut covered = Vec::new();
            let mut counts = BTreeMap::new();
            for entry in entries {
                if let Some(owner) = entry.stopped_paid().await? {
                    ensure!(covered.len() < 8, "paid stop set capacity");
                    let seed = owner.recovery.as_ref().context("paid origin absent")?;
                    let count = counts.entry(seed.tenant.clone()).or_insert(0usize);
                    *count += 1;
                    ensure!(*count <= 2, "paid tenant stop set capacity");
                    covered.push(owner);
                }
            }
            covered.sort_by(|a, b| a.execution.as_str().cmp(b.execution.as_str()));
            let mut covered = covered.into_iter();
            let Some(first) = covered.next() else {
                return Ok(None);
            };
            Ok(Some(Arc::new(StopSet {
                first,
                rest: covered.collect(),
            })))
        })
        .await
        .context("paid shutdown coverage deadline")?
    }
}
impl Entry {
    async fn stopped_paid(&self) -> Result<Option<Arc<Owner>>> {
        let task = self
            .task
            .lock()
            .map_err(|_| anyhow::anyhow!("registry task unavailable"))?
            .clone()
            .context("registry task absent")?;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.effects_stopped.load(Ordering::Acquire) {
                break;
            }
            tokio::select! {
                result = task.clone() => return match result {
                    Completion::Drained => Ok(None),
                    Completion::Retryable(_) | Completion::Failed(_) => anyhow::bail!("uncovered local execution effects"),
                },
                () = &mut changed => {}
            }
        }
        if let Some(receipt) = &self.receipt
            && let Some(owner) = receipt
                .stopped_learning()
                .map_err(|_| anyhow::anyhow!("uncovered auxiliary effect"))?
            && owner
                .stopped()
                .await
                .map_err(|_| anyhow::anyhow!("paid producer outcome unconfirmed"))?
        {
            ensure!(
                owner.execution == self.id,
                "paid registry identity mismatch"
            );
            return Ok(Some(owner));
        }
        // Finished/unused paid owners and all ordinary/native owners must be
        // fully joined; their absence is never inferred from an old row.
        match task.await {
            Completion::Drained => Ok(None),
            Completion::Retryable(_) | Completion::Failed(_) => {
                anyhow::bail!("uncovered execution owner")
            }
        }
    }
}
