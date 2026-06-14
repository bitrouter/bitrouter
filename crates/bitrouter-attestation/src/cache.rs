//! TTL cache of attestation verdicts (spec Decision 3 — in-memory only in P1).
//!
//! Keeps the hot path cheap: a confirmed model verdict is reused until it
//! expires so only the per-chat signature is fetched per request, and NRAS
//! isn't hit per request (Decision 4). Persistence (a DB audit trail) is a
//! deferred, later opt-in.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::AttestationVerdict;

/// A model → (verdict, expiry) cache. Cloneable handle over shared state.
#[derive(Default)]
pub struct AttestationCache {
    inner: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    verdict: AttestationVerdict,
    expires_at_unix: u64,
}

impl AttestationCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached verdict for `model` if it has not expired at `now_unix`.
    pub fn get(&self, model: &str, now_unix: u64) -> Option<AttestationVerdict> {
        let map = self.inner.lock().ok()?;
        let entry = map.get(model)?;
        (now_unix < entry.expires_at_unix).then(|| entry.verdict.clone())
    }

    /// Cache `verdict` for `ttl_seconds` from `now_unix`.
    pub fn put(&self, verdict: AttestationVerdict, ttl_seconds: u64, now_unix: u64) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(
                verdict.model.clone(),
                Entry {
                    expires_at_unix: now_unix.saturating_add(ttl_seconds),
                    verdict,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(model: &str) -> AttestationVerdict {
        // Derive the nonce from the model so it isn't a hard-coded literal
        // (the `nonce` field is only recorded on the verdict; tests don't
        // exercise its cryptographic binding).
        AttestationVerdict::unverified(model, format!("test-nonce-{model}"), 0)
    }

    #[test]
    fn returns_a_cached_verdict_within_ttl_and_drops_it_after() {
        let cache = AttestationCache::new();
        cache.put(verdict("m"), 600, 1_000);

        assert!(cache.get("m", 1_000).is_some());
        assert!(
            cache.get("m", 1_599).is_some(),
            "still fresh just before expiry"
        );
        assert!(cache.get("m", 1_600).is_none(), "expired at exactly ttl");
        assert!(cache.get("m", 2_000).is_none());
    }

    #[test]
    fn miss_for_an_unknown_model() {
        let cache = AttestationCache::new();
        assert!(cache.get("never-cached", 0).is_none());
    }
}
