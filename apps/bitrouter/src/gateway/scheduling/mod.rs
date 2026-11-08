//! Process-local reservation custody, separate from execution authority and receipts.
//!
//! Only the concrete gateway executor can attest send and terminal evidence.

pub(crate) mod bridge;
pub(crate) mod observation;
pub(crate) mod payload;
pub(crate) mod prefix;
mod queue;
pub(crate) mod runtime;

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;
use tokio::time::{Duration, Instant};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::config::inference_routing::{DestinationConfig, InferenceRoutingConfig, LimitConfig};
use crate::gateway::error::{BitrouterError, Result};
use crate::gateway::pipeline::types::RoutingTarget;
use observation::{Counters, SAMPLE_BYTES, SAMPLE_TTL, Sample, Shape};

const BASE_BYTES: u64 = std::mem::size_of::<(Inner, Scheduler)>() as u64 + 512;
const DESTINATION_BYTES: u64 = std::mem::size_of::<Destination>() as u64 + 64;
const ATTEMPT_BYTES: u64 = std::mem::size_of::<Record>() as u64 + 128;
const OWNER_BYTES: u64 = std::mem::size_of::<Reservation>() as u64 + 64;
const RESERVED: u8 = 0;
const SENDING: u8 = 1;
const UNSENT: u8 = 2;
const REJECTED: u8 = 3;
const COMPLETED: u8 = 4;
const FAILED: u8 = 5;
const UNKNOWN: u8 = 6;
const EXPIRED: u8 = 7;

#[derive(Clone, Copy)]
enum UnknownReason {
    Timeout,
    InvalidResponse,
    Transport,
    Dropped,
}

/// A capability for exactly one target in one frozen route allocation. The
/// address is an opaque, never-dereferenced equality token, not a secret hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CandidateId {
    instance: uuid::Uuid,
    generation: u64,
    destination: usize,
    ordinal: usize,
    target: usize,
    protocol: u8,
}

impl CandidateId {
    fn matches(self, target: &RoutingTarget) -> bool {
        self.target == std::ptr::from_ref(target) as usize
            && Some(self.protocol) == protocol(target)
    }
}

struct Destination {
    config: DestinationConfig,
    reserved: u32,
    sending: u32,
    uncertain: u32,
    work: u64,
    counters: Counters,
    // At most 32 total samples, hence at most 32 for any destination/shape.
    samples: VecDeque<Sample>,
}

struct Record {
    candidate: CandidateId,
    work: u64,
    shape: Shape,
    deadline: Instant,
    sent_at: Option<Instant>,
    status: Option<u16>,
    first_output: Option<Duration>,
    expires: Option<Instant>,
    witness: Arc<AtomicU8>,
    prefix: Option<prefix::Binding>,
}

struct State {
    closed: bool,
    next: u64,
    entries: u32,
    bytes: u64,
    outstanding: u32,
    owners: u32,
    destinations: Vec<Destination>,
    records: BTreeMap<u64, Record>,
    queue: queue::Queue,
    prefix: Option<prefix::Index>,
}

struct Inner {
    instance: uuid::Uuid,
    limits: LimitConfig,
    state: Mutex<State>,
    changed: Notify,
    stop: CancellationToken,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

pub(crate) struct Scheduler {
    inner: Arc<Inner>,
    workers: Arc<Workers>,
    pub(crate) request_timeout: Duration,
    queue_timeout: Duration,
    projector: Option<prefix::Projector>,
    generation: u64,
    policy: crate::config::inference_routing::Policy,
    weights: BTreeMap<String, u32>,
}

struct Workers {
    tracker: TaskTracker,
    stop: CancellationToken,
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(Debug)]
pub(crate) struct DestinationView {
    pub(crate) destination: usize,
    pub(crate) active: u32,
    pub(crate) uncertain: u32,
    pub(crate) work: u64,
    pub(crate) mean_success_millis: Option<u64>,
    pub(crate) counters: Counters,
}

impl Scheduler {
    #[cfg(test)]
    pub(crate) fn from_config(config: &InferenceRoutingConfig) -> Result<Option<Arc<Self>>> {
        if !config.enabled {
            return Ok(None);
        }
        Self::new(config, 1).map(Some)
    }

    pub(crate) fn new(config: &InferenceRoutingConfig, generation: u64) -> Result<Arc<Self>> {
        config.validate_settings()?;
        if generation == 0 || config.destinations.is_empty() {
            return Err(unavailable());
        }
        let (bytes, entries) = Self::capacity(config)?;
        let inner = Arc::new(Inner {
            instance: uuid::Uuid::new_v4(),
            limits: config.limits.clone(),
            state: Mutex::new(State {
                closed: false,
                next: 0,
                entries,
                bytes,
                outstanding: 0,
                owners: 0,
                destinations: config
                    .destinations
                    .iter()
                    .cloned()
                    .map(|config| Destination {
                        config,
                        reserved: 0,
                        sending: 0,
                        uncertain: 0,
                        work: 0,
                        counters: Counters::default(),
                        samples: VecDeque::new(),
                    })
                    .collect(),
                records: BTreeMap::new(),
                queue: queue::Queue::new(config),
                prefix: matches!(
                    config.policy,
                    crate::config::inference_routing::Policy::PrefixAffinity
                )
                .then(|| prefix::Index::new(&config.prefix)),
            }),
            changed: Notify::new(),
            stop: CancellationToken::new(),
        });
        let tracker = TaskTracker::new();
        tracker.spawn(queue::run(inner.clone()));
        let workers = Arc::new(Workers {
            tracker,
            stop: inner.stop.clone(),
        });
        Ok(Arc::new(Self {
            inner,
            workers,
            request_timeout: Duration::from_millis(u64::from(config.queue.request_timeout_ms)),
            queue_timeout: Duration::from_millis(u64::from(config.queue.wait_timeout_ms)),
            projector: matches!(
                config.policy,
                crate::config::inference_routing::Policy::PrefixAffinity
            )
            .then(|| prefix::Projector::new(&config.prefix, generation)),
            generation,
            policy: config.policy,
            weights: config.tenant_weights.clone(),
        }))
    }

    pub(crate) fn capacity(config: &InferenceRoutingConfig) -> Result<(u64, u32)> {
        let bytes = config
            .destinations
            .iter()
            .try_fold(
                BASE_BYTES
                    + config
                        .tenant_weights
                        .keys()
                        .map(|name| name.len() as u64 + 112)
                        .sum::<u64>(),
                |sum, d| {
                    sum.checked_add(
                        DESTINATION_BYTES
                            + d.id.len() as u64
                            + d.provider.len() as u64
                            + d.model.len() as u64
                            + d.endpoint.len() as u64
                            + d.account.as_ref().map_or(0, |s| s.len() as u64),
                    )
                },
            )
            .ok_or_else(unavailable)?;
        let entries = u32::try_from(config.destinations.len() + config.tenant_weights.len())
            .map_err(|_| unavailable())?;
        if bytes + ATTEMPT_BYTES + OWNER_BYTES > u64::from(config.limits.max_state_bytes)
            || entries
                .checked_add(2)
                .is_none_or(|n| n > config.limits.max_state_entries)
        {
            return Err(unavailable());
        }
        Ok((bytes, entries))
    }

    pub(crate) fn retained_bytes(&self) -> u64 {
        self.weights
            .keys()
            .map(|name| name.capacity() as u64 + 112)
            .sum::<u64>()
            + std::mem::size_of::<Self>() as u64
            + 256
    }

    fn snapshot(&self, config: &InferenceRoutingConfig, generation: u64) -> Arc<Self> {
        let projector = matches!(
            config.policy,
            crate::config::inference_routing::Policy::PrefixAffinity
        )
        .then(|| prefix::Projector::new(&config.prefix, generation));
        if projector.is_some() {
            self.inner
                .lock()
                .prefix
                .get_or_insert_with(|| prefix::Index::new(&config.prefix));
        }
        Arc::new(Self {
            inner: self.inner.clone(),
            workers: self.workers.clone(),
            generation,
            request_timeout: Duration::from_millis(u64::from(config.queue.request_timeout_ms)),
            queue_timeout: Duration::from_millis(u64::from(config.queue.wait_timeout_ms)),
            policy: config.policy,
            weights: config.tenant_weights.clone(),
            projector,
        })
    }

    pub(crate) fn candidate(&self, target: &RoutingTarget, ordinal: usize) -> Result<CandidateId> {
        let state = self.inner.lock();
        if state.closed {
            return Err(unavailable());
        }
        let destination = state
            .destinations
            .iter()
            .position(|d| maps(&d.config, target))
            .ok_or_else(|| BitrouterError::bad_request("scheduling destination is not mapped"))?;
        Ok(CandidateId {
            instance: self.inner.instance,
            generation: self.generation,
            destination,
            ordinal,
            target: std::ptr::from_ref(target) as usize,
            protocol: protocol(target).ok_or_else(unavailable)?,
        })
    }

    /// Direct booking is retained only for reservation lifecycle fixtures.
    #[cfg(test)]
    pub(crate) fn book(
        &self,
        candidate: CandidateId,
        target: &RoutingTarget,
        work: u64,
        shape: Shape,
        deadline: Instant,
    ) -> Result<Reservation> {
        if candidate.generation != self.generation {
            return Err(unavailable());
        }
        let mut state = self.inner.lock();
        validate(&self.inner, &state, candidate, target, work)?;
        reap(
            &mut state,
            Instant::now(),
            Duration::from_millis(u64::from(self.inner.limits.uncertainty_ttl_ms)),
        );
        let result = reserve(
            &self.inner,
            &mut state,
            candidate,
            work,
            shape,
            deadline,
            None,
        );
        drop(state);
        self.inner.changed.notify_one();
        result
    }

    /// Read-only: expiry is filtered, never performed, by inspection.
    pub(crate) fn inspect(&self, shape: Shape) -> Vec<DestinationView> {
        let now = Instant::now();
        let state = self.inner.lock();
        state
            .destinations
            .iter()
            .enumerate()
            .map(|(index, d)| {
                let ttl = Duration::from_millis(u64::from(self.inner.limits.uncertainty_ttl_ms));
                let (active, uncertain, work) = state
                    .records
                    .values()
                    .filter(|r| r.candidate.destination == index)
                    .fold((0, 0, 0), |(active, uncertain, work), r| {
                        match r.witness.load(Ordering::Acquire) {
                            RESERVED if now < r.deadline => (active + 1, uncertain, work + r.work),
                            SENDING if now < r.deadline => (active + 1, uncertain, work + r.work),
                            SENDING if now < r.deadline + ttl => {
                                (active, uncertain + 1, work + r.work)
                            }
                            UNKNOWN if r.expires.is_some_and(|at| now < at) => {
                                (active, uncertain + 1, work + r.work)
                            }
                            _ => (active, uncertain, work),
                        }
                    });
                let mut samples = d.samples.iter().filter(|sample| {
                    sample.shape == shape && now.duration_since(sample.at) < SAMPLE_TTL
                });
                let (count, sum) = samples.by_ref().fold((0u64, 0u128), |(n, sum), sample| {
                    (n + 1, sum + u128::from(sample.millis))
                });
                DestinationView {
                    destination: index,
                    active,
                    uncertain,
                    work,
                    mean_success_millis: (count >= 3).then(|| (sum / u128::from(count)) as u64),
                    counters: d.counters,
                }
            })
            .collect()
    }

    fn inspect_prefix(&self, binding: &prefix::Binding, exclude: Option<u64>) -> prefix::View {
        self.inner
            .lock()
            .prefix
            .as_ref()
            .map_or_else(prefix::View::default, |index| {
                index.inspect(binding, exclude, Instant::now())
            })
    }

    pub(crate) fn close(&self) {
        self.inner.lock().closed = true;
        self.inner.changed.notify_one();
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.close();
        self.inner.stop.cancel();
        self.workers.tracker.close();
        self.workers.tracker.wait().await;
        if self.inner.lock().owners != 0 {
            return Err(unavailable());
        }
        Ok(())
    }
}

/// The sole capacity custodian. Never clone this object into observers.
pub(crate) struct Reservation {
    observation: Observation,
}

impl Reservation {
    pub(crate) fn failed(&self, error: &BitrouterError) {
        if self.observation.witness.load(Ordering::Acquire) == SENDING {
            let reason = match error {
                BitrouterError::UpstreamTimeout => UnknownReason::Timeout,
                BitrouterError::UpstreamInvalidResponse { .. } => UnknownReason::InvalidResponse,
                _ => UnknownReason::Transport,
            };
            self.observation.finish(UNKNOWN, Some(reason));
        }
    }
    pub(crate) fn observer(&self) -> Observation {
        self.observation.clone()
    }
    pub(crate) fn deadline(&self) -> Instant {
        self.observation.deadline
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        match self.observation.witness.load(Ordering::Acquire) {
            RESERVED => self.observation.finish(UNSENT, None),
            SENDING => self.observation.finish(
                UNKNOWN,
                Some(if Instant::now() >= self.observation.deadline {
                    UnknownReason::Timeout
                } else {
                    UnknownReason::Dropped
                }),
            ),
            _ => {}
        }
        let mut state = self.observation.inner.lock();
        state.owners -= 1;
        state.entries -= 1;
        state.bytes -= OWNER_BYTES;
        drop(state);
        self.observation.inner.changed.notify_one();
    }
}

/// A bounded, non-owning event capability; it cannot book or retrieve a target.
#[derive(Clone)]
pub(crate) struct Observation {
    inner: Arc<Inner>,
    id: u64,
    candidate: CandidateId,
    witness: Arc<AtomicU8>,
    deadline: Instant,
}

impl Observation {
    pub(crate) fn begin_send(&self, target: &RoutingTarget) -> Result<()> {
        let mut state = self.inner.lock();
        if state.closed || !self.candidate.matches(target) {
            return Err(unavailable());
        }
        if !state
            .destinations
            .get(self.candidate.destination)
            .is_some_and(|d| maps(&d.config, target))
        {
            return Err(unavailable());
        }
        let record = state.records.get_mut(&self.id).ok_or_else(unavailable)?;
        if Instant::now() >= record.deadline {
            return Err(BitrouterError::UpstreamTimeout);
        }
        if record.witness.load(Ordering::Acquire) != RESERVED {
            return Err(unavailable());
        }
        let binding = record.prefix.clone();
        if let (Some(index), Some(binding)) = (&mut state.prefix, &binding) {
            index.observe(self.id, binding, prefix::Event::Sent, Instant::now());
        }
        // Prefix bookkeeping is bounded but may take time. It must finish
        // before the final deadline check and actual-send witness transition.
        let now = Instant::now();
        if now >= self.deadline {
            if let (Some(index), Some(binding)) = (&mut state.prefix, &binding) {
                index.observe(self.id, binding, prefix::Event::Removed, now);
            }
            return Err(BitrouterError::UpstreamTimeout);
        }
        let record = state.records.get_mut(&self.id).ok_or_else(unavailable)?;
        record.sent_at = Some(now);
        record.witness.store(SENDING, Ordering::Release);
        let d = &mut state.destinations[self.candidate.destination];
        d.reserved -= 1;
        d.sending += 1;
        drop(state);
        self.inner.changed.notify_one();
        Ok(())
    }
    pub(crate) fn headers(&self, status: u16) {
        if !(200..=599).contains(&status) {
            return;
        }
        {
            let mut state = self.inner.lock();
            let Some(record) = state.records.get_mut(&self.id) else {
                return;
            };
            if !matches!(record.witness.load(Ordering::Acquire), SENDING | UNKNOWN)
                || record.status.is_some()
            {
                return;
            }
            record.status = Some(status);
            let d = &mut state.destinations[self.candidate.destination];
            d.counters.response_headers = d.counters.response_headers.saturating_add(1);
        }
        if (300..=599).contains(&status) {
            self.finish(REJECTED, None);
        }
    }
    pub(crate) fn part(&self, part: &bitrouter_ai::types::StreamPart) {
        let mut state = self.inner.lock();
        let Some(record) = state.records.get_mut(&self.id) else {
            return;
        };
        if record.witness.load(Ordering::Acquire) != SENDING {
            return;
        }
        if record.shape.streaming() && record.first_output.is_none() && observation::useful(part) {
            record.first_output = record.sent_at.map(|at| at.elapsed());
        }
        if let bitrouter_ai::types::StreamPart::Usage { usage } = part {
            let d = &mut state.destinations[self.candidate.destination];
            d.counters.usage_reports = d.counters.usage_reports.saturating_add(1);
            cache_usage(&mut state, self.id, usage);
            drop(state);
            self.inner.changed.notify_one();
        }
    }
    pub(crate) fn completed(&self, success: bool) {
        self.finish(if success { COMPLETED } else { FAILED }, None);
    }
    pub(crate) fn usage(&self, usage: &bitrouter_ai::types::Usage) {
        let mut state = self.inner.lock();
        if state
            .records
            .get(&self.id)
            .is_some_and(|r| r.witness.load(Ordering::Acquire) == SENDING)
        {
            let d = &mut state.destinations[self.candidate.destination];
            d.counters.usage_reports = d.counters.usage_reports.saturating_add(1);
        }
        cache_usage(&mut state, self.id, usage);
        drop(state);
        self.inner.changed.notify_one();
    }
    pub(crate) fn blocks_fallback(&self) -> bool {
        !matches!(
            self.witness.load(Ordering::Acquire),
            RESERVED | UNSENT | REJECTED
        )
    }
    fn finish(&self, outcome: u8, reason: Option<UnknownReason>) {
        let now = Instant::now();
        let mut state = self.inner.lock();
        let Some(mut record) = state.records.remove(&self.id) else {
            return;
        };
        let previous = record.witness.load(Ordering::Acquire);
        if !matches!(previous, RESERVED | SENDING | UNKNOWN) {
            state.records.insert(self.id, record);
            return;
        }
        // Late terminal evidence can close an unknown record, but expiry or
        // earlier completion removes it; neither can release the charge twice.
        if (previous == RESERVED && outcome != UNSENT)
            || (previous == UNKNOWN && !matches!(outcome, COMPLETED | FAILED | REJECTED))
        {
            state.records.insert(self.id, record);
            return;
        }
        if matches!(outcome, COMPLETED | FAILED)
            && !record
                .status
                .is_some_and(|status| (200..300).contains(&status))
        {
            state.records.insert(self.id, record);
            return;
        }
        if let (Some(index), Some(binding)) = (&mut state.prefix, &record.prefix) {
            let event = match outcome {
                COMPLETED => prefix::Event::Succeeded,
                UNKNOWN => prefix::Event::Unknown,
                _ => prefix::Event::Removed,
            };
            index.observe(self.id, binding, event, now);
        }
        let d = &mut state.destinations[record.candidate.destination];
        match previous {
            RESERVED => d.reserved -= 1,
            SENDING => d.sending -= 1,
            _ => d.uncertain -= 1,
        }
        record.witness.store(outcome, Ordering::Release);
        match outcome {
            UNSENT => d.counters.unsent = d.counters.unsent.saturating_add(1),
            REJECTED => d.counters.rejected = d.counters.rejected.saturating_add(1),
            COMPLETED => d.counters.completed = d.counters.completed.saturating_add(1),
            FAILED => d.counters.failed_terminal = d.counters.failed_terminal.saturating_add(1),
            UNKNOWN => {
                d.counters.unknown = d.counters.unknown.saturating_add(1);
                let counter = match reason.unwrap_or(UnknownReason::Dropped) {
                    UnknownReason::Timeout => &mut d.counters.timeouts,
                    UnknownReason::InvalidResponse => &mut d.counters.invalid_responses,
                    UnknownReason::Transport => &mut d.counters.transport_failures,
                    UnknownReason::Dropped => &mut d.counters.dropped,
                };
                *counter = counter.saturating_add(1);
                d.uncertain += 1;
                record.expires = Some(
                    now.min(record.deadline)
                        + Duration::from_millis(u64::from(self.inner.limits.uncertainty_ttl_ms)),
                );
                state.records.insert(self.id, record);
                drop(state);
                self.inner.changed.notify_one();
                return;
            }
            _ => {}
        }
        d.work -= record.work;
        let elapsed = record.sent_at.map(|at| now.duration_since(at));
        let sample = if record.shape.streaming() {
            record.first_output
        } else {
            elapsed
        };
        state.outstanding -= 1;
        state.entries -= 1;
        state.bytes -= ATTEMPT_BYTES
            + record
                .prefix
                .as_ref()
                .map_or(0, prefix::Binding::retained_bytes);
        if outcome == COMPLETED
            && let Some(elapsed) = sample
        {
            let d = &mut state.destinations[record.candidate.destination];
            if d.samples.len() == 32 {
                d.samples.pop_front();
                state.entries -= 1;
                state.bytes -= SAMPLE_BYTES;
            }
            if state.entries < self.inner.limits.max_state_entries
                && state.bytes + SAMPLE_BYTES <= u64::from(self.inner.limits.max_state_bytes)
            {
                state.destinations[record.candidate.destination]
                    .samples
                    .push_back(Sample {
                        shape: record.shape,
                        at: now,
                        millis: elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
                    });
                state.entries += 1;
                state.bytes += SAMPLE_BYTES;
            }
        }
        drop(state);
        self.inner.changed.notify_one();
    }
}

fn cache_usage(state: &mut State, id: u64, usage: &bitrouter_ai::types::Usage) {
    let Some(record) = state.records.get(&id) else {
        return;
    };
    if !matches!(record.witness.load(Ordering::Acquire), SENDING | UNKNOWN) {
        return;
    }
    if let (Some(index), Some(binding), Some(cached)) =
        (&mut state.prefix, &record.prefix, prefix::cached(usage))
    {
        index.observe(id, binding, prefix::Event::Usage(cached), Instant::now());
    }
}

fn reap(state: &mut State, now: Instant, uncertainty_ttl: Duration) {
    if let Some(index) = &mut state.prefix {
        index.reap(now);
    }
    let due: Vec<_> = state
        .records
        .iter()
        .filter_map(|(id, r)| {
            r.expires
                .map_or(r.deadline <= now, |at| at <= now)
                .then_some(*id)
        })
        .collect();
    for id in due {
        let Some(mut record) = state.records.remove(&id) else {
            continue;
        };
        if let (Some(index), Some(binding)) = (&mut state.prefix, &record.prefix) {
            let event = if record.witness.load(Ordering::Acquire) == RESERVED {
                prefix::Event::Removed
            } else {
                prefix::Event::Unknown
            };
            index.observe(id, binding, event, now);
        }
        let d = &mut state.destinations[record.candidate.destination];
        match record.witness.load(Ordering::Acquire) {
            RESERVED => {
                d.reserved -= 1;
                d.counters.unsent = d.counters.unsent.saturating_add(1);
                record.witness.store(UNSENT, Ordering::Release);
            }
            SENDING | UNKNOWN => {
                if record.witness.load(Ordering::Acquire) == SENDING {
                    d.sending -= 1;
                    d.uncertain += 1;
                    d.counters.unknown = d.counters.unknown.saturating_add(1);
                    d.counters.timeouts = d.counters.timeouts.saturating_add(1);
                    record.witness.store(UNKNOWN, Ordering::Release);
                    record.expires = Some(record.deadline + uncertainty_ttl);
                    if record.expires.is_some_and(|at| at > now) {
                        state.records.insert(id, record);
                        continue;
                    }
                }
                d.uncertain -= 1;
                d.counters.expired = d.counters.expired.saturating_add(1);
                record.witness.store(EXPIRED, Ordering::Release);
            }
            _ => {
                state.records.insert(id, record);
                continue;
            }
        }
        d.work -= record.work;
        state.outstanding -= 1;
        state.entries -= 1;
        state.bytes -= ATTEMPT_BYTES
            + record
                .prefix
                .as_ref()
                .map_or(0, prefix::Binding::retained_bytes);
    }
    for d in &mut state.destinations {
        while d
            .samples
            .front()
            .is_some_and(|sample| now.duration_since(sample.at) >= SAMPLE_TTL)
        {
            d.samples.pop_front();
            state.entries -= 1;
            state.bytes -= SAMPLE_BYTES;
        }
    }
}

fn validate(
    inner: &Inner,
    state: &State,
    candidate: CandidateId,
    target: &RoutingTarget,
    work: u64,
) -> Result<()> {
    if candidate.instance != inner.instance
        || candidate.generation == 0
        || candidate.ordinal >= 4096
        || !candidate.matches(target)
        || !(256..=131072).contains(&work)
        || !state
            .destinations
            .get(candidate.destination)
            .is_some_and(|d| maps(&d.config, target))
    {
        return Err(unavailable());
    }
    Ok(())
}

fn reserve(
    inner: &Arc<Inner>,
    state: &mut State,
    candidate: CandidateId,
    work: u64,
    shape: Shape,
    deadline: Instant,
    mut prefix: Option<prefix::Binding>,
) -> Result<Reservation> {
    if candidate.instance != inner.instance
        || candidate.generation == 0
        || candidate.ordinal >= 4096
        || !(256..=131072).contains(&work)
    {
        return Err(unavailable());
    }
    let now = Instant::now();
    if now >= deadline {
        return Err(BitrouterError::UpstreamTimeout);
    }
    if state.closed {
        return Err(unavailable());
    }
    if state.outstanding >= inner.limits.max_active
        || state.outstanding >= inner.limits.max_uncertain
    {
        return Err(saturated());
    }
    let d = state
        .destinations
        .get(candidate.destination)
        .ok_or_else(unavailable)?;
    if d.reserved + d.sending + d.uncertain >= d.config.max_active {
        return Err(saturated());
    }
    let work_sum = d.work.checked_add(work).ok_or_else(unavailable)?;
    if state
        .entries
        .checked_add(2)
        .is_none_or(|n| n > inner.limits.max_state_entries)
        || state.bytes + ATTEMPT_BYTES + OWNER_BYTES > u64::from(inner.limits.max_state_bytes)
    {
        return Err(unavailable());
    }
    if prefix.as_ref().is_some_and(|binding| {
        state.bytes + ATTEMPT_BYTES + OWNER_BYTES + binding.retained_bytes()
            > u64::from(inner.limits.max_state_bytes)
    }) {
        prefix = None;
    }
    let prefix_bytes = prefix.as_ref().map_or(0, prefix::Binding::retained_bytes);
    let id = state.next.checked_add(1).ok_or_else(unavailable)?;
    let witness = Arc::new(AtomicU8::new(RESERVED));
    state.next = id;
    state.entries += 2;
    state.bytes += ATTEMPT_BYTES + OWNER_BYTES + prefix_bytes;
    if let (Some(index), Some(binding)) = (&mut state.prefix, &prefix) {
        index.observe(id, binding, prefix::Event::Reserved, now);
    }
    state.outstanding += 1;
    state.owners += 1;
    let d = &mut state.destinations[candidate.destination];
    d.reserved += 1;
    d.work = work_sum;
    state.records.insert(
        id,
        Record {
            candidate,
            work,
            shape,
            deadline,
            sent_at: None,
            status: None,
            first_output: None,
            expires: None,
            witness: witness.clone(),
            prefix,
        },
    );
    let reservation = Reservation {
        observation: Observation {
            inner: inner.clone(),
            id,
            candidate,
            witness,
            deadline,
        },
    };
    Ok(reservation)
}

fn maps(config: &DestinationConfig, target: &RoutingTarget) -> bool {
    config.provider == target.provider_name
        && config.model == target.service_id
        && &config.endpoint
            == target
                .api_base_override
                .as_ref()
                .unwrap_or(&target.api_base)
        && config.account == target.account_label
}

fn protocol(target: &RoutingTarget) -> Option<u8> {
    use bitrouter_ai::types::ApiProtocol;
    match target.api_protocol {
        ApiProtocol::ChatCompletions => Some(0),
        ApiProtocol::Messages => Some(1),
        ApiProtocol::Responses => Some(2),
        _ => None,
    }
}

fn unavailable() -> BitrouterError {
    BitrouterError::UpstreamUnavailable
}
fn saturated() -> BitrouterError {
    BitrouterError::RateLimited { retry_after: None }
}

#[cfg(test)]
mod tests;
