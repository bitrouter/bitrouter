//! Native stream identity and the finish that awaits transport EOF.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};

pub(crate) mod json;

pub(crate) struct State {
    response_id: Option<String>,
    model_version: Option<String>,
    candidates: BTreeMap<Option<u32>, Option<String>>,
    indexed: Option<bool>,
    finished_candidates: usize,
    remaining: usize,
}

impl State {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            response_id: None,
            model_version: None,
            candidates: BTreeMap::new(),
            indexed: None,
            finished_candidates: 0,
            remaining: limit,
        }
    }

    pub(crate) fn observe(&mut self, value: &Value) -> Result<bool> {
        let object = value
            .as_object()
            .ok_or(Error::Response("native SSE data must be an object"))?;
        if object.get("error").is_some_and(|error| !error.is_null())
            || object
                .get("promptFeedback")
                .and_then(|feedback| feedback.get("blockReason"))
                .is_some_and(|reason| !reason.is_null())
        {
            return Err(Error::Response(
                "native response contains an error or prompt rejection",
            ));
        }
        let closed_before = self.finished();
        let prior_candidates = self.candidates.len();
        for (key, slot) in [
            ("responseId", &mut self.response_id),
            ("modelVersion", &mut self.model_version),
        ] {
            let Some(id) = object.get(key) else {
                continue;
            };
            let id = id
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or(Error::Response("invalid native response identity"))?;
            match slot {
                Some(original) if original.as_str() != id => {
                    return Err(Error::Response("native response identity changed"));
                }
                Some(_) => {}
                None => {
                    self.remaining = self
                        .remaining
                        .checked_sub(id.len().saturating_add(std::mem::size_of::<String>()))
                        .ok_or(Error::Limit)?;
                    *slot = Some(id.to_owned());
                }
            }
        }
        if let Some(candidates) = object.get("candidates") {
            let candidates = candidates
                .as_array()
                .ok_or(Error::Response("invalid native candidate array"))?;
            let mut indices = BTreeSet::new();
            for candidate in candidates {
                let index = match candidate.get("index") {
                    Some(index) => Some(
                        index
                            .as_u64()
                            .and_then(|index| u32::try_from(index).ok())
                            .filter(|index| *index <= i32::MAX as u32)
                            .ok_or(Error::Response("invalid native candidate index"))?,
                    ),
                    None if candidates.len() == 1 => None,
                    None => return Err(Error::Response("native candidate index is ambiguous")),
                };
                let indexed = index.is_some();
                if self.indexed.is_some_and(|prior| prior != indexed) {
                    return Err(Error::Response("native candidate identity mode changed"));
                }
                self.indexed.get_or_insert(indexed);
                if !indices.insert(index) {
                    return Err(Error::Response("duplicate native candidate index"));
                }
                self.observe_candidate(index, candidate)?;
            }
        }
        // A finish belongs to one candidate. Root metadata may still accompany
        // another open or newly introduced candidate. Fully closed candidates
        // retain the narrow trailing-usage/identity contract.
        if closed_before
            && self.candidates.len() == prior_candidates
            && object.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "responseId" | "modelVersion" | "usageMetadata" | "candidates" | "error"
                )
            })
        {
            return Err(Error::Response("unexpected native field after finish"));
        }
        // Hold from the first finish, including any other candidates still
        // streaming. No terminal-bearing event escapes before clean HTTP EOF.
        Ok(self.finished_candidates != 0)
    }

    fn observe_candidate(&mut self, index: Option<u32>, candidate: &Value) -> Result<()> {
        let candidate = candidate
            .as_object()
            .ok_or(Error::Response("invalid native candidate"))?;
        if candidate.get("error").is_some_and(|error| !error.is_null()) {
            return Err(Error::Response("native candidate contains an error"));
        }
        if !self.candidates.contains_key(&index) {
            // Charge identities across the entire stream, so an endless series
            // of new indices cannot grow retained state beyond the read budget.
            self.remaining = self
                .remaining
                .checked_sub(
                    std::mem::size_of::<(Option<u32>, Option<String>)>().saturating_add(64),
                )
                .ok_or(Error::Limit)?;
        }
        let prior_finish = self.candidates.entry(index).or_default();
        if prior_finish.is_some() && candidate.get("finishReason").is_some_and(Value::is_null) {
            return Err(Error::Response("native finish reason was cleared"));
        }
        if let Some(content) = candidate.get("content") {
            let content = content
                .as_object()
                .ok_or(Error::Response("invalid native content object"))?;
            if content.get("parts").is_some_and(|parts| !parts.is_array()) {
                return Err(Error::Response("invalid native content parts"));
            }
        }
        if prior_finish.is_some()
            && candidate
                .keys()
                .any(|key| !matches!(key.as_str(), "index" | "finishReason"))
        {
            return Err(Error::Response(
                "unexpected native candidate field after finish",
            ));
        }
        if let Some(finish) = candidate
            .get("finishReason")
            .filter(|reason| !reason.is_null())
        {
            let finish = finish
                .as_str()
                .filter(|reason| !reason.is_empty())
                .ok_or(Error::Response("invalid native finish reason"))?;
            if prior_finish
                .as_ref()
                .is_some_and(|original| original != finish)
            {
                return Err(Error::Response("native finish reason changed"));
            }
            if finish != "FINISH_REASON_UNSPECIFIED" && prior_finish.is_none() {
                self.remaining = self
                    .remaining
                    .checked_sub(finish.len())
                    .ok_or(Error::Limit)?;
                *prior_finish = Some(finish.to_owned());
                self.finished_candidates += 1;
            }
        }
        Ok(())
    }

    pub(crate) fn finished(&self) -> bool {
        !self.candidates.is_empty() && self.finished_candidates == self.candidates.len()
    }
}
