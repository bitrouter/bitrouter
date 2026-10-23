//! Joint readiness evidence; no individual worker can reopen a withdrawn host.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use super::super::authority::Revision;
use super::super::engine::LeaseObservation;
use super::super::http::Serving;

struct State {
    lease: Option<Instant>,
    lease_ok: bool,
    authority: Option<(Revision, Instant)>,
    withdrawn: bool,
}

pub(super) struct Health {
    serving: Arc<Serving>,
    state: Mutex<State>,
    freshness: Duration,
}

impl Health {
    pub fn new(serving: Arc<Serving>, freshness: Duration, initial_lease: Instant) -> Arc<Self> {
        Arc::new(Self {
            serving,
            freshness,
            state: Mutex::new(State {
                lease: Some(initial_lease),
                lease_ok: false,
                authority: None,
                withdrawn: false,
            }),
        })
    }

    fn update(&self, edit: impl FnOnce(&mut State)) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        edit(&mut state);
        let now = Instant::now();
        let ready = !state.withdrawn
            && state.lease_ok
            && state.lease.is_some_and(|until| now < until)
            && state.authority.is_some_and(|(revision, checked)| {
                now < checked + self.freshness && self.serving.loaded_revision() == Some(revision)
            });
        self.serving.set_ready(ready);
    }

    pub fn lease(&self, started: Instant, observation: LeaseObservation) -> anyhow::Result<()> {
        let remaining = observation
            .expires_at_ms
            .checked_sub(observation.database_now_ms)
            .and_then(|remaining| u64::try_from(remaining).ok())
            .ok_or_else(|| anyhow::anyhow!("invalid shared lease observation"))?;
        // Use request start, not response receipt: network/lock delay must never
        // extend the server's lease with an additional round trip.
        let deadline = started + Duration::from_millis(remaining);
        self.update(|state| {
            state.lease = Some(deadline);
            state.lease_ok = observation.accepting;
        });
        Ok(())
    }

    pub fn lease_failed(&self) {
        self.update(|state| state.lease_ok = false);
    }

    pub fn authority(&self, revision: Option<Revision>) {
        self.update(|state| state.authority = revision.map(|revision| (revision, Instant::now())));
    }

    pub fn refresh(&self) {
        self.update(|_| {});
    }

    pub fn next_check(&self) -> Instant {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        [
            state.lease,
            state.authority.map(|(_, checked)| checked + self.freshness),
        ]
        .into_iter()
        .flatten()
        .filter(|deadline| *deadline > now)
        .fold(now + Duration::from_millis(100), Instant::min)
    }

    pub fn lease_deadline(&self) -> Option<Instant> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .lease
    }

    pub fn withdraw(&self) {
        self.update(|state| state.withdrawn = true);
    }
}
