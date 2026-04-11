//! Concurrency utilities for bitrouter.

use std::sync::{Arc, RwLock};

use crate::errors::{BitrouterError, Result};

/// A hot-swappable shared value.
///
/// Wraps `Arc<RwLock<Arc<T>>>` to provide atomic reads and writes of a
/// shared, immutable snapshot. Readers call [`load`](HotSwap::load) to
/// clone the inner `Arc<T>` without holding the lock. Writers call
/// [`store`](HotSwap::store) to atomically replace the value.
///
/// Used for hot-reload of engines that must be swapped without dropping
/// in-flight requests: each request snapshots the current `Arc<T>` and
/// works against that snapshot while the reload closure swaps in a new one.
pub struct HotSwap<T>(Arc<RwLock<Arc<T>>>);

impl<T> HotSwap<T> {
    /// Create a new hot-swappable value.
    pub fn new(value: T) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(value))))
    }

    /// Wrap an existing `Arc<T>`.
    pub fn from_arc(arc: Arc<T>) -> Self {
        Self(Arc::new(RwLock::new(arc)))
    }

    /// Snapshot the current value without holding the lock.
    ///
    /// On a poisoned lock, returns the last known good value rather than
    /// panicking — this is consistent with fail-open for reads.
    pub fn load(&self) -> Arc<T> {
        self.0
            .read()
            .map(|guard| Arc::clone(&guard))
            .unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
    }

    /// Atomically replace the inner value.
    ///
    /// Returns `Err` if the lock is poisoned.
    pub fn store(&self, value: T) -> Result<()> {
        let mut guard = self
            .0
            .write()
            .map_err(|_| BitrouterError::transport(None, "hot-swap lock poisoned"))?;
        *guard = Arc::new(value);
        Ok(())
    }
}

impl<T> Clone for HotSwap<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for HotSwap<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.read() {
            Ok(guard) => f.debug_tuple("HotSwap").field(&*guard).finish(),
            Err(_) => f.debug_tuple("HotSwap").field(&"<poisoned>").finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_load() {
        let hs = HotSwap::new(42);
        assert_eq!(*hs.load(), 42);
    }

    #[test]
    fn from_arc_and_load() {
        let arc = Arc::new("hello");
        let hs = HotSwap::from_arc(arc);
        assert_eq!(*hs.load(), "hello");
    }

    #[test]
    fn store_replaces_value() {
        let hs = HotSwap::new(1);
        assert_eq!(*hs.load(), 1);
        hs.store(2).ok();
        assert_eq!(*hs.load(), 2);
    }

    #[test]
    fn clone_shares_state() {
        let hs1 = HotSwap::new(10);
        let hs2 = hs1.clone();
        hs1.store(20).ok();
        assert_eq!(*hs2.load(), 20);
    }

    #[test]
    fn load_returns_snapshot() {
        let hs = HotSwap::new(100);
        let snapshot = hs.load();
        hs.store(200).ok();
        // Snapshot is still the old value.
        assert_eq!(*snapshot, 100);
        // New load returns updated value.
        assert_eq!(*hs.load(), 200);
    }
}
