//! Private non-model effects under the existing terminal owner.
//!
//! Refresh and controlled input counting are exclusive bounded consumers.
use bitrouter_ai::tenancy::{configuration::ResourceRef, operation::ErrorCode};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub(crate) mod count;

pub(in crate::tenancy) const MAX_BYTES: usize = 16 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Fact {
    pub credential: ResourceRef,
    pub slot: String,
    pub generation: u64,
    pub work: String,
    pub request_digest: Option<String>,
    pub http_status: Option<u16>,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::tenancy) enum Outcome {
    NotSent,
    Refreshed,
    Rejected,
    Unknown,
}

enum Cell {
    Reserved(Fact),
    Armed(Fact),
    Settled(Fact),
}
enum Mode {
    Empty,
    Refresh(Cell),
    Counts(count::State),
}
struct State {
    closed: bool,
    mode: Mode,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(in crate::tenancy) enum Snapshot {
    Refresh(Vec<Fact>),
    Counts(count::Envelope),
}
impl From<Vec<Fact>> for Snapshot {
    fn from(value: Vec<Fact>) -> Self {
        Self::Refresh(value)
    }
}
impl Snapshot {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Refresh(facts) => facts.is_empty(),
            Self::Counts(evidence) => evidence.counts.is_empty(),
        }
    }
    pub fn unclean(&self) -> bool {
        match self {
            Self::Refresh(facts) => facts.iter().any(|fact| fact.outcome != Outcome::Refreshed),
            Self::Counts(evidence) => evidence
                .counts
                .iter()
                .any(|fact| fact.outcome != count::Outcome::Counted),
        }
    }
    pub fn unknown(&self) -> bool {
        match self {
            Self::Refresh(facts) => facts.iter().any(|fact| fact.outcome == Outcome::Unknown),
            Self::Counts(evidence) => evidence
                .counts
                .iter()
                .any(|fact| fact.outcome == count::Outcome::Unknown),
        }
    }
}

pub(in crate::tenancy) struct Journal {
    state: Mutex<State>,
    changed: Notify,
}

/// Not cloneable. Only the execution-owned OAuth exchange holds this token.
pub(in crate::tenancy) struct Permit {
    journal: Arc<Journal>,
    work: String,
    settled: bool,
}
impl Journal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                closed: false,
                mode: Mode::Empty,
            }),
            changed: Notify::new(),
        })
    }
    pub fn reserve(
        self: &Arc<Self>,
        credential: ResourceRef,
        slot: String,
        generation: u64,
    ) -> Result<Permit, ErrorCode> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        if state.closed || !matches!(state.mode, Mode::Empty) {
            return Err(ErrorCode::CapacityExceeded);
        }
        let work = uuid::Uuid::new_v4().to_string();
        let fact = Fact {
            credential,
            slot,
            generation,
            work: work.clone(),
            request_digest: None,
            http_status: None,
            outcome: Outcome::NotSent,
        };
        if serde_json::to_vec(&fact)
            .map_err(|_| ErrorCode::StorageUnavailable)?
            .len()
            > 4096
        {
            return Err(ErrorCode::CapacityExceeded);
        }
        state.mode = Mode::Refresh(Cell::Reserved(fact));
        Ok(Permit {
            journal: self.clone(),
            work,
            settled: false,
        })
    }
    /// Called before enqueueing H terminal work, never from inside that actor.
    pub async fn close(&self) -> Result<(), ErrorCode> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| ErrorCode::StorageUnavailable)?;
                state.closed = true;
                if match &state.mode {
                    Mode::Empty | Mode::Refresh(Cell::Settled(_)) => true,
                    Mode::Counts(counts) => counts.settled(),
                    _ => false,
                } {
                    return Ok(());
                }
            }
            notified.await;
        }
    }
    pub fn snapshot(&self) -> Result<Snapshot, ErrorCode> {
        let state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        match &state.mode {
            Mode::Empty => Ok(Snapshot::Refresh(Vec::new())),
            Mode::Refresh(Cell::Settled(fact)) => Ok(Snapshot::Refresh(vec![fact.clone()])),
            Mode::Counts(counts) => Ok(Snapshot::Counts(counts.snapshot()?)),
            _ => Err(ErrorCode::StorageUnavailable),
        }
    }
}
impl Permit {
    pub fn work(&self) -> &str {
        &self.work
    }
    pub fn arm(&mut self, request_digest: [u8; 32]) -> Result<(), ErrorCode> {
        let mut state = self
            .journal
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        let Mode::Refresh(Cell::Reserved(fact)) = &state.mode else {
            return Err(ErrorCode::Forbidden);
        };
        if fact.work != self.work {
            return Err(ErrorCode::Forbidden);
        }
        let mut fact = fact.clone();
        fact.request_digest = Some(hex::encode(request_digest));
        fact.outcome = Outcome::Unknown;
        state.mode = Mode::Refresh(Cell::Armed(fact));
        Ok(())
    }
    pub fn status(&mut self, status: u16) -> Result<(), ErrorCode> {
        if !(200..=599).contains(&status) {
            return Err(ErrorCode::InvalidRequest);
        }
        let mut state = self
            .journal
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        let Mode::Refresh(Cell::Armed(fact)) = &mut state.mode else {
            return Err(ErrorCode::Forbidden);
        };
        if fact.work != self.work {
            return Err(ErrorCode::Forbidden);
        }
        fact.http_status = Some(status);
        if !(200..=299).contains(&status) {
            fact.outcome = Outcome::Rejected;
        }
        Ok(())
    }
    pub fn finish(mut self, outcome: Outcome) -> Result<(), ErrorCode> {
        self.settle(Some(outcome))?;
        self.settled = true;
        Ok(())
    }
    fn settle(&self, outcome: Option<Outcome>) -> Result<(), ErrorCode> {
        let mut state = self
            .journal
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        let mut fact = match &state.mode {
            Mode::Refresh(Cell::Reserved(fact) | Cell::Armed(fact)) if fact.work == self.work => {
                fact.clone()
            }
            _ => return Err(ErrorCode::Forbidden),
        };
        if let Some(outcome) = outcome {
            let armed = matches!(state.mode, Mode::Refresh(Cell::Armed(_)));
            if (outcome == Outcome::NotSent) == armed {
                return Err(ErrorCode::Forbidden);
            }
            if outcome == Outcome::Refreshed
                && !fact.http_status.is_some_and(|s| (200..=299).contains(&s))
            {
                return Err(ErrorCode::Forbidden);
            }
            if outcome != Outcome::Unknown || fact.outcome != Outcome::Rejected {
                fact.outcome = outcome;
            }
        }
        state.mode = Mode::Refresh(Cell::Settled(fact));
        drop(state);
        self.journal.changed.notify_waiters();
        Ok(())
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if !self.settled && self.settle(None).is_err() {
            // Poison remains observable to the terminal owner; no task or retry.
            self.journal.changed.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests;
