//! Bounded process-local scheduling state. This module never decides retries.
//!
//! Bytes cover allocation payload and conservative map/growth reservations,
//! including spare metadata, all retained key capacities and guard storage.
//! Bucket/sample buffers are reserved before admission and never grow on send or
//! completion. Allocator overhead and the caller's outer Arc are not RSS-accounted.

use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    mem::size_of,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_KEY_BYTES: usize = 16 * 1024;
const RATE_BUCKETS: usize = 61;
const SAMPLES: usize = 32;
const RATE_BYTES: usize = RATE_BUCKETS.next_power_of_two() * size_of::<(u64, u64)>();
const SAMPLE_BYTES: usize = SAMPLES * size_of::<Sample>();
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

impl Key {
    fn bytes(&self) -> Option<usize> {
        [&self.namespace, &self.provider, &self.model, &self.endpoint]
            .into_iter()
            .chain(self.account.iter())
            .try_fold(0usize, |bytes, part| bytes.checked_add(part.capacity()))
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ProviderKey {
    namespace: String,
    provider: String,
    account: Option<String>,
    endpoint: String,
    pattern: String,
}

impl ProviderKey {
    fn bytes(&self) -> Option<usize> {
        [
            &self.namespace,
            &self.provider,
            &self.endpoint,
            &self.pattern,
        ]
        .into_iter()
        .chain(self.account.iter())
        .try_fold(0usize, |bytes, part| bytes.checked_add(part.capacity()))
    }
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

struct Rate {
    pending: usize,
    active: usize,
    // At most 61 second buckets. Round expiry upwards: never admit more than
    // the configured count in any rolling 60 seconds, even at second edges.
    seconds: VecDeque<(u64, u64)>,
}

impl Rate {
    fn new() -> Result<Self, Refusal> {
        let mut seconds = VecDeque::new();
        seconds
            .try_reserve_exact(RATE_BUCKETS)
            .map_err(|_| Refusal::Capacity)?;
        if seconds.capacity() > RATE_BUCKETS.next_power_of_two() {
            return Err(Refusal::Capacity);
        }
        Ok(Self {
            pending: 0,
            active: 0,
            seconds,
        })
    }

    fn expire(&mut self, tick: u64) {
        while self
            .seconds
            .front()
            .is_some_and(|(then, _)| tick.saturating_sub(*then) >= RATE_BUCKETS as u64)
        {
            self.seconds.pop_front();
        }
    }
    fn available(&self, limit: u32) -> bool {
        let pending = u64::try_from(self.pending).unwrap_or(u64::MAX);
        self.seconds
            .iter()
            .fold(pending, |total, (_, count)| total.saturating_add(*count))
            < u64::from(limit)
    }
    fn sent(&mut self, tick: u64) {
        self.pending = self.pending.saturating_sub(1);
        // Send timestamps can reach the mutex out of order. Rounding a delayed
        // send forwards preserves the window and the 61-bucket allocation.
        let tick = self
            .seconds
            .back()
            .map_or(tick, |(last, _)| tick.max(*last));
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
        self.active == 0 && self.pending == 0 && self.seconds.is_empty()
    }
}

struct Sample {
    at: Instant,
    elapsed: Duration,
    output: Option<u64>,
}

struct Entry {
    rate: Rate,
    failures: u32,
    generation: u64,
    open_until: Option<Instant>,
    probe: Option<u64>,
    touched: Instant,
    samples: VecDeque<Sample>,
}

impl Entry {
    fn new(now: Instant) -> Result<Self, Refusal> {
        let mut samples = VecDeque::new();
        samples
            .try_reserve_exact(SAMPLES)
            .map_err(|_| Refusal::Capacity)?;
        if samples.capacity() > SAMPLES {
            return Err(Refusal::Capacity);
        }
        Ok(Self {
            rate: Rate::new()?,
            failures: 0,
            generation: 0,
            open_until: None,
            probe: None,
            touched: now,
            samples,
        })
    }
    fn expire(&mut self, now: Instant, tick: u64) {
        self.rate.expire(tick);
        self.samples
            .retain(|sample| now.saturating_duration_since(sample.at) < FRESH);
    }
    fn reopen(&mut self, now: Instant) {
        self.open_until = now.checked_add(OPEN);
        if self.open_until.is_none() {
            // No representable deadline: keep the circuit closed to admission.
            self.probe = Some(0);
        }
    }
    fn evictable(&self, now: Instant) -> bool {
        self.rate.idle()
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
    // Map allocations stay charged across removals, including tombstones.
    provider_bytes: usize,
    target_bytes: usize,
    // Key heaps, future bucket/sample storage, and outstanding guard copies.
    bytes: usize,
    next: u64,
    active: usize,
}

// Conservative SwissTable allocation payload: spare buckets, control bytes,
// alignment and the trailing control group. This is not allocator/RSS accounting.
// https://doc.rust-lang.org/src/std/collections/hash/map.rs.html
fn map_bytes<K, V>(capacity: usize) -> Option<usize> {
    if capacity == 0 {
        return Some(0);
    }
    capacity
        .checked_add(1)?
        .checked_mul(2)?
        .checked_mul(size_of::<(K, V)>().checked_add(1)?)?
        .checked_add(64)
}

fn growth_bytes<K, V>(map: &HashMap<K, V>, additional: usize) -> Option<usize> {
    let needed = map.len().checked_add(additional)?;
    if needed <= map.capacity() {
        return Some(0);
    }
    // A fresh table rounds up for its load factor and power-of-two buckets.
    // Reserve the replacement alongside the old table before moving entries.
    map_bytes::<K, V>(needed.checked_mul(2)?.max(4))
}

fn grow<K: Eq + Hash, V>(
    map: &mut HashMap<K, V>,
    charged: &mut usize,
    additional: usize,
) -> Result<(), Refusal> {
    let reservation = growth_bytes(map, additional).ok_or(Refusal::Capacity)?;
    if reservation == 0 {
        return Ok(());
    }
    let mut replacement = HashMap::new();
    replacement
        .try_reserve(map.len().checked_add(additional).ok_or(Refusal::Capacity)?)
        .map_err(|_| Refusal::Capacity)?;
    let bytes = map_bytes::<K, V>(replacement.capacity()).ok_or(Refusal::Capacity)?;
    if bytes > reservation {
        return Err(Refusal::Capacity);
    }
    replacement.extend(map.drain());
    *map = replacement;
    *charged = bytes;
    Ok(())
}

impl State {
    fn used_bytes(&self) -> Option<usize> {
        size_of::<Runtime>()
            .checked_add(self.bytes)?
            .checked_add(self.target_bytes)?
            .checked_add(self.provider_bytes)
    }

    fn reclaim(&mut self, now: Instant, tick: u64) {
        self.targets.retain(|key, entry| {
            entry.expire(now, tick);
            if !entry.evictable(now) {
                return true;
            }
            if let Some(bytes) = key.bytes() {
                self.bytes -= bytes + RATE_BYTES + SAMPLE_BYTES;
            }
            false
        });
        self.providers.retain(|key, rate| {
            rate.expire(tick);
            if !rate.idle() {
                return true;
            }
            if let Some(bytes) = key.bytes() {
                self.bytes -= bytes + RATE_BYTES;
            }
            false
        });
        // Retain spare capacity while there are owners; only empty tables can
        // release all their metadata without moving live state.
        if self.targets.is_empty() {
            self.targets = HashMap::new();
            self.target_bytes = 0;
        }
        if self.providers.is_empty() {
            self.providers = HashMap::new();
            self.provider_bytes = 0;
        }
    }
}

/// Shared once per service. Memory and outstanding guard counts are bounded.
pub(crate) struct Runtime {
    epoch: Instant,
    max_entries: usize,
    max_bytes: usize,
    max_in_flight: usize,
    state: Mutex<State>,
}

#[cfg(test)]
impl Default for Runtime {
    fn default() -> Self {
        let routing = crate::config::tenancy::capacity::Routing::default();
        Self {
            epoch: Instant::now(),
            max_entries: routing.entries,
            max_bytes: routing.bytes,
            max_in_flight: crate::config::tenancy::capacity::Execution::default().instance_active,
            state: Mutex::new(State::default()),
        }
    }
}

impl Runtime {
    /// Pure numeric copy; in particular this never expires rates or reclaims maps.
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(crate) fn observed(
        &self,
    ) -> Result<
        crate::telemetry::receipt_latency::capacity::runtime::RoutingUsage,
        crate::telemetry::receipt_latency::capacity::ReadError,
    > {
        use crate::telemetry::receipt_latency::capacity::{ReadError, runtime::RoutingUsage};
        let state = self.state.try_lock().map_err(|_| ReadError::Unavailable)?;
        Ok(RoutingUsage {
            targets: u64::try_from(state.targets.len()).map_err(|_| ReadError::Invalid)?,
            providers: u64::try_from(state.providers.len()).map_err(|_| ReadError::Invalid)?,
            bytes: u64::try_from(state.used_bytes().ok_or(ReadError::Invalid)?)
                .map_err(|_| ReadError::Invalid)?,
            active: u64::try_from(state.active).map_err(|_| ReadError::Invalid)?,
        })
    }

    /// Construct lazy routing state with operator budgets and the active-owner cap.
    ///
    /// # Errors
    /// Rejects zero or unrepresentable budgets and an insufficient base allocation.
    pub(crate) fn configured(
        limits: &crate::config::tenancy::capacity::Routing,
        max_in_flight: usize,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(limits.entries > 0, "routing entries must be positive");
        anyhow::ensure!(max_in_flight > 0, "routing active owners must be positive");
        anyhow::ensure!(
            limits.bytes >= size_of::<Self>() && limits.bytes <= isize::MAX as usize,
            "routing bytes cannot represent the runtime allocation"
        );
        anyhow::ensure!(
            map_bytes::<Key, Entry>(limits.entries)
                .is_some_and(|bytes| bytes <= isize::MAX as usize)
                && max_in_flight
                    .checked_mul(size_of::<Admission>())
                    .is_some_and(|bytes| bytes <= isize::MAX as usize),
            "routing capacity exceeds the addressable allocation range"
        );
        Ok(Self {
            epoch: Instant::now(),
            max_entries: limits.entries,
            max_bytes: limits.bytes,
            max_in_flight,
            state: Mutex::new(State::default()),
        })
    }

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
        let bytes = [
            key.namespace.len(),
            key.provider.len(),
            key.model.len(),
            key.account.as_ref().map_or(0, String::len),
            key.endpoint.len(),
            limits
                .provider
                .as_ref()
                .map_or(0, |(pattern, _)| pattern.len()),
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
        .ok_or(Refusal::Capacity)?;
        let key_bytes = key.bytes().ok_or(Refusal::Capacity)?;
        if bytes > MAX_KEY_BYTES || key_bytes > self.max_bytes {
            return Err(Refusal::Capacity);
        }
        let provider = limits.provider.as_ref().map(|(pattern, _)| ProviderKey {
            namespace: key.namespace.clone(),
            provider: key.provider.clone(),
            account: key.account.clone(),
            endpoint: key.endpoint.clone(),
            pattern: pattern.clone(),
        });
        let provider_bytes = provider
            .as_ref()
            .map_or(Some(0), ProviderKey::bytes)
            .ok_or(Refusal::Capacity)?;
        let guard_bytes = size_of::<Admission>()
            .checked_add(key_bytes)
            .and_then(|bytes| bytes.checked_add(provider_bytes))
            .ok_or(Refusal::Capacity)?;
        let tick = now.saturating_duration_since(self.epoch).as_secs();
        let mut state = self.state.lock().map_err(|_| Refusal::Unavailable)?;
        if state.active >= self.max_in_flight {
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
        let fits = |state: &State| -> Option<bool> {
            let target = usize::from(!state.targets.contains_key(&key));
            let provider = usize::from(
                provider
                    .as_ref()
                    .is_some_and(|key| !state.providers.contains_key(key)),
            );
            let count = state
                .targets
                .len()
                .checked_add(state.providers.len())?
                .checked_add(target)?
                .checked_add(provider)?;
            let bytes = state
                .used_bytes()?
                .checked_add(guard_bytes)?
                .checked_add(
                    target.checked_mul(
                        key_bytes
                            .checked_add(RATE_BYTES)?
                            .checked_add(SAMPLE_BYTES)?,
                    )?,
                )?
                .checked_add(provider.checked_mul(provider_bytes.checked_add(RATE_BYTES)?)?)?
                .checked_add(growth_bytes(&state.targets, target)?)?
                .checked_add(growth_bytes(&state.providers, provider)?)?;
            Some(count <= self.max_entries && bytes <= self.max_bytes)
        };
        if fits(&state) != Some(true) {
            // Only capacity pressure scans the bounded maps. Live windows,
            // probes and active attempts remain pinned under either budget.
            state.reclaim(now, tick);
            if fits(&state) != Some(true) {
                return Err(Refusal::Capacity);
            }
        }
        let id = state.next.checked_add(1).ok_or(Refusal::Capacity)?;
        let active = state.active.checked_add(1).ok_or(Refusal::Capacity)?;
        let next_counts = |rate: Option<&Rate>| -> Result<(usize, usize), Refusal> {
            Ok((
                rate.map_or(0, |rate| rate.pending)
                    .checked_add(1)
                    .ok_or(Refusal::Capacity)?,
                rate.map_or(0, |rate| rate.active)
                    .checked_add(1)
                    .ok_or(Refusal::Capacity)?,
            ))
        };
        let target_counts = next_counts(state.targets.get(&key).map(|entry| &entry.rate))?;
        let provider_counts =
            next_counts(provider.as_ref().and_then(|key| state.providers.get(key)))?;
        // Prepare all fallible storage before changing either quota or owner.
        let target = if state.targets.contains_key(&key) {
            None
        } else {
            let copy = key.clone();
            let bytes = copy
                .bytes()
                .filter(|bytes| *bytes <= key_bytes)
                .ok_or(Refusal::Capacity)?;
            Some((copy, Entry::new(now)?, bytes + RATE_BYTES + SAMPLE_BYTES))
        };
        let provider_entry = if let Some(provider) = &provider
            && !state.providers.contains_key(provider)
        {
            let copy = provider.clone();
            let bytes = copy
                .bytes()
                .filter(|bytes| *bytes <= provider_bytes)
                .ok_or(Refusal::Capacity)?;
            Some((copy, Rate::new()?, bytes + RATE_BYTES))
        } else {
            None
        };
        let State {
            targets,
            target_bytes,
            providers,
            provider_bytes,
            ..
        } = &mut *state;
        grow(targets, target_bytes, usize::from(target.is_some()))?;
        grow(
            providers,
            provider_bytes,
            usize::from(provider_entry.is_some()),
        )?;
        if let Some((key, entry, bytes)) = target {
            state.targets.insert(key, entry);
            state.bytes += bytes;
        }
        if let Some((key, rate, bytes)) = provider_entry {
            state.providers.insert(key, rate);
            state.bytes += bytes;
        }
        state.next = id;
        let entry = state.targets.get_mut(&key).ok_or(Refusal::Unavailable)?;
        let generation = entry.generation;
        if entry.open_until.is_some() {
            entry.probe = Some(id);
        }
        entry.rate.active = target_counts.1;
        entry.touched = now;
        // Track model calls even under an unlimited revision so a stricter
        // revision of the same credential cannot reset an existing window.
        entry.rate.pending = target_counts.0;
        if let Some(provider) = &provider {
            let rate = state
                .providers
                .get_mut(provider)
                .ok_or(Refusal::Unavailable)?;
            rate.pending = provider_counts.0;
            rate.active = provider_counts.1;
        }
        state.active = active;
        state.bytes += guard_bytes;
        Ok(Admission {
            runtime: self.clone(),
            key,
            provider,
            bytes: guard_bytes,
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
    bytes: usize,
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
        if self.sent.is_some() || self.finished {
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
        if let Some(provider) = &self.provider
            && let Some(rate) = state.providers.get_mut(provider)
        {
            rate.active = rate.active.saturating_sub(1);
            if self.sent.is_none() {
                rate.pending = rate.pending.saturating_sub(1);
            }
        }
        let Some(entry) = state.targets.get_mut(&self.key) else {
            return;
        };
        entry.rate.active = entry.rate.active.saturating_sub(1);
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
                    entry.reopen(now);
                }
            }
            Outcome::Neutral | Outcome::Unknown => {
                if probe {
                    entry.reopen(now);
                }
            }
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.finish_at(Outcome::Unknown, Instant::now());
        // finish_at can leave a guard in the caller's custody. Keep its copies
        // charged until they are actually destroyed, even after completion.
        self.key.namespace = String::new();
        self.key.provider = String::new();
        self.key.model = String::new();
        self.key.account = None;
        self.key.endpoint = String::new();
        self.provider = None;
        if let Ok(mut state) = self.runtime.state.lock() {
            state.bytes = state.bytes.saturating_sub(self.bytes);
        }
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
