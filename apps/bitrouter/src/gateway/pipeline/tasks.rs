//! Settlement registration shared by the pipeline and its delivery guards.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::task::JoinSet;

use crate::gateway::error::BitrouterError;

#[derive(Clone, Default)]
pub(super) struct Settlements {
    local: Arc<Mutex<JoinSet<()>>>,
    pub owned: Option<Arc<super::owned_tasks::OwnedTasks>>,
}

impl Settlements {
    pub fn spawn(&self, work: impl Future<Output = ()> + Send + 'static) {
        if let Some(owned) = &self.owned {
            owned.spawn_settlement(work);
            return;
        }
        let mut set = self.local.lock().unwrap_or_else(|error| error.into_inner());
        while set.try_join_next().is_some() {}
        set.spawn(work);
    }

    pub fn finalizer_failed(&self, error: &BitrouterError) {
        if let Some(owned) = &self.owned {
            owned.record_finalizer_failure();
        }
        tracing::error!(%error, "RequiredFinalizer failed");
    }

    pub async fn drain_local(&self) -> usize {
        let mut taken = {
            let mut set = self.local.lock().unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *set)
        };
        let mut drained = 0;
        while taken.join_next().await.is_some() {
            drained += 1;
        }
        drained
    }
}
