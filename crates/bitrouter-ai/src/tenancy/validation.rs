//! Bounded validation shared by clients and service adapters.

use serde::{Deserialize, Deserializer, Serialize};

/// A contract violation. Diagnostics never include untrusted values or secrets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid tenancy contract: {0}")]
pub struct ValidationError(pub(crate) &'static str);

/// Pure shape and consistency validation; this is not an authorization check.
pub trait Validate {
    /// Check all wire bounds and local cross-field invariants.
    ///
    /// # Errors
    /// Returns a redacted error when a bound or invariant is violated.
    fn validate(&self) -> Result<(), ValidationError>;
}

pub(super) fn require(condition: bool, message: &'static str) -> Result<(), ValidationError> {
    if condition {
        Ok(())
    } else {
        Err(ValidationError(message))
    }
}

pub(super) fn text(value: &str, max: usize) -> Result<(), ValidationError> {
    require(
        !value.is_empty() && value.chars().count() <= max,
        "text length",
    )
}

pub(super) fn unique<T: Eq + std::hash::Hash>(
    values: &[T],
    min: usize,
    max: usize,
) -> Result<(), ValidationError> {
    require((min..=max).contains(&values.len()), "collection length")?;
    require(
        values
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            == values.len(),
        "duplicate item",
    )
}

pub(super) fn bytes<T: Serialize>(value: &T, max: usize) -> Result<(), ValidationError> {
    // A bounded writer avoids allocating a second full copy of evidence.
    struct Counter {
        remaining: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.remaining = self
                .remaining
                .checked_sub(buffer.len())
                .ok_or_else(|| std::io::Error::other("contract byte limit"))?;
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter { remaining: max }, value)
        .map_err(|_| ValidationError("serialized byte limit"))
}

// `deserialize_with` without `default` makes nullable fields REQUIRED. Plain
// Option<T> would silently accept an absent CAS precondition as explicit null.
pub(super) fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}
