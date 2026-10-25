//! Presence-aware, bounded projection of original provider usage, never canonical defaults.
//!
//! This does not validate a provider terminal or authorize a send. Callers must
//! supply the original usage object before conversion, redaction, or stream hooks.

use bitrouter_ai::tenancy::{
    execution::{NumericUsage, Provenance, Usage, UsageDimension},
    identity::AuthorityRevision,
};
use serde_json::Value;

#[cfg(test)]
mod tests;

const MAX_OBSERVATIONS: u16 = 1024;
const DIMENSIONS: [UsageDimension; 5] = [
    UsageDimension::InputTokens,
    UsageDimension::OutputTokens,
    UsageDimension::CacheReadTokens,
    UsageDimension::CacheWriteTokens,
    UsageDimension::ReasoningTokens,
];

/// Select from the actual outbound protocol, not an inbound alias or provider body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protocol {
    ChatCompletions,
    Responses,
    Messages,
    Decisions,
}

#[derive(Clone, Copy, Default)]
enum Counter {
    #[default]
    Absent,
    Known(u64),
    Invalid,
}

impl Counter {
    fn value(self) -> Option<u64> {
        match self {
            Self::Known(value) => Some(value),
            Self::Absent | Self::Invalid => None,
        }
    }
}

#[derive(Clone, Copy, Default)]
enum Source {
    #[default]
    None,
    Provider(Protocol),
    #[cfg(test)]
    Estimated,
    Conflict,
}

/// Fixed-size accumulator. No raw JSON, strings, arbitrary keys, or event list is retained.
#[derive(Default)]
pub(crate) struct Evidence {
    source: Source,
    counters: [Counter; 5],
    // Redundant provider consistency constraint, never a sixth usage dimension.
    total: Counter,
    observations: u16,
    incomplete: bool,
}

pub(crate) struct Projection {
    pub(crate) usage: Usage,
    pub(crate) native_usage: Vec<NumericUsage>,
}

impl Evidence {
    /// Estimates require explicit presence; never pass default-filled canonical usage.
    #[cfg(test)]
    pub(crate) fn estimated(values: [Option<u64>; 5]) -> Self {
        Self {
            source: Source::Estimated,
            counters: values.map(|value| match value {
                Some(value) if value <= AuthorityRevision::MAX => Counter::Known(value),
                Some(_) => Counter::Invalid,
                None => Counter::Absent,
            }),
            total: Counter::Absent,
            observations: 0,
            incomplete: true,
        }
    }

    /// Latch lost/unsupported evidence, e.g. a decoder stripped additive usage fields.
    pub(crate) fn mark_incomplete(&mut self) {
        self.incomplete = true;
    }

    /// Observe one raw usage object; missing fields stay missing. Repeated snapshots
    /// are idempotent. Only Messages accepts monotonic cumulative updates, matching
    /// message_start/message_delta semantics; no protocol's snapshots are summed.
    pub(crate) fn observe(&mut self, protocol: Protocol, raw: &Value) {
        if self.observations >= MAX_OBSERVATIONS {
            self.mark_incomplete();
            return;
        }
        self.observations += 1;
        self.source = match self.source {
            Source::None => Source::Provider(protocol),
            Source::Provider(previous) if previous == protocol => self.source,
            Source::Provider(_) | Source::Conflict => Source::Conflict,
            #[cfg(test)]
            Source::Estimated => Source::Conflict,
        };
        let Some(object) = raw.as_object() else {
            self.mark_incomplete();
            return;
        };
        // Inspect only fixed known paths. An oversized/unknown extension cannot
        // consume an unbounded scan or erase independently observed valid counts.
        let (input, output, input_details, output_details) = match protocol {
            Protocol::ChatCompletions => (
                "prompt_tokens",
                "completion_tokens",
                "prompt_tokens_details",
                "completion_tokens_details",
            ),
            Protocol::Responses | Protocol::Decisions => (
                "input_tokens",
                "output_tokens",
                "input_tokens_details",
                "output_tokens_details",
            ),
            Protocol::Messages => ("input_tokens", "output_tokens", "", ""),
        };
        self.field(0, object.get(input), protocol);
        self.field(1, object.get(output), protocol);
        if protocol == Protocol::Messages {
            self.field(2, object.get("cache_read_input_tokens"), protocol);
            self.field(3, object.get("cache_creation_input_tokens"), protocol);
            // Messages has no qualified original reasoning-token field. Do not
            // manufacture zero from the canonical Usage default.
            self.known_keys(
                raw,
                &[
                    "input_tokens",
                    "output_tokens",
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                ],
            );
        } else {
            self.details(
                raw.get(input_details),
                &[("cached_tokens", 2), ("cache_write_tokens", 3)],
                protocol,
            );
            self.details(
                raw.get(output_details),
                &[("reasoning_tokens", 4)],
                protocol,
            );
            self.known_keys(
                raw,
                &[input, output, "total_tokens", input_details, output_details],
            );
            self.observe_total(object.get("total_tokens"));
        }
    }

    fn observe_total(&mut self, raw: Option<&Value>) {
        if let Some(raw) = raw {
            self.total = match (number(raw), self.total) {
                (Some(value), Counter::Absent) => Counter::Known(value),
                (Some(value), Counter::Known(previous)) if value == previous => {
                    Counter::Known(value)
                }
                _ => Counter::Invalid,
            };
        }
        // Check accumulated fields even when this observation omits the total.
        // Keep relationship failure sticky without deleting original raw facts.
        if let (Some(total), Some(input), Some(output)) = (
            self.total.value(),
            self.counters[0].value(),
            self.counters[1].value(),
        ) && input.checked_add(output) != Some(total)
        {
            self.total = Counter::Invalid;
        }
        if matches!(self.total, Counter::Invalid) {
            self.mark_incomplete();
        }
    }

    fn field(&mut self, index: usize, value: Option<&Value>, protocol: Protocol) {
        let Some(value) = value else {
            return;
        };
        let Some(value) = number(value) else {
            self.counters[index] = Counter::Invalid;
            self.mark_incomplete();
            return;
        };
        self.counters[index] = match self.counters[index] {
            Counter::Absent => Counter::Known(value),
            Counter::Known(previous) if previous == value => Counter::Known(value),
            Counter::Known(previous) if protocol == Protocol::Messages && value >= previous => {
                Counter::Known(value)
            }
            Counter::Known(_) | Counter::Invalid => {
                self.incomplete = true;
                Counter::Invalid
            }
        };
    }

    fn details(&mut self, raw: Option<&Value>, fields: &[(&str, usize)], protocol: Protocol) {
        let Some(raw) = raw else {
            return;
        };
        let Some(object) = raw.as_object() else {
            for (_, index) in fields {
                self.counters[*index] = Counter::Invalid;
            }
            self.mark_incomplete();
            return;
        };
        for (name, index) in fields {
            self.field(*index, object.get(*name), protocol);
        }
        // At most two known detail keys; object length detects extra fields in O(1).
        if object.len() > fields.len()
            || object
                .keys()
                .any(|name| !fields.iter().any(|(known, _)| name.as_str() == *known))
        {
            self.mark_incomplete();
        }
    }

    fn known_keys(&mut self, raw: &Value, known: &[&str]) {
        if let Some(object) = raw.as_object()
            && (object.len() > known.len()
                || object.keys().any(|name| !known.contains(&name.as_str())))
        {
            self.mark_incomplete();
        }
    }

    pub(crate) fn project(&self) -> Projection {
        let native_usage = if matches!(self.source, Source::Provider(_)) {
            DIMENSIONS
                .iter()
                .zip(self.counters)
                .filter_map(|(name, counter)| {
                    counter
                        .value()
                        .map(|value| NumericUsage { name: *name, value })
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut values = self.counters.map(Counter::value);
        let provenance = match self.source {
            Source::Provider(_) if values.iter().any(Option::is_some) => Provenance::Provider,
            #[cfg(test)]
            Source::Estimated if values.iter().any(Option::is_some) => Provenance::Estimated,
            _ => Provenance::Unknown,
        };
        let mut complete = !self.incomplete;
        if matches!(self.source, Source::Provider(Protocol::Messages)) {
            // Anthropic input_tokens is exclusive of both cache buckets. A
            // missing bucket prevents an inclusive total; it is not implicit zero.
            values[0] = values[0].and_then(|input| {
                input
                    .checked_add(values[2]?)?
                    .checked_add(values[3]?)
                    .filter(|value| *value <= AuthorityRevision::MAX)
            });
        }
        if let Some(input) = values[0] {
            let invalid = match (values[2], values[3]) {
                (Some(read), Some(write)) => {
                    read.checked_add(write).is_none_or(|cache| cache > input)
                }
                (Some(cache), None) | (None, Some(cache)) => cache > input,
                (None, None) => false,
            };
            if invalid {
                values[0] = None;
                values[2] = None;
                values[3] = None;
                complete = false;
            }
        }
        if let (Some(output), Some(reasoning)) = (values[1], values[4])
            && reasoning > output
        {
            values[1] = None;
            values[4] = None;
            complete = false;
        }
        if matches!(self.total, Counter::Invalid) {
            values[0] = None;
            values[1] = None;
            complete = false;
        }
        if provenance == Provenance::Unknown {
            values = [None; 5];
        }
        Projection {
            usage: usage(values, provenance, complete),
            native_usage,
        }
    }
}

fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .filter(|value| *value <= AuthorityRevision::MAX)
}

fn usage(values: [Option<u64>; 5], provenance: Provenance, complete: bool) -> Usage {
    Usage {
        input_tokens: values[0],
        output_tokens: values[1],
        cache_read_tokens: values[2],
        cache_write_tokens: values[3],
        reasoning_tokens: values[4],
        provenance,
        complete: complete
            && provenance == Provenance::Provider
            && values.iter().all(Option::is_some),
    }
}

/// Totals include every attempt. Unknown dominates, then estimated. Overflow or
/// one absent dimension yields null for that total, never a known subtotal.
/// An oversized input is unknown without scanning it; the caller must also
/// reject that invalid receipt, not truncate it to the first eight attempts.
pub(crate) fn aggregate(attempts: &[bitrouter_ai::tenancy::execution::Attempt]) -> Usage {
    if attempts.is_empty()
        || attempts.len() > 8
        || attempts
            .iter()
            .any(|attempt| attempt.usage.provenance == Provenance::Unknown)
    {
        return usage([None; 5], Provenance::Unknown, false);
    }
    let mut totals = [Some(0_u64); 5];
    let mut provenance = Provenance::Provider;
    let mut complete = true;
    for attempt in attempts {
        let value = &attempt.usage;
        if value.provenance == Provenance::Estimated {
            provenance = Provenance::Estimated;
        }
        complete &= value.complete;
        for (total, next) in totals.iter_mut().zip([
            value.input_tokens,
            value.output_tokens,
            value.cache_read_tokens,
            value.cache_write_tokens,
            value.reasoning_tokens,
        ]) {
            *total = total
                .and_then(|total| total.checked_add(next?))
                .filter(|total| *total <= AuthorityRevision::MAX);
        }
    }
    usage(totals, provenance, complete)
}
