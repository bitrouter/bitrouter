//! Exact rolling windows, bounded by allocated timestamp storage. Capacity is
//! reclaimed only after every timestamp in a window has expired.

use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use bitrouter_ai::tenancy::identity::{InstallationId, OwnerId, ResourceId};

use super::state::Denial;

pub(super) type Key = (InstallationId, OwnerId, ResourceId);
const WINDOW: Duration = Duration::from_secs(60);
// One process's timestamp allocation budget, not a ceiling on a profile's RPM.
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[cfg_attr(test, derive(Debug))]
pub(super) struct Rates {
    windows: HashMap<Key, VecDeque<Instant>>,
    slots: usize,
    max_slots: usize,
}

impl Default for Rates {
    fn default() -> Self {
        Self {
            windows: HashMap::new(),
            slots: 0,
            max_slots: MAX_BYTES / std::mem::size_of::<Instant>(),
        }
    }
}

impl Rates {
    pub(super) fn count(&self, key: &Key, now: Instant) -> u64 {
        self.windows.get(key).map_or(0, |window| {
            let (first, second) = window.as_slices();
            let expired = |at: &Instant| now.saturating_duration_since(*at) > WINDOW;
            (window.len() - first.partition_point(expired) - second.partition_point(expired)) as u64
        })
    }

    pub(super) fn has_capacity(&self, key: &Key, now: Instant) -> bool {
        if self.windows.get(key).is_some_and(|window| {
            window.len() < window.capacity()
                || window
                    .front()
                    .is_some_and(|at| now.saturating_duration_since(*at) > WINDOW)
        }) {
            return true;
        }
        self.slots < self.max_slots
            || self.windows.values().any(|window| {
                window
                    .back()
                    .is_none_or(|at| now.saturating_duration_since(*at) > WINDOW)
            })
    }

    pub(super) fn admit(&mut self, key: Key, rpm: u32, now: Instant) -> Result<(), Denial> {
        if self.count(&key, now) >= u64::from(rpm) {
            return Err(Denial::Rate);
        }
        if self.slots == self.max_slots {
            self.windows.retain(|_, window| {
                if window
                    .back()
                    .is_none_or(|at| now.saturating_duration_since(*at) > WINDOW)
                {
                    self.slots -= window.capacity();
                    false
                } else {
                    true
                }
            });
        }
        if !self.windows.contains_key(&key) {
            if self.windows.len() >= super::MAX_RUNTIME_KEYS {
                return Err(Denial::Capacity);
            }
            self.windows.try_reserve(1).map_err(|_| Denial::Capacity)?;
        }
        let window = self.windows.entry(key).or_default();
        while window
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) > WINDOW)
        {
            window.pop_front();
        }
        if window.len() == window.capacity() {
            let spare = self.max_slots.saturating_sub(self.slots);
            if spare == 0 {
                return Err(Denial::Capacity);
            }
            let previous = window.capacity();
            window
                .try_reserve_exact(spare.min(64))
                .map_err(|_| Denial::Capacity)?;
            self.slots += window.capacity() - previous;
        }
        window.push_back(now);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn bounded(slots: usize) -> Self {
        Self {
            max_slots: slots,
            ..Self::default()
        }
    }
}
