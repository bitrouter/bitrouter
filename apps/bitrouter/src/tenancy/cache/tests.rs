use std::{
    mem::{align_of, size_of},
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};

use super::{Cache, CacheError, Entry};

fn budget<K, V>(entries: usize, heap_bytes: usize) -> usize {
    // Independently spell out the documented accounting, including padding for
    // over-aligned values. Fixtures below use small, known-safe arithmetic.
    let per_value = size_of::<V>()
        + 4 * size_of::<usize>()
        + 2 * (align_of::<V>().max(align_of::<usize>()) - 1);
    size_of::<Cache<K, V>>() + entries * size_of::<Entry<K, V>>() + entries * per_value + heap_bytes
}

#[test]
fn hits_refresh_lru_and_misses_do_not_evict() -> Result<()> {
    let cache = Cache::<u8, u8>::new(2, budget::<u8, u8>(2, 0))?;
    drop(cache.insert(1, 11, 0, 0)?);
    drop(cache.insert(2, 22, 0, 0)?);
    ensure!(cache.get(&9)?.is_none());
    ensure!(*cache.get(&1)?.context("missing first entry")? == 11);
    drop(cache.insert(3, 33, 0, 0)?);
    ensure!(cache.get(&2)?.is_none());
    ensure!(*cache.get(&1)?.context("recent entry was evicted")? == 11);
    ensure!(*cache.get(&3)?.context("new entry is missing")? == 33);
    Ok(())
}

#[test]
fn pins_and_clones_keep_old_revisions_resident() -> Result<()> {
    type Key = (&'static str, &'static str, &'static str);
    let cache = Cache::<Key, u8>::new(2, budget::<Key, u8>(2, 0))?;
    let old = ("installation", "tenant-a", "revision-1");
    let newer = ("installation", "tenant-a", "revision-2");
    let other_tenant = ("installation", "tenant-b", "revision-1");
    let old_pin = cache.insert(old, 1, 0, 0)?;
    let surviving_clone = old_pin.clone();
    drop(old_pin);
    drop(cache.insert(newer, 2, 0, 0)?);
    let other_pin = cache.insert(other_tenant, 3, 0, 0)?;
    ensure!(cache.get(&newer)?.is_none());
    ensure!(
        *cache
            .get(&old)?
            .context("pinned old revision was evicted")?
            == 1
    );
    ensure!(*other_pin == 3 && *surviving_clone == 1);
    ensure!(
        cache.insert(newer, 2, 0, 0).err() == Some(CacheError::Capacity),
        "all pinned entries must reject immediately"
    );
    drop(surviving_clone);
    drop(cache.insert(newer, 2, 0, 0)?);
    ensure!(cache.get(&old)?.is_none());
    ensure!(
        *cache
            .get(&other_tenant)?
            .context("other tenant pin was evicted")?
            == 3
    );
    ensure!(
        cache
            .get(&("other-installation", "tenant-b", "revision-1"))?
            .is_none()
    );
    Ok(())
}

#[test]
fn failed_capacity_preflight_preserves_entries_bytes_and_recency() -> Result<()> {
    let cache = Cache::<u8, Vec<u8>>::new(3, budget::<u8, Vec<u8>>(3, 30))?;
    drop(cache.insert(1, vec![1; 10], 10, 0)?);
    let second = cache.insert(2, vec![2; 10], 10, 0)?;
    let third = cache.insert(3, vec![3; 10], 10, 0)?;
    let before = cache
        .state
        .lock()
        .map_err(|_| CacheError::Unavailable)?
        .bytes;
    // Entry 1 is evictable, but does not free enough bytes. It must survive.
    ensure!(cache.insert(4, vec![4; 20], 20, 0).err() == Some(CacheError::Capacity));
    {
        let state = cache.state.lock().map_err(|_| CacheError::Unavailable)?;
        ensure!(state.bytes == before);
        ensure!(state.entries.iter().map(|entry| entry.key).eq([1, 2, 3]));
    }
    drop(second);
    drop(third);
    drop(cache.insert(4, vec![4; 20], 20, 0)?);
    ensure!(cache.get(&1)?.is_none() && cache.get(&2)?.is_none());
    ensure!(cache.get(&3)?.is_some() && cache.get(&4)?.is_some());
    Ok(())
}

#[repr(align(128))]
struct Aligned([u8; 128]);

#[test]
fn byte_cap_counts_reserved_key_heap_value_heap_metadata_and_alignment() -> Result<()> {
    let mut key = String::with_capacity(96);
    key.push('k');
    let key_heap = key.capacity();
    let mut value = Vec::<Aligned>::with_capacity(2);
    value.push(Aligned([7; 128]));
    let value_heap = value.capacity() * size_of::<Aligned>();
    let cap = budget::<String, Vec<Aligned>>(1, key_heap + value_heap);
    let too_small = Cache::new(1, cap - 1)?;
    ensure!(too_small.insert(key, value, value_heap, key_heap).err() == Some(CacheError::Capacity));
    let mut key = String::with_capacity(96);
    key.push('k');
    let key_heap = key.capacity();
    let mut value = Vec::<Aligned>::with_capacity(2);
    value.push(Aligned([7; 128]));
    let value_heap = value.capacity() * size_of::<Aligned>();
    let cache = Cache::new(1, budget::<String, Vec<Aligned>>(1, key_heap + value_heap))?;
    let pin = cache.insert(key, value, value_heap, key_heap)?;
    ensure!(pin[0].0[0] == 7);
    let state = cache.state.lock().map_err(|_| CacheError::Unavailable)?;
    ensure!(state.bytes == cache.max_bytes);
    ensure!(state.entries.capacity() == 1);
    drop(state);

    // The Arc value itself, not just a value's heap contents, can be over-aligned.
    let cap = budget::<u8, Aligned>(1, 0);
    let aligned = Cache::<u8, Aligned>::new(1, cap)?;
    ensure!(aligned.insert(1, Aligned([9; 128]), 0, 0)?.0[0] == 9);
    let small = Cache::<u8, Aligned>::new(1, cap - 1)?;
    ensure!(small.insert(1, Aligned([9; 128]), 0, 0).err() == Some(CacheError::Capacity));
    Ok(())
}

struct Probe {
    label: u8,
    drops: Arc<AtomicUsize>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn concurrent_duplicate_loads_choose_one_winner_without_replacing_or_double_charging() -> Result<()>
{
    let cache = Arc::new(Cache::<u8, Probe>::new(1, budget::<u8, Probe>(1, 64))?);
    let barrier = Arc::new(Barrier::new(2));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();
    for label in [1, 2] {
        let cache = Arc::clone(&cache);
        let barrier = Arc::clone(&barrier);
        let drops = Arc::clone(&drops);
        workers.push(std::thread::spawn(move || -> Result<_> {
            let initial = cache.get(&1);
            let candidate = Probe { label, drops };
            barrier.wait();
            ensure!(initial?.is_none());
            Ok(cache.insert(1, candidate, 64, 0)?)
        }));
    }
    let mut winners = Vec::new();
    for worker in workers {
        winners.push(
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("duplicate worker failed"))??,
        );
    }
    ensure!(winners.len() == 2 && winners[0].label == winners[1].label);
    ensure!(std::ptr::eq(&*winners[0], &*winners[1]));
    ensure!(drops.load(Ordering::SeqCst) == 1);
    {
        let state = cache.state.lock().map_err(|_| CacheError::Unavailable)?;
        ensure!(state.entries.len() == 1 && state.bytes == cache.max_bytes);
    }
    drop(winners);
    drop(cache);
    ensure!(drops.load(Ordering::SeqCst) == 2);
    Ok(())
}

#[test]
fn duplicate_refreshes_recency_without_accepting_oversize_candidates() -> Result<()> {
    let cache = Cache::<u8, u8>::new(2, budget::<u8, u8>(2, 0))?;
    drop(cache.insert(1, 11, 0, 0)?);
    drop(cache.insert(2, 22, 0, 0)?);
    ensure!(cache.insert(1, 99, usize::MAX, 0).err() == Some(CacheError::Capacity));
    {
        let state = cache.state.lock().map_err(|_| CacheError::Unavailable)?;
        ensure!(state.entries.iter().map(|entry| entry.key).eq([1, 2]));
    }
    ensure!(*cache.insert(1, 99, 0, 0)? == 11);
    drop(cache.insert(3, 33, 0, 0)?);
    ensure!(cache.get(&2)?.is_none());
    ensure!(
        *cache
            .get(&1)?
            .context("duplicate did not refresh recency")?
            == 11
    );
    Ok(())
}

#[test]
fn zero_caps_overflow_and_metadata_only_budgets_fail_without_eviction() -> Result<()> {
    ensure!(Cache::<u8, u8>::new(0, usize::MAX).err() == Some(CacheError::Capacity));
    ensure!(Cache::<u8, u8>::new(1, 0).err() == Some(CacheError::Capacity));
    ensure!(Cache::<u8, u8>::new(usize::MAX, usize::MAX).err() == Some(CacheError::Capacity));
    let fixed = size_of::<Cache<(), ()>>() + size_of::<Entry<(), ()>>();
    let empty = Cache::new(1, fixed)?;
    ensure!(empty.insert((), (), 0, 0).err() == Some(CacheError::Capacity));

    let cache = Cache::<u8, u8>::new(1, budget::<u8, u8>(1, 0))?;
    drop(cache.insert(1, 1, 0, 0)?);
    for (heap, key_heap) in [
        (usize::MAX, 0),
        (0, usize::MAX),
        (usize::MAX / 2, usize::MAX),
    ] {
        ensure!(cache.insert(2, 2, heap, key_heap).err() == Some(CacheError::Capacity));
    }
    ensure!(
        *cache
            .get(&1)?
            .context("overflow evicted the existing value")?
            == 1
    );
    Ok(())
}

#[test]
fn all_pinned_capacity_has_no_release_waiter() -> Result<()> {
    let cache = Arc::new(Cache::<u8, u8>::new(1, budget::<u8, u8>(1, 0))?);
    let pin = cache.insert(1, 1, 0, 0)?;
    let (send, receive) = mpsc::sync_channel(1);
    let worker_cache = Arc::clone(&cache);
    let worker = std::thread::spawn(move || send.send(worker_cache.insert(2, 2, 0, 0).err()));
    let result = receive.recv_timeout(Duration::from_secs(2));
    // Release only after observing whether the operation tried to wait for it.
    drop(pin);
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("capacity worker failed"))??;
    ensure!(result.context("cache waited for a pin release")? == Some(CacheError::Capacity));
    drop(cache.insert(2, 2, 0, 0)?);
    ensure!(cache.get(&1)?.is_none());
    Ok(())
}

#[cfg(panic = "unwind")]
#[test]
fn poisoned_cache_fails_closed_even_for_misses_and_duplicates() -> Result<()> {
    let cache = Cache::<u8, u8>::new(1, budget::<u8, u8>(1, 0))?;
    let pin = cache.insert(1, 1, 0, 0)?;
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        let _guard = cache.state.lock().map_err(|_| CacheError::Unavailable)?;
        std::panic::resume_unwind(Box::new("cache poison fixture"));
    }));
    ensure!(poisoned.is_err());
    ensure!(cache.get(&1).err() == Some(CacheError::Unavailable));
    ensure!(cache.get(&2).err() == Some(CacheError::Unavailable));
    ensure!(cache.insert(1, 2, 0, 0).err() == Some(CacheError::Unavailable));
    ensure!(cache.insert(2, 2, usize::MAX, 0).err() == Some(CacheError::Unavailable));
    ensure!(
        *pin == 1,
        "poisoning changed an already held immutable value"
    );
    Ok(())
}
