//! Bounded process-local scheduling state. This module never decides retries.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_ENTRIES: usize = 2048;
const MAX_IN_FLIGHT: usize = 4096;
const MAX_KEY_BYTES: usize = 16 * 1024;
const SAMPLES: usize = 32;
const MIN_SAMPLES: usize = 3;
const FRESH: Duration = Duration::from_secs(300);
const OPEN: Duration = Duration::from_secs(30);
const FAILURES: u32 = 3;

/// Exact credential namespace plus concrete dispatch identity; no secret bytes.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub(crate) namespace: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) account: Option<String>,
    pub(crate) endpoint: String,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ProviderKey {
    namespace: String,
    provider: String,
    account: Option<String>,
    endpoint: String,
    pattern: String,
}

/// Limits are retained configuration, not caller request overrides.
#[derive(Clone, Default)]
pub(crate) struct Limits {
    pub(crate) provider: Option<(String, u32)>,
    pub(crate) model: Option<u32>,
}

/// A local refusal occurs before execution and never grants permission to retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    Capacity,
    RateLimited,
    CircuitOpen,
    Unavailable,
}

/// Only explicit provider completion may supply positive health/metric evidence.
#[derive(Clone, Copy)]
pub(crate) enum Outcome {
    Success { output_tokens: Option<u64> },
    Unavailable,
    Neutral,
    Unknown,
}

#[derive(Clone, Copy)]
pub(crate) enum Order {
    Retained,
    Cost,
    Latency,
    Throughput,
}

#[derive(Default)]
struct Rate {
    pending: u32,
    // At most 61 second buckets. Round expiry upwards: never admit more than
    // the configured count in any rolling 60 seconds, even at second edges.
    seconds: VecDeque<(u64, u32)>,
}

impl Rate {
    fn expire(&mut self, tick: u64) {
        while self
            .seconds
            .front()
            .is_some_and(|(then, _)| tick.saturating_sub(*then) >= 61)
        {
            self.seconds.pop_front();
        }
    }
    fn available(&self, limit: u32) -> bool {
        self.seconds
            .iter()
            .map(|(_, count)| u64::from(*count))
            .sum::<u64>()
            + u64::from(self.pending)
            < u64::from(limit)
    }
    fn sent(&mut self, tick: u64) {
        self.pending = self.pending.saturating_sub(1);
        self.expire(tick);
        if let Some((last, count)) = self.seconds.back_mut()
            && *last == tick
        {
            *count = count.saturating_add(1);
        } else {
            self.seconds.push_back((tick, 1));
        }
    }
    fn idle(&self) -> bool {
        self.pending == 0 && self.seconds.is_empty()
    }
}

struct Sample {
    at: Instant,
    elapsed: Duration,
    output: Option<u64>,
}

struct Entry {
    rate: Rate,
    active: usize,
    failures: u32,
    generation: u64,
    open_until: Option<Instant>,
    probe: Option<u64>,
    touched: Instant,
    samples: VecDeque<Sample>,
}

impl Entry {
    fn new(now: Instant) -> Self {
        Self {
            rate: Rate::default(),
            active: 0,
            failures: 0,
            generation: 0,
            open_until: None,
            probe: None,
            touched: now,
            samples: VecDeque::new(),
        }
    }
    fn expire(&mut self, now: Instant, tick: u64) {
        self.rate.expire(tick);
        self.samples
            .retain(|sample| now.saturating_duration_since(sample.at) < FRESH);
    }
    fn evictable(&self, now: Instant) -> bool {
        self.active == 0
            && self.rate.idle()
            && self.probe.is_none()
            && self.samples.is_empty()
            && now.saturating_duration_since(self.touched) >= FRESH
    }
    fn metric(&self, now: Instant, throughput: bool) -> Option<f64> {
        let mut count = 0;
        let mut seconds = 0.0;
        let mut tokens = 0.0;
        for sample in self
            .samples
            .iter()
            .filter(|sample| now.saturating_duration_since(sample.at) < FRESH)
        {
            if throughput && sample.output.is_none() {
                continue;
            }
            count += 1;
            seconds += sample.elapsed.as_secs_f64();
            tokens += sample.output.unwrap_or(0) as f64;
        }
        if count < MIN_SAMPLES || seconds <= 0.0 {
            return None;
        }
        Some(if throughput {
            tokens / seconds
        } else {
            seconds / count as f64
        })
    }
}

#[derive(Default)]
struct State {
    providers: HashMap<ProviderKey, Rate>,
    targets: HashMap<Key, Entry>,
    next: u64,
    active: usize,
}

/// Shared once per service. Memory and outstanding guard counts are bounded.
pub(crate) struct Runtime {
    epoch: Instant,
    state: Mutex<State>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            state: Mutex::new(State::default()),
        }
    }
}

impl Runtime {
    /// Atomically reserve both quotas and the sole half-open owner.
    pub(crate) fn admit(self: &Arc<Self>, key: Key, limits: &Limits) -> Result<Admission, Refusal> {
        self.admit_at(key, limits, Instant::now())
    }

    fn admit_at(
        self: &Arc<Self>,
        key: Key,
        limits: &Limits,
        now: Instant,
    ) -> Result<Admission, Refusal> {
        let bytes = key.namespace.len()
            + key.provider.len()
            + key.model.len()
            + key.account.as_ref().map_or(0, String::len)
            + key.endpoint.len()
            + limits
                .provider
                .as_ref()
                .map_or(0, |(pattern, _)| pattern.len());
        if bytes > MAX_KEY_BYTES {
            return Err(Refusal::Capacity);
        }
        let provider = limits.provider.as_ref().map(|(pattern, _)| ProviderKey {
            namespace: key.namespace.clone(),
            provider: key.provider.clone(),
            account: key.account.clone(),
            endpoint: key.endpoint.clone(),
            pattern: pattern.clone(),
        });
        let tick = now.saturating_duration_since(self.epoch).as_secs();
        let mut state = self.state.lock().map_err(|_| Refusal::Unavailable)?;
        if state.active >= MAX_IN_FLIGHT {
            return Err(Refusal::Capacity);
        }
        if let Some(entry) = state.targets.get_mut(&key) {
            entry.expire(now, tick);
        }
        if let Some(rate) = provider
            .as_ref()
            .and_then(|key| state.providers.get_mut(key))
        {
            rate.expire(tick);
        }
        let needed = usize::from(!state.targets.contains_key(&key))
            + usize::from(
                provider
                    .as_ref()
                    .is_some_and(|key| !state.providers.contains_key(key)),
            );
        if state.targets.len() + state.providers.len() + needed > MAX_ENTRIES {
            // Only capacity pressure scans the bounded map. Ordinary admission
            // touches its two exact buckets, independent of other tenants.
            for entry in state.targets.values_mut() {
                entry.expire(now, tick);
            }
            for rate in state.providers.values_mut() {
                rate.expire(tick);
            }
            state.targets.retain(|_, entry| !entry.evictable(now));
            state.providers.retain(|_, rate| !rate.idle());
            let needed = usize::from(!state.targets.contains_key(&key))
                + usize::from(
                    provider
                        .as_ref()
                        .is_some_and(|key| !state.providers.contains_key(key)),
                );
            if state.targets.len() + state.providers.len() + needed > MAX_ENTRIES {
                return Err(Refusal::Capacity);
            }
        }
        if let Some(entry) = state.targets.get(&key) {
            if entry.probe.is_some() || entry.open_until.is_some_and(|until| now < until) {
                return Err(Refusal::CircuitOpen);
            }
            if limits
                .model
                .is_some_and(|limit| !entry.rate.available(limit))
            {
                return Err(Refusal::RateLimited);
            }
        } else if limits.model == Some(0) {
            return Err(Refusal::RateLimited);
        }
        if let Some((_, limit)) = &limits.provider
            && (provider
                .as_ref()
                .and_then(|key| state.providers.get(key))
                .is_some_and(|rate| !rate.available(*limit))
                || *limit == 0)
        {
            return Err(Refusal::RateLimited);
        }
        let id = state.next.checked_add(1).ok_or(Refusal::Capacity)?;
        state.next = id;
        let entry = state
            .targets
            .entry(key.clone())
            .or_insert_with(|| Entry::new(now));
        let generation = entry.generation;
        if entry.open_until.is_some() {
            entry.probe = Some(id);
        }
        entry.active += 1;
        entry.touched = now;
        // Track model calls even under an unlimited revision so a stricter
        // revision of the same credential cannot reset an existing window.
        entry.rate.pending += 1;
        if let Some(provider) = &provider {
            state.providers.entry(provider.clone()).or_default().pending += 1;
        }
        state.active += 1;
        Ok(Admission {
            runtime: self.clone(),
            key,
            provider,
            id,
            generation,
            sent: None,
            finished: false,
        })
    }

    /// Sort only the supplied candidate indices; absent/freshness-failed data
    /// sorts last and equal scores preserve the retained order.
    pub(crate) fn order(
        &self,
        candidates: &[(Key, Option<u64>)],
        order: Order,
    ) -> Result<Vec<usize>, Refusal> {
        self.order_at(candidates, order, Instant::now())
    }

    fn order_at(
        &self,
        candidates: &[(Key, Option<u64>)],
        order: Order,
        now: Instant,
    ) -> Result<Vec<usize>, Refusal> {
        if candidates.len() > 256 {
            return Err(Refusal::Capacity);
        }
        let mut indices: Vec<_> = (0..candidates.len()).collect();
        if matches!(order, Order::Retained) {
            return Ok(indices);
        }
        let state = self.state.lock().map_err(|_| Refusal::Unavailable)?;
        indices.sort_by(|left, right| {
            let (left_key, left_weight) = &candidates[*left];
            let (right_key, right_weight) = &candidates[*right];
            if matches!(order, Order::Cost) {
                return missing_last(*left_weight, *right_weight, u64::cmp);
            }
            let throughput = matches!(order, Order::Throughput);
            let left = state
                .targets
                .get(left_key)
                .and_then(|entry| entry.metric(now, throughput));
            let right = state
                .targets
                .get(right_key)
                .and_then(|entry| entry.metric(now, throughput));
            missing_last(left, right, |a, b| {
                if throughput {
                    b.total_cmp(a)
                } else {
                    a.total_cmp(b)
                }
            })
        });
        Ok(indices)
    }
}

fn missing_last<T>(
    left: Option<T>,
    right: Option<T>,
    cmp: impl FnOnce(&T, &T) -> std::cmp::Ordering,
) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => cmp(&left, &right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Unique ownership from send admission through provider terminal evidence.
/// Dropping an unsent guard refunds reservations; dropping a sent guard retains
/// RPM and releases its probe without manufacturing successful health evidence.
pub(crate) struct Admission {
    runtime: Arc<Runtime>,
    key: Key,
    provider: Option<ProviderKey>,
    id: u64,
    generation: u64,
    sent: Option<Instant>,
    finished: bool,
}

impl Admission {
    /// Call synchronously after receipt arming and immediately before transport.
    pub(crate) fn sent(&mut self) -> Result<(), Refusal> {
        self.sent_at(Instant::now())
    }

    fn sent_at(&mut self, now: Instant) -> Result<(), Refusal> {
        if self.sent.is_some() {
            return Err(Refusal::Unavailable);
        }
        let mut state = self
            .runtime
            .state
            .lock()
            .map_err(|_| Refusal::Unavailable)?;
        let tick = now.saturating_duration_since(self.runtime.epoch).as_secs();
        state
            .targets
            .get_mut(&self.key)
            .ok_or(Refusal::Unavailable)?
            .rate
            .sent(tick);
        if let Some(provider) = &self.provider {
            state
                .providers
                .get_mut(provider)
                .ok_or(Refusal::Unavailable)?
                .sent(tick);
        }
        self.sent = Some(now);
        Ok(())
    }

    /// Only classify a completed attempt; this is health accounting, never a
    /// fallback decision. Receipt fences remain authoritative.
    pub(crate) fn finish(mut self, outcome: Outcome) {
        self.finish_at(outcome, Instant::now());
    }

    fn finish_at(&mut self, outcome: Outcome, now: Instant) {
        if self.finished {
            return;
        }
        self.finished = true;
        let Ok(mut state) = self.runtime.state.lock() else {
            return;
        };
        state.active = state.active.saturating_sub(1);
        if self.sent.is_none()
            && let Some(provider) = &self.provider
            && let Some(rate) = state.providers.get_mut(provider)
        {
            rate.pending = rate.pending.saturating_sub(1);
        }
        let Some(entry) = state.targets.get_mut(&self.key) else {
            return;
        };
        entry.active = entry.active.saturating_sub(1);
        entry.touched = now;
        let probe = entry.probe == Some(self.id);
        if probe {
            entry.probe = None;
        }
        let Some(started) = self.sent else {
            entry.rate.pending = entry.rate.pending.saturating_sub(1);
            return;
        };
        // A slow completion from before an opening cannot heal or count against
        // the new generation, including a probe another attempt already owns.
        if self.generation != entry.generation {
            return;
        }
        match outcome {
            Outcome::Success { output_tokens } => {
                entry.failures = 0;
                entry.open_until = None;
                let elapsed = now.saturating_duration_since(started);
                if !elapsed.is_zero() && elapsed <= Duration::from_secs(3600) {
                    if entry.samples.len() == SAMPLES {
                        entry.samples.pop_front();
                    }
                    entry.samples.push_back(Sample {
                        at: now,
                        elapsed,
                        output: output_tokens,
                    });
                }
            }
            Outcome::Unavailable => {
                entry.failures = entry.failures.saturating_add(1);
                if probe || entry.failures >= FAILURES {
                    entry.generation = entry.generation.saturating_add(1);
                    entry.open_until = Some(now + OPEN);
                }
            }
            Outcome::Neutral | Outcome::Unknown => {
                if probe {
                    entry.open_until = Some(now + OPEN);
                }
            }
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.finish_at(Outcome::Unknown, Instant::now());
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
