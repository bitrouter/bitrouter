//! Best-effort private prefix evidence. No execution ownership or raw input.
pub(crate) mod projection;
use crate::{
    config::inference_routing::PrefixConfig,
    gateway::{
        model::protocol,
        pipeline::{context::PipelineContext, types::RoutingTarget},
    },
};
use bitrouter_ai::types::{Usage, UsageOrigin};
use projection::{Digest, Projection, digest};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio::time::{Duration, Instant};

#[derive(Clone)]
pub(super) struct Binding {
    namespace: Digest,
    ttl_ms: u32,
    pub(super) input: Arc<Projection>,
}
impl Binding {
    pub(super) fn retained_bytes(&self) -> u64 {
        self.input.retained_bytes() + std::mem::size_of::<Self>() as u64 + 32
    }
}
pub(super) struct Projector {
    key: Digest,
    cap: usize,
    generation: u64,
    ttl_ms: u32,
}
impl Projector {
    pub(super) fn new(config: &PrefixConfig, generation: u64) -> Self {
        Self {
            key: rand::random(),
            cap: config.max_projection_bytes as usize,
            generation,
            ttl_ms: config.ttl_ms,
        }
    }
    pub(super) fn prepare(
        &self,
        chain: &[RoutingTarget],
        ctx: &PipelineContext,
    ) -> Vec<Option<Binding>> {
        let Some(prompt) = ctx.generation_prompt() else {
            return Vec::new();
        };
        if ctx.caller().is_anonymous() || ctx.caller().user_id().len() > 512 {
            return Vec::new();
        }
        let tenant = if let Some(execution) =
            ctx.extension::<crate::tenancy::execution::TenantExecution>()
        {
            digest(
                &self.key,
                &[
                    b"tenant",
                    execution.installation_id().as_str().as_bytes(),
                    b"\0",
                    execution.scheduling_tenant().as_bytes(),
                ],
            )
        } else if ctx.caller().is_local() {
            digest(&self.key, &[b"local"])
        } else {
            digest(&self.key, &[b"caller", ctx.caller().user_id().as_bytes()])
        };
        let inputs = [
            protocol::chat_completions::prefix_projection(prompt, &self.key, &tenant, self.cap),
            protocol::messages::prefix_projection(prompt, &self.key, &tenant, self.cap),
            protocol::responses::prefix_projection(prompt, &self.key, &tenant, self.cap),
        ];
        if inputs.iter().all(Option::is_none) {
            return Vec::new();
        }
        let bindings: Vec<_> = chain
            .iter()
            .map(|target| {
                let protocol = super::protocol(target)? as usize;
                if [
                    target.provider_name.len(),
                    target.service_id.len(),
                    target.effective_api_base().len(),
                    target.account_label.as_ref().map_or(0, String::len),
                ]
                .into_iter()
                .any(|n| n > self.cap)
                {
                    return None;
                }
                // Dynamic/custom header cache domains are not proven by v1.
                if !target.headers.is_empty() {
                    return None;
                }
                let credential = if let Some(execution) =
                    ctx.extension::<crate::tenancy::execution::TenantExecution>()
                {
                    let reference = execution.route().match_candidate(target).ok()?;
                    let encoded = serde_json::to_vec(&reference).ok()?;
                    digest(&self.key, &[b"retained-credential", &encoded])
                } else {
                    if target.effective_api_key().is_empty()
                        || target.effective_api_key().len() > 16384
                    {
                        return None;
                    }
                    digest(
                        &self.key,
                        &[
                            b"configured-credential",
                            target.effective_api_key().as_bytes(),
                        ],
                    )
                };
                let compatibility = bounded_json(
                    &(
                        &target.reasoning_effort,
                        &target.model_constraints,
                        target.chat_token_limit_field,
                        target.chat_supports_store,
                        target.chat_supports_stream_options,
                        target.chat_google_extensions,
                        target.auth_scheme,
                    ),
                    self.cap,
                )?;
                // Length-prefix every namespace component, including absent account.
                let mut mac = projection::mac(&self.key);
                use hmac::Mac;
                for part in [
                    b"bitrouter.cache-domain.v1".as_slice(),
                    &self.generation.to_be_bytes(),
                    &tenant,
                    &credential,
                    target.provider_name.as_bytes(),
                    target.service_id.as_bytes(),
                    target.effective_api_base().as_bytes(),
                    target.account_label.as_deref().unwrap_or("").as_bytes(),
                    &[protocol as u8],
                    &compatibility,
                ] {
                    mac.update(&(part.len() as u64).to_be_bytes());
                    mac.update(part);
                }
                Some(Binding {
                    namespace: mac.finalize().into_bytes().into(),
                    ttl_ms: self.ttl_ms,
                    input: inputs[protocol].clone()?,
                })
            })
            .collect();
        if bindings.iter().all(Option::is_none) {
            Vec::new()
        } else {
            bindings
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    namespace: Digest,
    block: Digest,
    owner: u64,
}
type Cached = (Option<u64>, Option<u64>);
#[derive(Clone, Copy)]
struct Evidence {
    touched: u64,
    tentative: Option<Instant>,
    sent: Option<Instant>,
    success: Option<Instant>,
    unknown: bool,
    cached: Option<Cached>,
    expires: Instant,
}
// Reserve root/edge/slack storage for three ordered trees as well as payload.
// These deliberately conservative logical charges are not allocator RSS.
const WIDTH: u64 = (std::mem::size_of::<(Key, Evidence)>()
    + std::mem::size_of::<(u64, Key)>()
    + std::mem::size_of::<(Instant, Key)>()) as u64;
const INDEX_BASE_BYTES: u64 = 16 * WIDTH + 512;
const ENTRY_BYTES: u64 = 3 * WIDTH + 384;
#[derive(Clone, Copy)]
pub(super) enum Event {
    Reserved,
    Sent,
    Succeeded,
    Unknown,
    Removed,
    Usage(Cached),
}
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct View {
    pub(crate) entries: usize,
    pub(crate) bytes: u64,
    pub(crate) tentative: usize,
    pub(crate) recent: usize,
    pub(crate) successful: usize,
    pub(crate) unknown: usize,
    pub(crate) cached_reports: usize,
    pub(crate) last_cached_tokens: Option<Cached>,
    pub(crate) matched_bytes: u64,
    pub(crate) projection_bytes: u64,
    pub(crate) affinity_millionths: u64,
}
pub(super) struct Index {
    config: PrefixConfig,
    entries: BTreeMap<Key, Evidence>,
    lru: BTreeSet<(u64, Key)>,
    deadlines: BTreeSet<(Instant, Key)>,
    tick: u64,
}
impl Index {
    pub(super) fn new(config: &PrefixConfig) -> Self {
        Self {
            config: config.clone(),
            entries: BTreeMap::new(),
            lru: BTreeSet::new(),
            deadlines: BTreeSet::new(),
            tick: 0,
        }
    }
    fn remove(&mut self, key: Key) {
        if let Some(value) = self.entries.remove(&key) {
            self.lru.remove(&(value.touched, key));
            self.deadlines.remove(&(value.expires, key));
            if self.entries.is_empty() {
                self.entries = BTreeMap::new();
                self.lru = BTreeSet::new();
                self.deadlines = BTreeSet::new();
            }
        }
    }
    pub(super) fn reap(&mut self, now: Instant) {
        while let Some((at, key)) = self.deadlines.first().copied() {
            if at > now {
                break;
            }
            self.remove(key);
        }
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadlines.first().map(|(at, _)| *at)
    }
    pub(super) fn observe(&mut self, owner: u64, binding: &Binding, event: Event, now: Instant) {
        self.reap(now);
        let ttl = Duration::from_millis(u64::from(binding.ttl_ms));
        for block in &binding.input.blocks {
            let key = Key {
                namespace: binding.namespace,
                block: *block,
                owner,
            };
            let existing = self.entries.get(&key).copied();
            self.remove(key);
            if matches!(event, Event::Removed) {
                continue;
            }
            let mut value = existing.unwrap_or(Evidence {
                touched: 0,
                tentative: None,
                sent: None,
                success: None,
                unknown: false,
                cached: None,
                expires: now + ttl,
            });
            match event {
                Event::Reserved => {
                    value.tentative = Some(now);
                    value.expires = now + ttl.min(Duration::from_secs(1));
                }
                Event::Sent => {
                    value.tentative = None;
                    value.sent = Some(now);
                    value.expires = now + ttl;
                }
                Event::Succeeded => {
                    value.tentative = None;
                    value.success = Some(now);
                    value.unknown = false;
                    value.expires = now + ttl;
                }
                Event::Unknown => {
                    value.tentative = None;
                    value.unknown = true;
                }
                Event::Usage(cached) => {
                    value.cached = Some(cached);
                }
                Event::Removed => {}
            }
            if value.expires <= now {
                continue;
            }
            let Some(tick) = self.tick.checked_add(1) else {
                continue;
            };
            self.tick = tick;
            value.touched = tick;
            // A full/very small index loses advisory evidence, never ownership.
            if INDEX_BASE_BYTES + ENTRY_BYTES > u64::from(self.config.max_bytes) {
                continue;
            }
            while self.entries.len() >= self.config.max_entries as usize
                || INDEX_BASE_BYTES + (self.entries.len() as u64 + 1) * ENTRY_BYTES
                    > u64::from(self.config.max_bytes)
            {
                let Some((_, oldest)) = self.lru.first().copied() else {
                    break;
                };
                self.remove(oldest);
            }
            self.entries.insert(key, value);
            self.lru.insert((tick, key));
            self.deadlines.insert((value.expires, key));
        }
    }
    pub(super) fn inspect(&self, binding: &Binding, exclude: Option<u64>, now: Instant) -> View {
        let mut view = View {
            entries: self.entries.len(),
            bytes: if self.entries.is_empty() {
                0
            } else {
                INDEX_BASE_BYTES + self.entries.len() as u64 * ENTRY_BYTES
            },
            projection_bytes: binding.input.bytes,
            ..Default::default()
        };
        let mut latest_usage = 0;
        let scope_start = Key {
            namespace: binding.namespace,
            block: [0; 32],
            owner: 0,
        };
        let scope_end = Key {
            namespace: binding.namespace,
            block: [255; 32],
            owner: u64::MAX,
        };
        for (_, value) in self
            .entries
            .range(scope_start..=scope_end)
            .filter(|(_, v)| v.expires > now)
        {
            if value.cached.is_some() && value.touched > latest_usage {
                view.last_cached_tokens = value.cached;
                latest_usage = value.touched;
            }
            view.tentative += usize::from(value.tentative.is_some());
            view.recent += usize::from(value.sent.is_some());
            view.successful += usize::from(value.success.is_some());
            view.unknown += usize::from(value.unknown);
            view.cached_reports += usize::from(value.cached.is_some());
        }
        let matched = self.matching(binding, exclude, now);
        view.matched_bytes = matched.matched_bytes;
        view.affinity_millionths = matched.affinity_millionths;
        view
    }
    pub(super) fn affinity(&self, binding: &Binding, now: Instant) -> u64 {
        self.matching(binding, None, now).affinity_millionths
    }
    fn matching(&self, binding: &Binding, exclude: Option<u64>, now: Instant) -> View {
        let mut view = View::default();
        let ttl = Duration::from_millis(u64::from(binding.ttl_ms)).as_nanos();
        let mut contiguous = 1_000_000u64;
        for block in &binding.input.blocks {
            let start = Key {
                namespace: binding.namespace,
                block: *block,
                owner: 0,
            };
            let end = Key {
                owner: u64::MAX,
                ..start
            };
            let best = self
                .entries
                .range(start..=end)
                .filter(|(key, v)| Some(key.owner) != exclude && v.expires > now && !v.unknown)
                .filter_map(|(_, v)| {
                    v.success
                        .map(|at| (at, 750_000u128))
                        .or_else(|| v.sent.map(|at| (at, 250_000u128)))
                })
                .map(|(at, confidence)| {
                    let remaining =
                        ttl.saturating_sub(now.saturating_duration_since(at).as_nanos());
                    (confidence * remaining / ttl) as u64
                })
                .max()
                .unwrap_or(0);
            if best == 0 {
                break;
            }
            contiguous = contiguous.min(best);
            view.matched_bytes += 256;
            view.affinity_millionths = (u128::from(view.matched_bytes) * u128::from(contiguous)
                / u128::from(binding.input.bytes)) as u64;
        }
        view
    }
}

pub(super) fn cached(usage: &Usage) -> Option<Cached> {
    if usage.origin != UsageOrigin::ProviderReported || usage.normalized_buckets().is_err() {
        return None;
    }
    let counter = |canonical: u64, paths: &[&[&str]]| -> Option<u64> {
        let mut found = false;
        for path in paths {
            let value = path
                .iter()
                .try_fold(usage.raw.as_deref()?, |value, field| value.get(*field));
            if let Some(value) = value {
                found = true;
                if value.as_u64() != Some(canonical) {
                    return None;
                }
            }
        }
        (found || canonical > 0).then_some(canonical)
    };
    // A typed positive provider count is already explicit evidence. A default
    // zero without an explicit matching raw field remains unknown per counter.
    let read = if usage.raw.is_none() {
        (usage.cache_read_tokens > 0).then_some(usage.cache_read_tokens)
    } else {
        counter(
            usage.cache_read_tokens,
            &[
                &["cache_read_input_tokens"],
                &["prompt_tokens_details", "cached_tokens"],
                &["input_tokens_details", "cached_tokens"],
            ],
        )
    };
    let write = if usage.raw.is_none() {
        (usage.cache_write_tokens > 0).then_some(usage.cache_write_tokens)
    } else {
        counter(
            usage.cache_write_tokens,
            &[
                &["cache_creation_input_tokens"],
                &["prompt_tokens_details", "cache_write_tokens"],
                &["input_tokens_details", "cache_write_tokens"],
            ],
        )
    };
    (read.is_some() || write.is_some()).then_some((read, write))
}

pub(super) fn prepared_bytes(bindings: &Vec<Option<Binding>>) -> u64 {
    let mut bytes = (bindings.capacity() * std::mem::size_of::<Option<Binding>>()) as u64;
    let mut seen = [false; 3];
    for binding in bindings.iter().flatten() {
        if !seen[binding.input.protocol] {
            bytes += binding.input.retained_bytes();
            seen[binding.input.protocol] = true;
        }
    }
    bytes
}

fn bounded_json(value: &impl serde::Serialize, cap: usize) -> Option<Vec<u8>> {
    struct Limited {
        bytes: Vec<u8>,
        cap: usize,
    }
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.cap.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("prefix namespace work limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Limited {
        bytes: Vec::new(),
        cap,
    };
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.bytes)
}

#[cfg(test)]
mod tests;
