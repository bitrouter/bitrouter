//! Validation of retained input-count evidence from existing terminal receipts.
use bitrouter_ai::tenancy::{configuration::ResourceRef, operation::ErrorCode};
use serde::{Deserialize, Serialize};

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
