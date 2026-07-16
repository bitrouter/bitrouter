use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReliabilityKey {
    pub provider: String,
    pub model: String,
    pub credential_class: String,
    pub endpoint_scope: String,
    pub protocol: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReliabilityObservation {
    Success,
    TransientFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutePermit {
    Closed,
    HalfOpenProbe,
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitPhase {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReliabilitySnapshot {
    pub phase: CircuitPhase,
    pub window_size: usize,
    pub failure_count: usize,
    pub consecutive_failures: u32,
}

pub struct ProviderReliabilityLedger {
    state: Mutex<ReliabilityState>,
    window_size: usize,
    consecutive_failure_threshold: u32,
    error_rate_percent: u32,
    cooldown_secs: u64,
}

#[derive(Default)]
struct ReliabilityState {
    routes: HashMap<String, CircuitEntry>,
    endpoints: HashMap<ReliabilityKey, CircuitEntry>,
}

struct CircuitEntry {
    outcomes: VecDeque<bool>,
    consecutive_failures: u32,
    phase: EntryPhase,
}

#[derive(Clone, Copy)]
enum EntryPhase {
    Closed,
    Open { opened_at: u64 },
    HalfOpen,
}

impl Default for CircuitEntry {
    fn default() -> Self {
        Self {
            outcomes: VecDeque::new(),
            consecutive_failures: 0,
            phase: EntryPhase::Closed,
        }
    }
}

impl ProviderReliabilityLedger {
    pub fn new(
        window_size: usize,
        consecutive_failure_threshold: u32,
        error_rate_percent: u32,
        cooldown_secs: u64,
    ) -> Self {
        assert!(window_size > 0, "reliability window must be positive");
        assert!(
            consecutive_failure_threshold > 0,
            "consecutive failure threshold must be positive"
        );
        assert!(
            (1..=100).contains(&error_rate_percent),
            "error-rate threshold must be between 1 and 100"
        );
        Self {
            state: Mutex::new(ReliabilityState::default()),
            window_size,
            consecutive_failure_threshold,
            error_rate_percent,
            cooldown_secs,
        }
    }

    pub fn observe(
        &self,
        route_key: &str,
        endpoint_key: ReliabilityKey,
        observation: ReliabilityObservation,
        now_unix: u64,
    ) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let route = state.routes.entry(route_key.to_string()).or_default();
        let route_was_half_open = matches!(route.phase, EntryPhase::HalfOpen);
        self.apply_observation(route, observation, now_unix);
        let endpoint = state.endpoints.entry(endpoint_key).or_default();
        if route_was_half_open {
            endpoint.phase = EntryPhase::HalfOpen;
        }
        self.apply_observation(endpoint, observation, now_unix);
    }

    pub fn permit(&self, route_key: &str, now_unix: u64) -> RoutePermit {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = state.routes.get_mut(route_key) else {
            return RoutePermit::Closed;
        };
        match entry.phase {
            EntryPhase::Closed => RoutePermit::Closed,
            EntryPhase::HalfOpen => RoutePermit::Open,
            EntryPhase::Open { opened_at }
                if now_unix.saturating_sub(opened_at) >= self.cooldown_secs =>
            {
                entry.phase = EntryPhase::HalfOpen;
                RoutePermit::HalfOpenProbe
            }
            EntryPhase::Open { .. } => RoutePermit::Open,
        }
    }

    pub fn route_snapshot(&self, route_key: &str) -> Option<ReliabilitySnapshot> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.routes.get(route_key).map(snapshot)
    }

    pub fn endpoint_snapshot(&self, endpoint_key: &ReliabilityKey) -> Option<ReliabilitySnapshot> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.endpoints.get(endpoint_key).map(snapshot)
    }

    fn apply_observation(
        &self,
        entry: &mut CircuitEntry,
        observation: ReliabilityObservation,
        now_unix: u64,
    ) {
        if matches!(entry.phase, EntryPhase::HalfOpen)
            && observation == ReliabilityObservation::Success
        {
            entry.outcomes.clear();
            entry.consecutive_failures = 0;
            entry.phase = EntryPhase::Closed;
        }

        let failed = observation == ReliabilityObservation::TransientFailure;
        entry.outcomes.push_back(failed);
        if entry.outcomes.len() > self.window_size {
            entry.outcomes.pop_front();
        }
        if failed {
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        } else {
            entry.consecutive_failures = 0;
        }

        if matches!(entry.phase, EntryPhase::HalfOpen) && failed {
            entry.phase = EntryPhase::Open {
                opened_at: now_unix,
            };
            return;
        }
        if matches!(entry.phase, EntryPhase::Open { .. }) {
            return;
        }

        let failures = entry.outcomes.iter().filter(|failed| **failed).count();
        let rolling_threshold_reached = entry.outcomes.len() == self.window_size
            && failures * 100 >= self.error_rate_percent as usize * self.window_size;
        if entry.consecutive_failures >= self.consecutive_failure_threshold
            || rolling_threshold_reached
        {
            entry.phase = EntryPhase::Open {
                opened_at: now_unix,
            };
        }
    }
}

fn snapshot(entry: &CircuitEntry) -> ReliabilitySnapshot {
    ReliabilitySnapshot {
        phase: match entry.phase {
            EntryPhase::Closed => CircuitPhase::Closed,
            EntryPhase::Open { .. } => CircuitPhase::Open,
            EntryPhase::HalfOpen => CircuitPhase::HalfOpen,
        },
        window_size: entry.outcomes.len(),
        failure_count: entry.outcomes.iter().filter(|failed| **failed).count(),
        consecutive_failures: entry.consecutive_failures,
    }
}
