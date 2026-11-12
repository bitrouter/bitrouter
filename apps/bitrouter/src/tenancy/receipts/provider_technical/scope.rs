//! The owning pipeline's actual freeze and original hop callback boundaries.
//! No external observer plugin, receipt participant, remote query or second writer.
use super::{Capture, MAX_OPERATIONS};
use bitrouter_ai::tenancy::{
    identity::AttemptId,
    provider_technical_facts_v2::{HopEnd, HopObservation, HopProfile, PipelineScope},
};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;

#[derive(Default)]
struct State {
    context: Option<String>,
    finalized: Option<PipelineScope>,
    hops: Vec<Arc<Hop>>,
    arms: Vec<AttemptId>,
    first_opened: Option<Arc<Hop>>,
    selected: Option<Arc<Hop>>,
    sealed: bool,
}
#[derive(Default)]
pub(super) struct Scope {
    state: Mutex<State>,
    changed: Notify,
}
struct Observation {
    profile: HopProfile,
    attempts: Vec<AttemptId>,
    end: HopEnd,
    duration: Option<u64>,
    partial: bool,
}
pub(crate) struct Hop {
    capture: Weak<Capture>,
    observation: Mutex<Observation>,
    observer: ObserverCapture,
}
#[derive(Default)]
struct ObserverClock {
    started: Option<std::time::Instant>,
    ended: Option<(HopEnd, u64)>,
    closed: bool,
}
#[derive(Default)]
struct ObserverCapture {
    clock: Mutex<ObserverClock>,
    lost: std::sync::atomic::AtomicBool,
}
pub(super) struct Snapshot {
    pub scope: Option<PipelineScope>,
    pub hops: Vec<HopObservation>,
    pub observers: Vec<bitrouter_ai::tenancy::provider_technical_facts_v3::ObserverHop>,
}
impl Scope {
    pub(super) fn notify_lost(&self) {
        self.changed.notify_waiters();
    }
}
impl Capture {
    pub(crate) fn register_context(&self, id: &str) {
        if id.is_empty()
            || id.len() > bitrouter_ai::tenancy::provider_technical_facts::MAX_BYTES
            || id.contains('\0')
        {
            self.invalidate();
            return;
        }
        match self.2.state.try_lock() {
            Ok(mut state) if state.context.is_none() && !state.sealed => {
                state.context = Some(id.to_owned())
            }
            _ => self.invalidate(),
        }
    }
    pub(crate) fn begin_hop(self: &Arc<Self>, profile: HopProfile) -> Option<Arc<Hop>> {
        if !self.healthy() {
            return None;
        }
        let Ok(mut state) = self.2.state.try_lock() else {
            self.invalidate();
            return None;
        };
        if state.sealed || state.hops.len() >= MAX_OPERATIONS {
            self.invalidate();
            return None;
        }
        let hop = Arc::new(Hop {
            capture: Arc::downgrade(self),
            observer: ObserverCapture::default(),
            observation: Mutex::new(Observation {
                profile,
                attempts: Vec::new(),
                end: HopEnd::Unobserved,
                duration: None,
                partial: true,
            }),
        });
        state.hops.push(hop.clone());
        Some(hop)
    }
    pub(crate) fn finalize_scope(&self, mut scope: PipelineScope) {
        if !self.healthy() {
            return;
        }
        let Ok(mut state) = self.2.state.try_lock() else {
            self.invalidate();
            return;
        };
        if state.sealed {
            return;
        } // Optional writer already took its one snapshot.
        if state.context.as_deref() != Some(scope.context_request_id.as_str())
            || state.finalized.is_some()
        {
            self.invalidate();
            return;
        }
        if let Some(stream) = &mut scope.stream {
            stream.source_attempt_id = state.first_opened.as_ref().and_then(|hop| hop.closing());
            stream.terminal_attempt_id = state.selected.as_ref().and_then(|hop| hop.closing());
            stream.partial |=
                stream.source_attempt_id.is_none() || stream.terminal_attempt_id.is_none();
        }
        state.finalized = Some(scope);
        drop(state);
        self.2.changed.notify_waiters();
    }
    pub(super) async fn scope_ready(&self) {
        loop {
            let notified = self.2.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let ready = match self.2.state.try_lock() {
                Ok(state) => {
                    state.context.is_none()
                        || state.finalized.is_some()
                        || state.sealed
                        || !self.healthy()
                }
                Err(_) => {
                    self.invalidate();
                    true
                }
            };
            if ready {
                return;
            }
            notified.await;
        }
    }
    pub(super) fn scope_snapshot(&self) -> Option<Snapshot> {
        let Ok(mut state) = self.2.state.try_lock() else {
            self.invalidate();
            return None;
        };
        if state.sealed || !self.healthy() {
            return None;
        }
        state.sealed = true;
        let mut hops = Vec::new();
        let mut observers = Vec::new();
        for hop in &state.hops {
            let Ok(value) = hop.observation.try_lock() else {
                self.invalidate();
                return None;
            };
            let Some(closing) = value.attempts.last() else {
                continue;
            };
            if let Some(observed) = hop.observer_snapshot(&value) {
                observers.push(observed);
            }
            hops.push(HopObservation {
                profile: value.profile,
                attempt_ids: value.attempts.clone(),
                closing_attempt_id: closing.clone(),
                duration_ms: value.duration,
                end: value.end,
                partial: value.partial,
            });
        }
        Some(Snapshot {
            scope: state.finalized.clone(),
            hops,
            observers,
        })
    }
}
#[cfg(test)]
mod observer_tests;

impl Hop {
    pub(crate) fn belongs_to(&self, capture: &Arc<Capture>) -> bool {
        self.capture
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, capture))
    }
    pub(crate) fn lose_observer(&self) {
        self.observer
            .lost
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn observer_pending(&self) -> bool {
        self.observer
            .clock
            .try_lock()
            .map(|clock| !clock.closed)
            .unwrap_or(true)
    }
    pub(crate) fn observer_start(&self) {
        let Ok(mut value) = self.observer.clock.try_lock() else {
            self.observer
                .lost
                .store(true, std::sync::atomic::Ordering::Release);
            return;
        };
        if value.closed || value.started.is_some() {
            self.observer
                .lost
                .store(true, std::sync::atomic::Ordering::Release);
            return;
        }
        // INSIDE the same observer's start callback, after its own pairing lock.
        value.started = Some(std::time::Instant::now());
    }
    pub(crate) fn observer_end(&self, end: Option<HopEnd>) {
        let Ok(mut value) = self.observer.clock.try_lock() else {
            self.observer
                .lost
                .store(true, std::sync::atomic::Ordering::Release);
            return;
        };
        if value.closed {
            self.observer
                .lost
                .store(true, std::sync::atomic::Ordering::Release);
            return;
        }
        value.closed = true;
        let Some(started) = value.started.take() else {
            return;
        };
        let Some(end) = end else {
            return;
        }; // Opened success: notify, not open latency.
        // ONE scalar inside this observer's end callback. The original JSON
        // operation helper has exactly the legacy observer's floor/saturation.
        let duration = super::json_millis(started.elapsed());
        value.ended = Some((end, duration));
    }
    fn observer_snapshot(
        &self,
        hop: &Observation,
    ) -> Option<bitrouter_ai::tenancy::provider_technical_facts_v3::ObserverHop> {
        use bitrouter_ai::tenancy::provider_technical_facts_v3::{ObserverHop, ObserverHopProfile};
        if self
            .observer
            .lost
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        let clock = self.observer.clock.try_lock().ok()?;
        let (end, duration_ms) = clock.ended?;
        let profile = match hop.profile {
            HopProfile::CoreJsonHopCallbacksV1 => ObserverHopProfile::CoreJsonHopObserverFloorV1,
            HopProfile::CoreStreamOpenCallbacksV1 if end == HopEnd::Failed => {
                ObserverHopProfile::CoreStreamOpenHopObserverFloorV1
            }
            _ => return None,
        };
        Some(ObserverHop {
            profile,
            attempt_ids: hop.attempts.clone(),
            closing_attempt_id: hop.attempts.last()?.clone(),
            duration_ms,
            end,
        })
    }

    pub(crate) fn armed(&self, id: &AttemptId) {
        let Some(capture) = self.capture.upgrade() else {
            return;
        };
        if !capture.healthy() {
            return;
        }
        let Ok(mut state) = capture.2.state.try_lock() else {
            capture.invalidate();
            return;
        };
        if state.sealed {
            return;
        }
        if state.arms.len() >= MAX_OPERATIONS || state.arms.contains(id) {
            capture.invalidate();
            return;
        }
        match self.observation.try_lock() {
            Ok(mut value) if value.end == HopEnd::Unobserved => {
                value.attempts.push(id.clone());
                state.arms.push(id.clone());
            }
            _ => capture.invalidate(),
        }
    }
    fn closing(&self) -> Option<AttemptId> {
        match self.observation.try_lock() {
            Ok(value) => value.attempts.last().cloned(),
            Err(_) => {
                if let Some(capture) = self.capture.upgrade() {
                    capture.invalidate();
                }
                None
            }
        }
    }
    pub(crate) fn selected(self: &Arc<Self>, opened: bool) {
        let Some(capture) = self.capture.upgrade() else {
            return;
        };
        let Ok(mut state) = capture.2.state.try_lock() else {
            capture.invalidate();
            return;
        };
        if state.sealed {
            return;
        }
        let Ok(value) = self.observation.try_lock() else {
            capture.invalidate();
            return;
        };
        // Restoring the last successful target after a later failed turn cannot
        // label that failed turn as the original successful selection.
        if matches!(value.end, HopEnd::Failed | HopEnd::Dropped) {
            return;
        }
        state.selected = Some(self.clone());
        if opened && state.first_opened.is_none() {
            state.first_opened = Some(self.clone());
        }
    }
    pub(crate) fn finish(&self, profile: HopProfile, end: HopEnd, duration: u64, partial: bool) {
        let Some(capture) = self.capture.upgrade() else {
            return;
        };
        match self.observation.try_lock() {
            Ok(mut value) if value.end == HopEnd::Unobserved => {
                value.profile = profile;
                value.end = end;
                value.duration = Some(duration);
                value.partial = partial;
            }
            Ok(_) => {} // The original wrapper notifies only once; Drop cannot overwrite EOF.
            Err(_) => capture.invalidate(),
        }
    }
}
