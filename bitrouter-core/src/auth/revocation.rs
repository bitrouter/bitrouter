//! API key revocation set trait and in-memory implementation.
//!
//! Defines the interface for checking whether an API key (identified by
//! its `id` claim in the JWT) has been revoked. Implementations may be
//! backed by an in-memory set, a database, or both (in-memory cache with
//! DB persistence).

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::RwLock;

/// Trait for checking API key revocation status.
///
/// The revocation set maps `id` claim values (base64url-encoded 32-byte
/// key identifiers) to a boolean revoked/not-revoked status. The set is
/// expected to be small (number of API keys, not number of JWTs), so
/// implementations are free to keep the full set in memory.
pub trait KeyRevocationSet: Send + Sync {
    /// Returns `true` if the given key ID has been revoked.
    fn is_revoked(&self, key_id: &str) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;

    /// Add a key ID to the revocation set.
    fn revoke(&self, key_id: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// In-memory implementation of [`KeyRevocationSet`].
///
/// Suitable for development or single-instance deployments. The set is
/// lost on process restart. For persistent revocation, a DB-backed
/// implementation should be used.
pub struct InMemoryRevocationSet {
    revoked: RwLock<HashSet<String>>,
}

impl InMemoryRevocationSet {
    /// Create a new empty revocation set.
    pub fn new() -> Self {
        Self {
            revoked: RwLock::new(HashSet::new()),
        }
    }
}

impl Default for InMemoryRevocationSet {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyRevocationSet for InMemoryRevocationSet {
    fn is_revoked(&self, key_id: &str) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        let result = self
            .revoked
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(key_id);
        Box::pin(async move { result })
    }

    fn revoke(&self, key_id: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.revoked
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key_id.to_owned());
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn revocation_set_works() {
        let set = InMemoryRevocationSet::new();
        assert!(!set.is_revoked("key-1").await);

        set.revoke("key-1").await;
        assert!(set.is_revoked("key-1").await);
        assert!(!set.is_revoked("key-2").await);
    }
}
