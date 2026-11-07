//! Native wire validation without canonical part conversion.
//!
//! Candidate.index is optional, while a missing/unspecified finish is not a
//! terminal. Anonymous singleton and indexed identity modes never mix.
//! <https://github.com/googleapis/googleapis/blob/master/google/ai/generativelanguage/v1beta/generative_service.proto>

use crate::gateway::model::error::{ModelError, Result};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Anonymous,
    Index(u32),
}

#[derive(Default)]
enum Mode {
    #[default]
    Undecided,
    Anonymous,
    Indexed,
}

pub(super) struct Lifecycle {
    mode: Mode,
    response_id: Option<String>,
    model_version: Option<String>,
    candidates: BTreeMap<Key, Option<String>>,
    primary: Option<Key>,
    finished: usize,
    remaining: usize,
}

fn invalid() -> ModelError {
    ModelError::InvalidResponse {
        message: "invalid native response lifecycle".into(),
        usage: None,
    }
}

impl Lifecycle {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            mode: Mode::Undecided,
            response_id: None,
            model_version: None,
            candidates: BTreeMap::new(),
            primary: None,
            finished: 0,
            remaining: limit,
        }
    }

    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.remaining = self.remaining.checked_sub(bytes).ok_or_else(invalid)?;
        Ok(())
    }

    /// Return whether this event belongs to the held terminal group. No field
    /// is removed or synthesized, and no canonical tool call is constructed.
    pub(super) fn observe(&mut self, body: &Value) -> Result<bool> {
        let object = body.as_object().ok_or_else(invalid)?;
        for (field, old) in [
            ("responseId", &self.response_id),
            ("modelVersion", &self.model_version),
        ] {
            if let Some(value) = object.get(field) {
                let value = value
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(invalid)?;
                if old.as_deref().is_some_and(|old| old != value) {
                    return Err(invalid());
                }
            }
        }
        let introduces_candidate = object
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    let key = entry
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                        .map(Key::Index)
                        .unwrap_or(Key::Anonymous);
                    !self.candidates.contains_key(&key)
                })
            });
        let strict_tail = !self.candidates.is_empty()
            && self.finished == self.candidates.len()
            && !introduces_candidate;
        if object.get("error").is_some_and(|v| !v.is_null())
            || object
                .get("promptFeedback")
                .and_then(|v| v.get("blockReason"))
                .is_some_and(|v| !v.is_null())
            || strict_tail
                && object.iter().any(|(key, value)| {
                    !matches!(
                        key.as_str(),
                        "responseId" | "modelVersion" | "usageMetadata" | "candidates"
                    ) && !(key == "error" && value.is_null())
                })
        {
            return Err(invalid());
        }
        for field in ["responseId", "modelVersion"] {
            let slot = if field == "responseId" {
                &self.response_id
            } else {
                &self.model_version
            };
            if slot.is_none()
                && let Some(value) = object.get(field)
            {
                let value = value.as_str().ok_or_else(invalid)?;
                self.charge(value.len().saturating_add(std::mem::size_of::<String>()))?;
                if field == "responseId" {
                    self.response_id = Some(value.into());
                } else {
                    self.model_version = Some(value.into());
                }
            }
        }
        if let Some(candidates) = object.get("candidates") {
            let candidates = candidates.as_array().ok_or_else(invalid)?;
            let mut keys = BTreeSet::new();
            for candidate in candidates {
                let candidate = candidate.as_object().ok_or_else(invalid)?;
                let key = match candidate.get("index") {
                    Some(index) => Key::Index(
                        u32::try_from(
                            index
                                .as_u64()
                                .filter(|n| *n <= i32::MAX as u64)
                                .ok_or_else(invalid)?,
                        )
                        .map_err(|_| invalid())?,
                    ),
                    None if candidates.len() == 1 => Key::Anonymous,
                    None => return Err(invalid()),
                };
                match (&self.mode, key) {
                    (Mode::Undecided, Key::Anonymous) => self.mode = Mode::Anonymous,
                    (Mode::Undecided, Key::Index(_)) => self.mode = Mode::Indexed,
                    (Mode::Anonymous, Key::Anonymous) | (Mode::Indexed, Key::Index(_)) => {}
                    _ => return Err(invalid()),
                }
                if !keys.insert(key) {
                    return Err(invalid());
                }
                self.candidate(key, candidate)?;
            }
        }
        Ok(self.finished != 0)
    }

    fn candidate(&mut self, key: Key, candidate: &Map<String, Value>) -> Result<()> {
        if candidate.get("error").is_some_and(|v| !v.is_null()) {
            return Err(invalid());
        }
        if let Some(content) = candidate.get("content") {
            let content = content.as_object().ok_or_else(invalid)?;
            if let Some(parts) = content.get("parts") {
                let parts = parts.as_array().ok_or_else(invalid)?;
                if parts.iter().any(|part| !part.is_object()) {
                    return Err(invalid());
                }
            }
        }
        let finish = match candidate.get("finishReason").filter(|v| !v.is_null()) {
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(invalid)?,
            ),
            None => None,
        };
        if let Some(Some(previous)) = self.candidates.get(&key) {
            if candidate
                .keys()
                .any(|k| !matches!(k.as_str(), "index" | "finishReason"))
                || candidate.contains_key("finishReason") && finish != Some(previous.as_str())
            {
                return Err(invalid());
            }
            return Ok(());
        }
        if !self.candidates.contains_key(&key) {
            self.charge(std::mem::size_of::<(Key, Option<String>)>().saturating_add(64))?;
            self.candidates.insert(key, None);
            if self.primary.is_none() {
                self.primary = Some(key);
            }
        }
        if let Some(finish) = finish.filter(|s| *s != "FINISH_REASON_UNSPECIFIED") {
            self.charge(finish.len())?;
            self.candidates.insert(key, Some(finish.into()));
            self.finished += 1;
        }
        Ok(())
    }

    pub(super) fn primary_candidate<'a>(&self, body: &'a Value) -> Option<&'a Value> {
        body.get("candidates")?
            .as_array()?
            .iter()
            .find(|candidate| {
                let key = candidate
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|index| u32::try_from(index).ok())
                    .map(Key::Index)
                    .unwrap_or(Key::Anonymous);
                Some(key) == self.primary
            })
    }

    pub(super) fn complete(&self) -> Result<&str> {
        if self.candidates.is_empty() || self.finished != self.candidates.len() {
            return Err(invalid());
        }
        self.primary
            .and_then(|k| self.candidates.get(&k))
            .and_then(Option::as_deref)
            .ok_or_else(invalid)
    }
}
