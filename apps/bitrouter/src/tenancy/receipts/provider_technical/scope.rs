//! The owning pipeline's actual freeze and original hop callback boundaries.
//! No observer plugin, receipt participant, remote query or second writer.
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
    pub(super) fn scope_snapshot(&self) -> Option<(Option<PipelineScope>, Vec<HopObservation>)> {
        let Ok(mut state) = self.2.state.try_lock() else {
            self.invalidate();
            return None;
        };
        if state.sealed || !self.healthy() {
            return None;
        }
        state.sealed = true;
        let mut hops = Vec::new();
        for hop in &state.hops {
            let Ok(value) = hop.observation.try_lock() else {
                self.invalidate();
                return None;
            };
            let Some(closing) = value.attempts.last() else {
                continue;
            };
            hops.push(HopObservation {
                profile: value.profile,
                attempt_ids: value.attempts.clone(),
                closing_attempt_id: closing.clone(),
                duration_ms: value.duration,
                end: value.end,
                partial: value.partial,
            });
        }
        Some((state.finalized.clone(), hops))
    }
}
impl Hop {
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
