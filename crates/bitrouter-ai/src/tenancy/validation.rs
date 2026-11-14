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
    write_json(value, max, std::io::sink())
}

// Match the transport's default serde_json budget, including the enclosing
// objects supplied by each caller. Programmatic Values must be checked before
// recursive hashing or serialization, not only after encoding on the wire.
pub(super) fn json_depth(
    value: &serde_json::Value,
    parents: usize,
    message: &'static str,
) -> Result<(), ValidationError> {
    match value {
        serde_json::Value::Array(values) => {
            require(parents + 1 < 128, message)?;
            for value in values {
                json_depth(value, parents + 1, message)?;
            }
        }
        serde_json::Value::Object(values) => {
            require(parents + 1 < 128, message)?;
            for value in values.values() {
                json_depth(value, parents + 1, message)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn write_json(
    value: &impl Serialize,
    max: usize,
    output: impl std::io::Write,
) -> Result<(), ValidationError> {
    struct Bounded<W> {
        remaining: usize,
        output: W,
    }
    impl<W: std::io::Write> std::io::Write for Bounded<W> {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            let remaining = self
                .remaining
                .checked_sub(buffer.len())
                .ok_or_else(|| std::io::Error::other("contract byte limit"))?;
            self.output.write_all(buffer)?;
            self.remaining = remaining;
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.output.flush()
        }
    }
    serde_json::to_writer(
        Bounded {
            remaining: max,
            output,
        },
        value,
    )
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

#[cfg(test)]
mod tests {
    use super::{ValidationError, write_json};

    #[test]
    fn bounded_writer_counts_encoded_bytes_before_appending() -> Result<(), ValidationError> {
        let mut output = Vec::new();
        write_json(&"é", 4, &mut output)?;
        assert_eq!(output, "\"é\"".as_bytes());
        output.clear();
        assert_eq!(
            write_json(&"é", 3, &mut output),
            Err(ValidationError("serialized byte limit"))
        );
        assert!(output.len() <= 3);
        Ok(())
    }
}
