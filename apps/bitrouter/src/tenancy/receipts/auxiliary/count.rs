//! Two serial, execution-owned input measurements. No model attempt is minted.
use super::{Journal, Mode};
use bitrouter_ai::tenancy::{configuration::ResourceRef, operation::ErrorCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    pub credential: ResourceRef,
    pub candidate: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Fact {
    pub ordinal: u8,
    pub work: String,
    pub credential: ResourceRef,
    pub candidate: String,
    pub prompt: String,
    pub generation_request: Option<String>,
    pub count_request: Option<String>,
    pub http_status: Option<u16>,
    pub outcome: Outcome,
    pub input_tokens: Option<u64>,
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::tenancy) enum Outcome {
    NotSent,
    Counted,
    Rejected,
    Unknown,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Envelope {
    pub version: u8,
    pub counts: Vec<Fact>,
}
impl Envelope {
    pub fn validate(&self) -> bool {
        if self.version != 2 || self.counts.is_empty() || self.counts.len() > 2 {
            return false;
        }
        for (index, fact) in self.counts.iter().enumerate() {
            if usize::from(fact.ordinal) != index
                || uuid::Uuid::parse_str(&fact.work).is_err()
                || !digest(&fact.candidate)
                || !digest(&fact.prompt)
                || fact
                    .generation_request
                    .as_deref()
                    .is_some_and(|s| !digest(s))
                || fact.count_request.as_deref().is_some_and(|s| !digest(s))
                || fact.http_status.is_some_and(|s| !(200..=599).contains(&s))
                || serde_json::to_vec(fact).map_or(true, |b| b.len() > 4096)
            {
                return false;
            }
            let armed = fact.generation_request.is_some() && fact.count_request.is_some();
            let coherent = match fact.outcome {
                Outcome::NotSent => {
                    !armed
                        && fact.generation_request.is_none()
                        && fact.count_request.is_none()
                        && fact.http_status.is_none()
                        && fact.input_tokens.is_none()
                }
                Outcome::Counted => {
                    armed
                        && fact.http_status.is_some_and(|s| (200..=299).contains(&s))
                        && fact.input_tokens.is_some()
                }
                Outcome::Rejected => {
                    armed
                        && fact.http_status.is_some_and(|s| !(200..=299).contains(&s))
                        && fact.input_tokens.is_none()
                }
                Outcome::Unknown => {
                    armed
                        && fact.http_status.is_none_or(|s| (200..=299).contains(&s))
                        && fact.input_tokens.is_none()
                }
            };
            if !coherent {
                return false;
            }
            if let Some(previous) = index.checked_sub(1).and_then(|i| self.counts.get(i))
                && (previous.outcome != Outcome::Counted
                    || previous.work == fact.work
                    || previous.prompt == fact.prompt
                    || previous.credential != fact.credential
                    || previous.candidate != fact.candidate)
            {
                return false;
            }
        }
        true
    }
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
enum Cell {
    Reserved(Fact),
    Armed(Fact),
    Settled(Fact),
}
pub(super) struct State {
    identity: Identity,
    cells: Vec<Cell>,
    rebuilt: Option<String>,
}
impl State {
    pub(super) fn settled(&self) -> bool {
        self.cells
            .iter()
            .all(|cell| matches!(cell, Cell::Settled(_)))
    }
    pub(super) fn snapshot(&self) -> Result<Envelope, ErrorCode> {
        let counts = self
            .cells
            .iter()
            .map(|cell| match cell {
                Cell::Settled(fact) => Ok(fact.clone()),
                _ => Err(ErrorCode::StorageUnavailable),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Envelope { version: 2, counts })
    }
}
/// A capability created only by the owned execution's selected Count profile.
/// Drop resolves its actual cell synchronously, retaining known HTTP rejection.
pub(crate) struct Permit {
    journal: Arc<Journal>,
    identity: Identity,
    ordinal: usize,
    work: String,
    settled: bool,
}
impl Journal {
    pub(in crate::tenancy) fn begin_counts(&self, identity: Identity) -> Result<(), ErrorCode> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        if state.closed || !matches!(state.mode, Mode::Empty) || !digest(&identity.candidate) {
            return Err(ErrorCode::Forbidden);
        }
        state.mode = Mode::Counts(State {
            identity,
            cells: Vec::with_capacity(2),
            rebuilt: None,
        });
        Ok(())
    }
    pub(in crate::tenancy) fn count_identity(&self) -> Result<Option<Identity>, ErrorCode> {
        let state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        Ok(match &state.mode {
            Mode::Counts(counts) => Some(counts.identity.clone()),
            _ => None,
        })
    }
    pub(in crate::tenancy) fn allow_count_rebuild(
        &self,
        prompt: [u8; 32],
    ) -> Result<(), ErrorCode> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        if state.closed {
            return Err(ErrorCode::Forbidden);
        }
        let Mode::Counts(counts) = &mut state.mode else {
            return Err(ErrorCode::Forbidden);
        };
        let [Cell::Settled(first)] = counts.cells.as_slice() else {
            return Err(ErrorCode::Forbidden);
        };
        let prompt = hex::encode(prompt);
        if first.outcome != Outcome::Counted || first.prompt == prompt || counts.rebuilt.is_some() {
            return Err(ErrorCode::Forbidden);
        }
        counts.rebuilt = Some(prompt);
        Ok(())
    }
    pub(in crate::tenancy) fn claim_count(
        self: &Arc<Self>,
        prompt: [u8; 32],
    ) -> Result<Permit, ErrorCode> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        if state.closed {
            return Err(ErrorCode::Forbidden);
        }
        let Mode::Counts(counts) = &mut state.mode else {
            return Err(ErrorCode::Forbidden);
        };
        let prompt = hex::encode(prompt);
        if !counts.cells.is_empty()
            && (counts.cells.len() != 1
                || !counts.settled()
                || counts.rebuilt.as_ref() != Some(&prompt))
        {
            return Err(ErrorCode::Forbidden);
        }
        let ordinal = counts.cells.len();
        let fact = Fact {
            ordinal: u8::try_from(ordinal).map_err(|_| ErrorCode::CapacityExceeded)?,
            work: uuid::Uuid::new_v4().to_string(),
            credential: counts.identity.credential.clone(),
            candidate: counts.identity.candidate.clone(),
            prompt,
            generation_request: None,
            count_request: None,
            http_status: None,
            outcome: Outcome::NotSent,
            input_tokens: None,
        };
        if serde_json::to_vec(&fact).map_or(true, |bytes| bytes.len() > 4096) {
            return Err(ErrorCode::CapacityExceeded);
        }
        let permit = Permit {
            journal: self.clone(),
            identity: counts.identity.clone(),
            ordinal,
            work: fact.work.clone(),
            settled: false,
        };
        counts.rebuilt = None;
        counts.cells.push(Cell::Reserved(fact));
        Ok(permit)
    }
    pub(in crate::tenancy) fn verify_count(
        &self,
        identity: &Identity,
        generation: &str,
    ) -> Result<(), ErrorCode> {
        let state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        let Mode::Counts(counts) = &state.mode else {
            return Err(ErrorCode::Forbidden);
        };
        let Some(Cell::Settled(fact)) = counts.cells.last() else {
            return Err(ErrorCode::Forbidden);
        };
        if state.closed
            || counts.identity != *identity
            || counts.rebuilt.is_some()
            || fact.outcome != Outcome::Counted
            || fact.generation_request.as_deref() != Some(generation)
        {
            return Err(ErrorCode::Forbidden);
        }
        Ok(())
    }
}
impl Permit {
    pub fn identity(&self) -> &Identity {
        &self.identity
    }
    pub(in crate::tenancy) fn belongs_to(&self, journal: &Arc<Journal>) -> bool {
        Arc::ptr_eq(&self.journal, journal)
    }
    pub fn arm(&mut self, generation: String, request: [u8; 32]) -> Result<(), ErrorCode> {
        if !digest(&generation) {
            return Err(ErrorCode::InvalidRequest);
        }
        let mut state = self
            .journal
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        if state.closed {
            return Err(ErrorCode::Forbidden);
        }
        let Mode::Counts(counts) = &mut state.mode else {
            return Err(ErrorCode::Forbidden);
        };
        let cell = counts
            .cells
            .get_mut(self.ordinal)
            .ok_or(ErrorCode::Forbidden)?;
        let Cell::Reserved(fact) = cell else {
            return Err(ErrorCode::Forbidden);
        };
        if fact.work != self.work {
            return Err(ErrorCode::Forbidden);
        }
        let mut fact = fact.clone();
        fact.generation_request = Some(generation);
        fact.count_request = Some(hex::encode(request));
        fact.outcome = Outcome::Unknown;
        *cell = Cell::Armed(fact);
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
        let Mode::Counts(counts) = &mut state.mode else {
            return Err(ErrorCode::Forbidden);
        };
        let Some(Cell::Armed(fact)) = counts.cells.get_mut(self.ordinal) else {
            return Err(ErrorCode::Forbidden);
        };
        if fact.work != self.work {
            return Err(ErrorCode::Forbidden);
        }
        if fact.http_status.is_some_and(|prior| prior != status) {
            return Err(ErrorCode::Forbidden);
        }
        fact.http_status = Some(status);
        if !(200..=299).contains(&status) {
            fact.outcome = Outcome::Rejected;
        }
        Ok(())
    }
    pub fn counted(mut self, tokens: u64) -> Result<(), ErrorCode> {
        self.settle(Some(tokens))?;
        self.settled = true;
        Ok(())
    }
    fn settle(&self, tokens: Option<u64>) -> Result<(), ErrorCode> {
        let mut state = self
            .journal
            .state
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        let Mode::Counts(counts) = &mut state.mode else {
            return Err(ErrorCode::Forbidden);
        };
        let cell = counts
            .cells
            .get_mut(self.ordinal)
            .ok_or(ErrorCode::Forbidden)?;
        let mut fact = match cell {
            Cell::Reserved(fact) | Cell::Armed(fact) if fact.work == self.work => fact.clone(),
            _ => return Err(ErrorCode::Forbidden),
        };
        if let Some(tokens) = tokens {
            if !matches!(cell, Cell::Armed(_))
                || fact.outcome != Outcome::Unknown
                || !fact.http_status.is_some_and(|s| (200..=299).contains(&s))
            {
                return Err(ErrorCode::Forbidden);
            }
            fact.outcome = Outcome::Counted;
            fact.input_tokens = Some(tokens);
        }
        *cell = Cell::Settled(fact);
        drop(state);
        self.journal.changed.notify_waiters();
        Ok(())
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if !self.settled && self.settle(None).is_err() {
            self.journal.changed.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests;
