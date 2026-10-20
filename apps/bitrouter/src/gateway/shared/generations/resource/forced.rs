//! Forced joining keeps unresolved durable state and every complete application.

use super::*;

impl Resource {
    pub fn request_force(&self) {
        self.forcing.store(true, Ordering::Release);
        self.mark_retiring();
        {
            let build = self.build.lock().unwrap_or_else(|error| error.into_inner());
            if let Build::Running(task) = &*build {
                task.abort();
            }
        }
        self.force_components();
        self.force_signal.notify_waiters();
    }

    pub fn request_pipeline_force(&self) {
        self.external_joined.store(true, Ordering::Release);
        self.force_components();
    }

    pub(super) fn force_components(&self) {
        let Some(components) = self.components() else {
            return;
        };
        let native_failed = components
            .assembled
            .continuation_registry
            .request_force_owned()
            .is_err();
        let pipeline_failed = match &components.tasks {
            Some(tasks) => {
                tasks.close_sources();
                self.external_joined.load(Ordering::Acquire) && tasks.force().is_err()
            }
            None => true,
        };
        let mut state = self
            .retirement
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.native_failed |= native_failed;
        state.pipeline_failed |= pipeline_failed;
    }

    pub fn forced_report(&self, serving: &Serving) -> ForcedReport {
        let (build_joined, failure, components) = {
            let build = self.build.lock().unwrap_or_else(|error| error.into_inner());
            match &*build {
                Build::Joined(outcome) => (true, outcome.failure, outcome.components.clone()),
                _ => (false, None, None),
            }
        };
        let native = components.as_ref().and_then(|components| {
            components
                .assembled
                .continuation_registry
                .owned_status()
                .ok()
        });
        let pending = components
            .as_ref()
            .and_then(|components| components.tasks.as_ref().map(|tasks| tasks.pending()));
        let snapshot_readers = components
            .as_ref()
            .and_then(|components| components.snapshot.as_ref())
            .map_or(0, |snapshot| Arc::strong_count(snapshot).saturating_sub(1));
        let state = self
            .retirement
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ForcedReport {
            generation: self.id,
            build_joined,
            build_failure: failure,
            has_components: components.is_some(),
            pipeline: state.pipeline,
            pending,
            native,
            producer_closed: state.producer_closed
                || native.is_some_and(|native| native.producer_closed),
            custody: serving.custody.pending_for(self.id),
            snapshot_readers,
            pipeline_failed: state.pipeline_failed,
            native_failed: state.native_failed,
            joining: self.waiting.load(Ordering::Acquire),
        }
    }

    pub async fn join_forced(&self, mut external: tokio::sync::watch::Receiver<bool>) {
        let Ok(waiter) = self.waiter() else {
            return;
        };
        let outcome = std::future::poll_fn(|cx| self.poll_build(cx)).await;
        let Some(components) = outcome.components else {
            return;
        };
        self.force_components();
        let native = components
            .assembled
            .continuation_registry
            .join_forced_owned();
        let pipeline = async {
            if external.wait_for(|joined| *joined).await.is_err() {
                return None;
            }
            // This also applies sticky force to a build completed after the
            // manager's initial pipeline broadcast.
            self.request_pipeline_force();
            let tasks = components.tasks.as_ref()?;
            if tasks.force().is_err() {
                return None;
            }
            tasks.join_forced().await.ok()
        };
        let (native, pipeline) = tokio::join!(native, pipeline);
        {
            let mut state = self
                .retirement
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match native {
                Ok(drain) => state.native = Some(drain),
                Err(_) => state.native_failed = true,
            }
            if let Some(summary) = pipeline {
                state.pipeline = Some(summary);
            } else {
                state.pipeline_failed = true;
            }
        }
        drop(waiter);
        // Counts are deliberately read again by the manager after every child
        // joins: pipeline destruction may have added a native marker meanwhile.
    }
}
