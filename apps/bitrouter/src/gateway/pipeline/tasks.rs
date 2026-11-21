//! Settlement registration shared by the pipeline and its delivery guards.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::task::JoinSet;

use crate::gateway::error::BitrouterError;

/// Retain the native HTTP trace parent across an owned task boundary.
pub(super) fn with_telemetry_context<F: Future>(work: F) -> impl Future<Output = F::Output> {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    {
        opentelemetry::context::FutureExt::with_current_context(work)
    }
    #[cfg(not(any(feature = "otel-http", feature = "otel-grpc")))]
    work
}

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
        set.spawn(with_telemetry_context(work));
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
