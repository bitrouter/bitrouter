//! Bounded storage for immutable values; cache hits never establish authority.
//!
//! Accounting is an allocation-payload budget, not a process RSS measurement:
//! `size_of::<Cache<K, V>>() + slots.capacity() * size_of::<Entry<K, V>>()`
//! plus, for each resident entry,
//! `key_heap_bytes + owned_bytes + size_of::<V>() + arc_allowance::<V>()`.
//! The allowance is four machine words plus twice the maximum alignment minus
//! two bytes, covering the Arc counters and conservative leading/trailing padding.
//! Vec storage includes each inline K, Pin pointer, and entry bookkeeping, even
//! for unused slots. The Vec never grows after construction. Allocator bookkeeping,
//! caller-owned Pin handles, an outer Arc<Cache>, and caller construction buffers
//! are outside this budget.
//!
//! `owned_bytes` counts V's heap allocations only, including reserved capacity
//! and recursively retained allocations. `key_heap_bytes` counts K's heap only.
//! Both estimates must be conservative and checked by the caller; inline storage
//! is already charged here. Values and keys must not grow or change while cached.
//! Any handle sharing charged value allocations must retain a Pin, including
//! derived views: Deref cannot prevent V from exposing its own shared allocations.
//! Loaders and concurrent transient candidates require separate caller bounds.
//!
//! Entries run from least to most recently used. A live Pin keeps its entry
//! resident and charged. Under the mutex, a strong count of one proves that only
//! the cache owns the wrapper: no raw Arc/Weak can escape and gain a new pin while
//! eviction is deciding. No await, loader, or capacity-wait queue runs here.
//! K equality and K/V destruction must be bounded, nonblocking and nonreentrant;
//! eviction destroys cache-only entries under the lock before reclaiming space.
//! The owner must retain the cache for the lifetime of its execution budget.

#[cfg(test)]
mod tests;

use std::{
    fmt,
    mem::{align_of, size_of},
    ops::Deref,
    sync::{Arc, Mutex},
};

/// Capacity rejection is distinct from a poisoned cache, which fails closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CacheError {
    Capacity,
    Unavailable,
}

impl fmt::Display for CacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Capacity => "immutable cache capacity exceeded",
            Self::Unavailable => "immutable cache unavailable",
        })
    }
}

impl std::error::Error for CacheError {}

/// An immutable value lease, with no Arc/Weak conversion or public constructor.
pub(crate) struct Pin<V> {
    inner: Arc<Value<V>>,
}

struct Value<V> {
    value: V,
}

impl<V> Clone for Pin<V> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<V> Deref for Pin<V> {
    type Target = V;

    fn deref(&self) -> &V {
        &self.inner.value
    }
}

struct Entry<K, V> {
    key: K,
    value: Pin<V>,
    bytes: usize,
}

struct State<K, V> {
    entries: Vec<Entry<K, V>>,
    bytes: usize,
}

/// Fixed entry and byte caps, with an allocation-free linear LRU lookup.
pub(crate) struct Cache<K, V> {
    max_entries: usize,
    max_bytes: usize,
    fixed_bytes: usize,
    state: Mutex<State<K, V>>,
}

fn arc_allowance<V>() -> Option<usize> {
    let alignment = align_of::<V>().max(align_of::<usize>());
    size_of::<usize>()
        .checked_mul(4)?
        .checked_add(alignment.checked_sub(1)?.checked_mul(2)?)
}

impl<K: Eq, V> Cache<K, V> {
    /// Preallocate metadata for a positive, fixed entry capacity.
    ///
    /// # Errors
    /// Returns Capacity for zero entries, overflow, insufficient metadata bytes
    /// or a failed Vec reservation. A metadata-only byte budget is valid, but
    /// cannot accept any value because its per-entry charge would not fit.
    pub(crate) fn new(max_entries: usize, max_bytes: usize) -> Result<Self, CacheError> {
        if max_entries == 0 {
            return Err(CacheError::Capacity);
        }
        let metadata_bytes = |capacity: usize| {
            capacity
                .checked_mul(size_of::<Entry<K, V>>())
                .and_then(|bytes| bytes.checked_add(size_of::<Self>()))
                .filter(|bytes| *bytes <= max_bytes)
                .ok_or(CacheError::Capacity)
        };
        metadata_bytes(max_entries)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(max_entries)
            .map_err(|_| CacheError::Capacity)?;
        // Charge the reported allocation, even if reserve_exact overallocated.
        let fixed_bytes = metadata_bytes(entries.capacity())?;
        Ok(Self {
            max_entries,
            max_bytes,
            fixed_bytes,
            state: Mutex::new(State {
                entries,
                bytes: fixed_bytes,
            }),
        })
    }

    /// Pin a hit and move it to the most recently used position.
    ///
    /// # Errors
    /// A poisoned mutex returns Unavailable, including for a missing key.
    pub(crate) fn get(&self, key: &K) -> Result<Option<Pin<V>>, CacheError> {
        let mut state = self.state.lock().map_err(|_| CacheError::Unavailable)?;
        let Some(index) = state.entries.iter().position(|entry| entry.key.eq(key)) else {
            return Ok(None);
        };
        let pin = state.entries[index].value.clone();
        state.entries[index..].rotate_left(1);
        Ok(Some(pin))
    }

    /// Publish an already constructed value, or return the existing winner.
    ///
    /// The candidate's individual charge is checked even for duplicate keys.
    /// A fitting duplicate changes only recency and drops the loser after
    /// unlocking; no value/size comparison or authoritative replacement occurs.
    ///
    /// # Errors
    /// Returns Capacity for overflow, oversize, or insufficient unpinned space.
    /// Capacity rejection leaves residency, recency and accounting unchanged.
    /// A poisoned mutex returns Unavailable. There is no wait for a pin release.
    pub(crate) fn insert(
        &self,
        key: K,
        value: V,
        owned_bytes: usize,
        key_heap_bytes: usize,
    ) -> Result<Pin<V>, CacheError> {
        let mut state = self.state.lock().map_err(|_| CacheError::Unavailable)?;
        let bytes = size_of::<V>()
            .checked_add(arc_allowance::<V>().ok_or(CacheError::Capacity)?)
            .and_then(|bytes| bytes.checked_add(owned_bytes))
            .and_then(|bytes| bytes.checked_add(key_heap_bytes))
            .filter(|bytes| *bytes <= self.max_bytes - self.fixed_bytes)
            .ok_or(CacheError::Capacity)?;
        if let Some(index) = state.entries.iter().position(|entry| entry.key.eq(&key)) {
            let winner = state.entries[index].value.clone();
            state.entries[index..].rotate_left(1);
            drop(state);
            return Ok(winner);
        }

        // Prove that eviction can succeed before changing anything. Pin counts
        // can fall concurrently, but an entry observed cache-only cannot gain
        // an external owner while get/insert are excluded by this mutex.
        // https://doc.rust-lang.org/std/sync/struct.Arc.html#method.strong_count
        let fits =
            |count: usize, used: usize| count < self.max_entries && bytes <= self.max_bytes - used;
        let mut remaining_entries = state.entries.len();
        let mut remaining_bytes = state.bytes;
        for entry in &state.entries {
            if fits(remaining_entries, remaining_bytes) {
                break;
            }
            if Arc::strong_count(&entry.value.inner) == 1 {
                remaining_entries -= 1;
                remaining_bytes -= entry.bytes;
            }
        }
        if !fits(remaining_entries, remaining_bytes) {
            return Err(CacheError::Capacity);
        }
        remaining_entries = state.entries.len();
        remaining_bytes = state.bytes;
        state.entries.retain(|entry| {
            if fits(remaining_entries, remaining_bytes)
                || Arc::strong_count(&entry.value.inner) != 1
            {
                return true;
            }
            remaining_entries -= 1;
            remaining_bytes -= entry.bytes;
            false
        });
        // retain has destroyed each victim before its charge becomes available.
        state.bytes = remaining_bytes;

        let pin = Pin {
            inner: Arc::new(Value { value }),
        };
        state.entries.push(Entry {
            key,
            value: pin.clone(),
            bytes,
        });
        state.bytes += bytes;
        Ok(pin)
    }
}
