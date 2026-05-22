//! Cardinality management for high-cardinality dimensions.

use std::collections::HashSet;
use std::sync::Mutex;

/// Limits cardinality of a dimension by capping unique values.
///
/// Known limitation: the `seen` set is append-only — entries are never
/// evicted. Once `cap` distinct values have been observed, every *new*
/// value buckets to `"other"` for the lifetime of the process, even if
/// earlier values are now stale (e.g. a rotated API key). For a
/// long-lived daemon with heavy key rotation this degrades over time; a
/// follow-up should replace the `HashSet` with an LRU or a periodic
/// reset. The cap still does its job — bounding metric dimension
/// cardinality — so this is a fidelity limitation, not a correctness or
/// memory-safety one.
pub struct CardinalityLimiter {
    seen: Mutex<HashSet<String>>,
    cap: usize,
}

impl CardinalityLimiter {
    /// Create a new limiter with the specified cap.
    pub fn new(cap: usize) -> Self {
        Self {
            seen: Mutex::new(HashSet::new()),
            cap,
        }
    }

    /// Cap a value - returns the value if under limit, "other" if over.
    /// Thread-safe and handles poisoned mutex gracefully.
    pub fn cap(&self, value: &str) -> String {
        let mut seen = match self.seen.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // If mutex is poisoned, log warning and recover
                tracing::warn!("Cardinality limiter mutex poisoned, recovering");
                poisoned.into_inner()
            }
        };

        // Check if value exists or can be inserted atomically
        if seen.contains(value) {
            return value.to_string();
        }

        // At capacity - bucket as "other"
        if seen.len() >= self.cap {
            return "other".to_string();
        }

        // New value under cap - remember and pass through
        seen.insert(value.to_string());
        value.to_string()
    }

    /// Get current cardinality count.
    pub fn cardinality(&self) -> usize {
        match self.seen.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Clear all seen values (useful for testing).
    #[cfg(test)]
    pub fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cardinality_capping() {
        let limiter = CardinalityLimiter::new(3);

        // First 3 values pass through
        assert_eq!(limiter.cap("key1"), "key1");
        assert_eq!(limiter.cap("key2"), "key2");
        assert_eq!(limiter.cap("key3"), "key3");
        assert_eq!(limiter.cardinality(), 3);

        // 4th unique value gets bucketed
        assert_eq!(limiter.cap("key4"), "other");
        assert_eq!(limiter.cardinality(), 3);

        // Repeated values still pass through
        assert_eq!(limiter.cap("key1"), "key1");
        assert_eq!(limiter.cap("key2"), "key2");

        // New values still get bucketed
        assert_eq!(limiter.cap("key5"), "other");
    }
}
