//! Private reached-hop admission and best-effort observations. Operational
//! counters grant no authority and never reconstruct financial evidence.

mod health;
mod observation;
mod rate;
pub(in crate::tenancy) mod state;
#[cfg(test)]
mod tests;

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use bitrouter_ai::tenancy::{
    admission::FallbackRule, configuration::ResourceRef, identity::AttemptId, operation::ErrorCode,
};

use crate::gateway::error::BitrouterError;
use state::{HealthOutcome, Ticket};

pub(in crate::tenancy) const MAX_RUNTIME_KEYS: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Completed,
    HttpRejection,
    TransportFailure,
    Timeout,
    ProtocolInvalid,
    LocalRateLimited,
    LocalCircuitBlocked,
    RequestBudgetExhausted,
    AuthorityDenied,
    CapacityDenied,
    PreDispatchFailure,
    CallerPolicy,
    CallerCancelled,
    Unknown,
}

pub(crate) struct Hop {
    pub(crate) ordinal: Option<u16>,
    pub(crate) rank: usize,
    pub(in crate::tenancy) profile: Option<ResourceRef>,
    fallback: FallbackRule,
    started: Instant,
    inner: Mutex<Inner>,
}

struct Inner {
    ticket: Option<Ticket>,
    consumed: bool,
    attempt: Option<AttemptId>,
    status: Option<u16>,
    outcome: Option<Outcome>,
    duration: Option<Duration>,
}

impl Hop {
    pub(in crate::tenancy) fn new(
        ordinal: Option<u16>,
        rank: usize,
        profile: Option<ResourceRef>,
        fallback: FallbackRule,
        started: Instant,
    ) -> Self {
        Self {
            ordinal,
            rank,
            profile,
            fallback,
            started,
            inner: Mutex::new(Inner {
                ticket: None,
                consumed: false,
                attempt: None,
                status: None,
                outcome: None,
                duration: None,
            }),
        }
    }

    pub(in crate::tenancy) fn admitted(&self, ticket: Ticket) -> Result<(), ErrorCode> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if inner.ticket.is_some() || inner.consumed || inner.outcome.is_some() {
            return Err(ErrorCode::Forbidden);
        }
        inner.ticket = Some(ticket);
        Ok(())
    }

    /// Consumed under the final current-authority guard, immediately before
    /// journal arm. No retry can reuse a successful runtime admission.
    pub(in crate::tenancy) fn consume(&self) -> Result<(), ErrorCode> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if inner.ticket.is_none() || inner.consumed || inner.outcome.is_some() {
            return Err(ErrorCode::Forbidden);
        }
        inner.consumed = true;
        Ok(())
    }

    pub(in crate::tenancy) fn armed(&self, attempt: AttemptId) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.consumed = true;
            inner.attempt = Some(attempt);
        }
    }

    pub(crate) fn headers(&self, status: u16, retry_after: Option<u64>) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.status.is_some_and(|previous| previous != status) {
                return;
            }
            inner.status = Some(status);
        }
        if status >= 300 {
            let health = match status {
                401..=404 => HealthOutcome::Persistent,
                408 | 500..=599 => HealthOutcome::Transient { retry_after: 0 },
                429 => HealthOutcome::Transient {
                    retry_after: retry_after.unwrap_or(0),
                },
                _ => HealthOutcome::Neutral,
            };
            self.finish(Outcome::HttpRejection, health, false);
        }
    }

    pub(crate) fn terminal(&self, successful: bool) {
        self.finish(
            if successful {
                Outcome::Completed
            } else {
                Outcome::ProtocolInvalid
            },
            if successful {
                HealthOutcome::Success
            } else {
                HealthOutcome::Transient { retry_after: 0 }
            },
            true,
        );
    }

    pub(crate) fn failed(&self, error: &BitrouterError, cancelled: bool) {
        let sent = self.inner.lock().is_ok_and(|inner| inner.attempt.is_some());
        let (outcome, health) = if cancelled {
            (Outcome::CallerCancelled, HealthOutcome::Neutral)
        } else if !sent {
            (Outcome::PreDispatchFailure, HealthOutcome::Neutral)
        } else {
            match error {
                BitrouterError::UpstreamTimeout => (
                    Outcome::Timeout,
                    HealthOutcome::Transient { retry_after: 0 },
                ),
                BitrouterError::UpstreamInvalidResponse { .. }
                | BitrouterError::Upstream { .. } => (
                    Outcome::ProtocolInvalid,
                    HealthOutcome::Transient { retry_after: 0 },
                ),
                BitrouterError::UpstreamUnavailable => (
                    Outcome::TransportFailure,
                    HealthOutcome::Transient { retry_after: 0 },
                ),
                BitrouterError::Forbidden(_) | BitrouterError::Incompatible { .. } => {
                    (Outcome::CallerPolicy, HealthOutcome::Neutral)
                }
                _ => (Outcome::Unknown, HealthOutcome::Neutral),
            }
        };
        self.finish(outcome, health, true);
    }

    pub(in crate::tenancy) fn refused(&self, denial: state::Denial) {
        let outcome = match denial {
            state::Denial::Rate => Outcome::LocalRateLimited,
            state::Denial::Circuit => Outcome::LocalCircuitBlocked,
            state::Denial::Capacity => Outcome::CapacityDenied,
            state::Denial::Unavailable => Outcome::AuthorityDenied,
        };
        self.finish(outcome, HealthOutcome::Neutral, true);
    }

    pub(crate) fn dropped(&self) {
        self.finish(Outcome::CallerCancelled, HealthOutcome::Neutral, true);
    }

    /// Called only at a provider transport boundary, never from a rendered
    /// gateway status or error text.
    pub(crate) fn transport_failed(&self, timeout: bool) {
        self.finish(
            if timeout {
                Outcome::Timeout
            } else {
                Outcome::TransportFailure
            },
            HealthOutcome::Transient { retry_after: 0 },
            true,
        );
    }

    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(crate) fn observation(&self) -> Option<Observation> {
        let inner = self.inner.lock().ok()?;
        Some(Observation {
            attempt: inner.attempt.clone(),
            status: inner.status,
            outcome: inner.outcome.map(Outcome::as_str),
            duration_ms: inner
                .duration
                .and_then(|duration| i64::try_from(duration.as_millis()).ok()),
        })
    }

    pub(in crate::tenancy) fn permits_fallback(&self) -> bool {
        self.inner.lock().is_ok_and(|inner| match inner.outcome {
            Some(Outcome::LocalRateLimited | Outcome::LocalCircuitBlocked) => !inner.consumed,
            Some(Outcome::HttpRejection) => {
                self.fallback == FallbackRule::OrderedTerminalRejectionsV1
                    && inner.attempt.is_some()
                    && inner.status.is_some_and(|status| status >= 300)
            }
            _ => false,
        })
    }

    pub(in crate::tenancy) fn budget_exhausted(&self) {
        self.finish(
            Outcome::RequestBudgetExhausted,
            HealthOutcome::Neutral,
            true,
        );
    }

    fn finish(&self, outcome: Outcome, health: HealthOutcome, ended: bool) {
        let ticket = if let Ok(mut inner) = self.inner.lock() {
            if ended && inner.duration.is_none() {
                inner.duration = Some(self.started.elapsed());
            }
            if inner.outcome.is_some() {
                return;
            }
            inner.outcome = Some(outcome);
            inner.ticket.take()
        } else {
            None
        };
        if let Some(ticket) = ticket {
            ticket.finish(health, Instant::now());
        }
    }
}

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
pub(crate) struct Observation {
    pub(crate) attempt: Option<AttemptId>,
    pub(crate) status: Option<u16>,
    pub(crate) outcome: Option<&'static str>,
    pub(crate) duration_ms: Option<i64>,
}

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::HttpRejection => "http_rejection",
            Self::TransportFailure => "transport_failure",
            Self::Timeout => "timeout",
            Self::ProtocolInvalid => "protocol_invalid",
            Self::LocalRateLimited => "local_rate_limited",
            Self::LocalCircuitBlocked => "local_circuit_blocked",
            Self::RequestBudgetExhausted => "request_budget_exhausted",
            Self::AuthorityDenied => "authority_denied",
            Self::CapacityDenied => "capacity_denied",
            Self::PreDispatchFailure => "pre_dispatch_failure",
            Self::CallerPolicy => "caller_policy",
            Self::CallerCancelled => "caller_cancelled",
            Self::Unknown => "unknown",
        }
    }
}
