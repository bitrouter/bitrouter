//! TTL-cached [`Executor`] — wraps another executor with a per-server cache
//! for cheap list calls (`tools/list`, `resources/list`,
//! `resources/templates/list`, `prompts/list`).
//!
//! Non-list methods (`tools/call`, `resources/read`, `prompts/get`) and
//! aggregate targets pass straight through.
//!
//! Cache entries expire on TTL. When the inner executor is hooked up to a
//! `notifications/*_list_changed` source via
//! [`CachingExecutor::with_invalidation`], affected entries are also evicted
//! on demand. The TTL on each entry honours the MCP spec's SEP-2549
//! `ttlMs` / `cacheScope` cache-control hints when the upstream supplies them
//! — see `extract_cache_hint`.
//!
//! Caching applies per [`McpTarget::Direct`] member; when used inside an
//! [`super::aggregating_executor::AggregatingExecutor`], the cache key
//! includes the per-member server name so a cold aggregate fan-out warms
//! each leaf cache independently.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use tokio::sync::broadcast;

use super::skills::SKILLS_LIST_METHOD;
use super::{
    Executor, InvalidationEvent, InvalidationKind, McpRequest, McpResponse, McpStreamPart,
    McpTarget,
};
use crate::error::Result;

// Defaults for `CacheTtls` and the YAML-facing `McpCacheConfig`
// (`crates/bitrouter-sdk/src/config/mod.rs`) — single source of truth so a
// `mcp.cache:` block with all-defaults behaves identically to no
// `mcp.cache:` block at all.
const DEFAULT_TOOLS_LIST_TTL_SECS: u64 = 60;
const DEFAULT_RESOURCES_LIST_TTL_SECS: u64 = 60;
const DEFAULT_RESOURCES_TEMPLATES_LIST_TTL_SECS: u64 = 300;
const DEFAULT_PROMPTS_LIST_TTL_SECS: u64 = 300;
/// TTL for SEP-2640 `skills/list`. Not YAML-configurable while the extension
/// is a draft; the constant is the single source of truth until it is.
const DEFAULT_SKILLS_LIST_TTL_SECS: u64 = 60;
/// Default per-server LRU bound. Pub so the config layer can spell the same
/// number without re-declaring it.
pub const DEFAULT_MAX_ENTRIES_PER_SERVER: usize = 64;

/// Per-method cache TTLs. `Duration::ZERO` disables caching for that method.
#[derive(Debug, Clone)]
pub struct CacheTtls {
    /// TTL for `tools/list`.
    pub tools_list: Duration,
    /// TTL for `resources/list`.
    pub resources_list: Duration,
    /// TTL for `resources/templates/list`.
    pub resources_templates_list: Duration,
    /// TTL for `prompts/list`.
    pub prompts_list: Duration,
    /// TTL for SEP-2640 `skills/list`.
    ///
    /// `skills/get` is deliberately absent: the SEP specifies that a single
    /// entry "carries no pagination cursor and no list-caching attributes"
    /// because it is a point-in-time snapshot, so caching it would defeat the
    /// method's purpose (refreshing one skill's digests).
    pub skills_list: Duration,
    /// Max entries per server (LRU eviction safety bound).
    pub max_entries_per_server: usize,
}

impl Default for CacheTtls {
    fn default() -> Self {
        Self {
            tools_list: Duration::from_secs(DEFAULT_TOOLS_LIST_TTL_SECS),
            resources_list: Duration::from_secs(DEFAULT_RESOURCES_LIST_TTL_SECS),
            resources_templates_list: Duration::from_secs(
                DEFAULT_RESOURCES_TEMPLATES_LIST_TTL_SECS,
            ),
            prompts_list: Duration::from_secs(DEFAULT_PROMPTS_LIST_TTL_SECS),
            skills_list: Duration::from_secs(DEFAULT_SKILLS_LIST_TTL_SECS),
            max_entries_per_server: DEFAULT_MAX_ENTRIES_PER_SERVER,
        }
    }
}

#[cfg(feature = "config_file")]
impl From<&crate::config::McpCacheConfig> for CacheTtls {
    fn from(cfg: &crate::config::McpCacheConfig) -> Self {
        Self {
            tools_list: Duration::from_secs(cfg.tools_list_ttl_secs),
            resources_list: Duration::from_secs(cfg.resources_list_ttl_secs),
            resources_templates_list: Duration::from_secs(cfg.resources_templates_list_ttl_secs),
            prompts_list: Duration::from_secs(cfg.prompts_list_ttl_secs),
            // No YAML knob yet — see `DEFAULT_SKILLS_LIST_TTL_SECS`.
            skills_list: Duration::from_secs(DEFAULT_SKILLS_LIST_TTL_SECS),
            max_entries_per_server: cfg.max_entries_per_server,
        }
    }
}

impl CacheTtls {
    fn ttl_for(&self, method: &str) -> Option<Duration> {
        let d = match method {
            "tools/list" => self.tools_list,
            "resources/list" => self.resources_list,
            "resources/templates/list" => self.resources_templates_list,
            "prompts/list" => self.prompts_list,
            SKILLS_LIST_METHOD => self.skills_list,
            _ => return None,
        };
        if d.is_zero() { None } else { Some(d) }
    }
}

#[derive(Hash, Eq, PartialEq, Clone)]
struct CacheKey {
    server_name: String,
    method: &'static str,
    params_hash: u64,
}

#[derive(Clone)]
struct CacheEntry {
    value: serde_json::Value,
    inserted_at: Instant,
    ttl: Duration,
}

impl CacheEntry {
    fn is_fresh(&self, now: Instant) -> bool {
        now.duration_since(self.inserted_at) < self.ttl
    }
}

/// Per-server LRU + TTL cache.
struct ServerCache {
    entries: HashMap<CacheKey, CacheEntry>,
    /// Insertion order — popped from the front when over the size bound.
    order: VecDeque<CacheKey>,
    max_entries: usize,
}

impl ServerCache {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            max_entries,
        }
    }

    fn get(&self, key: &CacheKey, now: Instant) -> Option<serde_json::Value> {
        self.entries
            .get(key)
            .filter(|e| e.is_fresh(now))
            .map(|e| e.value.clone())
    }

    fn insert(&mut self, key: CacheKey, entry: CacheEntry) {
        if !self.entries.contains_key(&key) {
            self.order.push_back(key.clone());
        }
        self.entries.insert(key, entry);
        while self.entries.len() > self.max_entries {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }

    fn evict_method(&mut self, method: &'static str) {
        self.entries.retain(|k, _| k.method != method);
        self.order.retain(|k| k.method != method);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

/// Wrap an inner [`Executor`] with a TTL cache for list-shaped methods.
pub struct CachingExecutor<E: Executor> {
    inner: Arc<E>,
    ttls: CacheTtls,
    caches: Arc<Mutex<HashMap<String, ServerCache>>>,
    /// `Some` once [`with_invalidation`] has spawned the receiver loop. Held so
    /// `Drop` can `abort()` it — otherwise the task would outlive the
    /// executor whenever the broadcast sender (typically on the inner
    /// `RmcpExecutor`) lives longer than this wrapper.
    invalidation_task: Option<tokio::task::JoinHandle<()>>,
}

impl<E: Executor + 'static> CachingExecutor<E> {
    /// Build a cache around `inner` with the per-method TTLs in `ttls`.
    pub fn new(inner: Arc<E>, ttls: CacheTtls) -> Self {
        Self {
            inner,
            ttls,
            caches: Arc::new(Mutex::new(HashMap::new())),
            invalidation_task: None,
        }
    }

    /// Subscribe the cache to an [`InvalidationEvent`] stream — typically
    /// [`super::rmcp_executor::RmcpExecutor::invalidation_receiver`]. Returns
    /// `self` so the builder reads naturally.
    pub fn with_invalidation(mut self, mut rx: broadcast::Receiver<InvalidationEvent>) -> Self {
        let caches = self.caches.clone();
        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => apply_invalidation(&caches, &event),
                    // Receiver closed — nothing more will come.
                    Err(broadcast::error::RecvError::Closed) => break,
                    // Lagged — invalidate everything we know about to stay
                    // safe (silent stale data is worse than a fresh re-fetch).
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Ok(mut map) = caches.lock() {
                            for sc in map.values_mut() {
                                sc.clear();
                            }
                        }
                    }
                }
            }
        });
        self.invalidation_task = Some(handle);
        self
    }

    fn cache_lookup(&self, key: &CacheKey) -> Option<serde_json::Value> {
        let now = Instant::now();
        let map = self.caches.lock().ok()?;
        map.get(&key.server_name).and_then(|sc| sc.get(key, now))
    }

    fn cache_insert(&self, key: CacheKey, value: serde_json::Value, ttl: Duration) {
        let Ok(mut map) = self.caches.lock() else {
            return;
        };
        let sc = map
            .entry(key.server_name.clone())
            .or_insert_with(|| ServerCache::new(self.ttls.max_entries_per_server));
        sc.insert(
            key,
            CacheEntry {
                value,
                inserted_at: Instant::now(),
                ttl,
            },
        );
    }
}

impl<E: Executor> Drop for CachingExecutor<E> {
    fn drop(&mut self) {
        if let Some(handle) = self.invalidation_task.take() {
            handle.abort();
        }
    }
}

fn apply_invalidation(
    caches: &Arc<Mutex<HashMap<String, ServerCache>>>,
    event: &InvalidationEvent,
) {
    let Ok(mut map) = caches.lock() else {
        return;
    };
    let Some(sc) = map.get_mut(&event.server_name) else {
        return;
    };
    match event.kind {
        InvalidationKind::ToolsListChanged => sc.evict_method("tools/list"),
        InvalidationKind::ResourcesListChanged => {
            sc.evict_method("resources/list");
            sc.evict_method("resources/templates/list");
        }
        InvalidationKind::PromptsListChanged => sc.evict_method("prompts/list"),
        InvalidationKind::Reinitialized => sc.clear(),
    }
}

/// Identify which method the executor will cache for, if any. The TTL to stamp
/// on the entry comes from [`extract_cache_hint`].
///
/// `skills/list` is here for a **correctness** reason, not just for speed. The
/// cache key is `{server_name, method, params_hash}` with no caller identity,
/// so any cached entry is visible to every downstream caller. Routing
/// `skills/list` through this path is what subjects it to
/// [`extract_cache_hint`], which declines to cache a `cacheScope: private`
/// result. A relayed extension method that bypassed this function would skip
/// that check and could serve one tenant's private skill catalog to another.
fn cached_method(method: &str) -> Option<&'static str> {
    match method {
        "tools/list" => Some("tools/list"),
        "resources/list" => Some("resources/list"),
        "resources/templates/list" => Some("resources/templates/list"),
        "prompts/list" => Some("prompts/list"),
        SKILLS_LIST_METHOD => Some(SKILLS_LIST_METHOD),
        _ => None,
    }
}

/// What an upstream's SEP-2549 cache-control hint tells us to do with a result.
enum CacheHint {
    /// Do not cache at all. Either the upstream scoped the result `private`, or
    /// it gave a TTL of zero (spec: "immediately stale").
    Uncacheable,
    /// Cache it. `Some(ttl)` when the upstream supplied one, `None` to fall
    /// back to our configured per-method default.
    Cacheable(Option<Duration>),
}

/// Read the SEP-2549 cache-control hint off an upstream result.
///
/// `ttlMs` and `cacheScope` are **top-level fields on the result**, siblings of
/// `_meta` — not nested inside it (MCP `2026-07-28` schema; rmcp models them on
/// `ListToolsResult`/`ReadResourceResult` et al). Servers that shipped the
/// earlier draft put `ttlMs` under `_meta`, so that location is still honoured
/// as a fallback.
///
/// `cacheScope: "private"` means only the requesting user's client may cache
/// the response. Our cache sits behind a connection pool keyed by server name
/// alone, so a cached entry is visible to *every* downstream caller — which is
/// exactly what `private` forbids. We therefore decline to cache those results
/// rather than trying to partition them.
///
/// Per spec a negative `ttlMs` is treated as `0` rather than an error, matching
/// rmcp's `deserialize_ttl_ms`.
fn extract_cache_hint(result: &serde_json::Value) -> CacheHint {
    if result.get("cacheScope").and_then(|v| v.as_str()) == Some("private") {
        return CacheHint::Uncacheable;
    }
    let raw = result
        .get("ttlMs")
        .or_else(|| result.get("_meta").and_then(|m| m.get("ttlMs")))
        .and_then(|v| v.as_i64());
    match raw {
        // Negative clamps to zero, and zero means immediately stale.
        Some(ms) if ms <= 0 => CacheHint::Uncacheable,
        Some(ms) => CacheHint::Cacheable(Some(Duration::from_millis(ms as u64))),
        None => CacheHint::Cacheable(None),
    }
}

fn params_hash(params: &serde_json::Value) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let mut h = DefaultHasher::new();
    hash_value(params, &mut h);
    h.finish()
}

/// Stable, key-order-independent hash of a JSON value. Object entries are
/// sorted lexicographically before hashing so two semantically equal payloads
/// (`{"a":1,"b":2}` and `{"b":2,"a":1}`) collide on the same cache key
/// regardless of whether `serde_json`'s `preserve_order` feature is enabled
/// anywhere in the workspace (the feature is additive — any transitive dep
/// can flip the default `Map` from `BTreeMap` to `IndexMap`).
///
/// A per-variant discriminator byte and a length prefix on collections keep
/// the hash unambiguous across types (`null` vs `"null"`, `[1,2]` vs `[1,[2]]`).
fn hash_value<H: std::hash::Hasher>(value: &serde_json::Value, hasher: &mut H) {
    use std::hash::Hash;
    match value {
        serde_json::Value::Null => 0u8.hash(hasher),
        serde_json::Value::Bool(b) => {
            1u8.hash(hasher);
            b.hash(hasher);
        }
        serde_json::Value::Number(n) => {
            2u8.hash(hasher);
            n.to_string().hash(hasher);
        }
        serde_json::Value::String(s) => {
            3u8.hash(hasher);
            s.hash(hasher);
        }
        serde_json::Value::Array(arr) => {
            4u8.hash(hasher);
            (arr.len() as u64).hash(hasher);
            for v in arr {
                hash_value(v, hasher);
            }
        }
        serde_json::Value::Object(obj) => {
            5u8.hash(hasher);
            let mut entries: Vec<(&String, &serde_json::Value)> = obj.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            (entries.len() as u64).hash(hasher);
            for (k, v) in entries {
                k.hash(hasher);
                hash_value(v, hasher);
            }
        }
    }
}

#[async_trait]
impl<E: Executor + 'static> Executor for CachingExecutor<E> {
    async fn execute(&self, target: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
        let (server_name, method) = match (target, cached_method(&request.method)) {
            (McpTarget::Direct { server_name, .. }, Some(m)) => (server_name.clone(), m),
            // Aggregate or non-cacheable method — pass straight through.
            _ => return self.inner.execute(target, request).await,
        };
        let default_ttl = match self.ttls.ttl_for(method) {
            Some(d) => d,
            None => return self.inner.execute(target, request).await,
        };
        let key = CacheKey {
            server_name,
            method,
            params_hash: params_hash(&request.params),
        };
        if let Some(value) = self.cache_lookup(&key) {
            tracing::debug!(
                server = %key.server_name,
                method = %key.method,
                "mcp cache: hit",
            );
            return Ok(McpResponse {
                request_id: request.request_id.clone(),
                result: value,
            });
        }
        tracing::debug!(
            server = %key.server_name,
            method = %key.method,
            "mcp cache: miss",
        );
        let response = self.inner.execute(target, request).await?;
        match extract_cache_hint(&response.result) {
            CacheHint::Uncacheable => {
                tracing::debug!(
                    server = %key.server_name,
                    method = %key.method,
                    "mcp cache: upstream declined caching (private scope or zero ttl)",
                );
            }
            CacheHint::Cacheable(hint) => {
                self.cache_insert(key, response.result.clone(), hint.unwrap_or(default_ttl));
            }
        }
        Ok(response)
    }

    async fn execute_streaming(
        &self,
        target: &McpTarget,
        request: &McpRequest,
    ) -> Result<BoxStream<'static, Result<McpStreamPart>>> {
        // For cacheable list methods, serve from cache as a single-element
        // stream without going upstream when fresh. Non-cacheable methods
        // delegate so progress notifications pass through.
        let cacheable = match (target, cached_method(&request.method)) {
            (McpTarget::Direct { server_name, .. }, Some(method)) => self
                .ttls
                .ttl_for(method)
                .map(|ttl| (server_name.clone(), method, ttl)),
            _ => None,
        };
        let Some((server_name, method, default_ttl)) = cacheable else {
            return self.inner.execute_streaming(target, request).await;
        };

        let key = CacheKey {
            server_name,
            method,
            params_hash: params_hash(&request.params),
        };
        if let Some(value) = self.cache_lookup(&key) {
            let response = McpResponse {
                request_id: request.request_id.clone(),
                result: value,
            };
            return Ok(stream::once(async move { Ok(McpStreamPart::Final(response)) }).boxed());
        }

        // Miss — proxy the inner stream, but stamp the cache when the
        // terminal `Final` frame arrives so subsequent SSE requests for the
        // same list become hits.
        let inner_stream = self.inner.execute_streaming(target, request).await?;
        let caches = self.caches.clone();
        let max_entries = self.ttls.max_entries_per_server;
        let key_for_stream = key.clone();
        let wrapped = inner_stream.map(move |item| {
            if let Ok(McpStreamPart::Final(ref response)) = item
                && let CacheHint::Cacheable(hint) = extract_cache_hint(&response.result)
                && let Ok(mut map) = caches.lock()
            {
                let sc = map
                    .entry(key_for_stream.server_name.clone())
                    .or_insert_with(|| ServerCache::new(max_entries));
                sc.insert(
                    key_for_stream.clone(),
                    CacheEntry {
                        value: response.result.clone(),
                        inserted_at: Instant::now(),
                        ttl: hint.unwrap_or(default_ttl),
                    },
                );
            }
            item
        });
        Ok(wrapped.boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::mcp::transport::McpTransport;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingExecutor {
        calls: AtomicUsize,
        value: serde_json::Value,
    }

    #[async_trait]
    impl Executor for CountingExecutor {
        async fn execute(&self, _t: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(McpResponse {
                request_id: request.request_id.clone(),
                result: self.value.clone(),
            })
        }
    }

    fn target(name: &str) -> McpTarget {
        McpTarget::Direct {
            server_name: name.into(),
            transport: McpTransport::Stdio {
                command: "/bin/true".into(),
                args: vec![],
                env: HashMap::new(),
            },
        }
    }

    fn list_req(server: &str, method: &str) -> McpRequest {
        McpRequest::direct(
            server,
            method,
            serde_json::json!({}),
            CallerContext::new("k", "u"),
        )
    }

    #[tokio::test]
    async fn second_tools_list_within_ttl_is_a_cache_hit() {
        let inner = Arc::new(CountingExecutor {
            calls: AtomicUsize::new(0),
            value: serde_json::json!({"tools": []}),
        });
        let exec = CachingExecutor::new(inner.clone(), CacheTtls::default());
        let _ = exec
            .execute(&target("a"), &list_req("a", "tools/list"))
            .await
            .unwrap();
        let _ = exec
            .execute(&target("a"), &list_req("a", "tools/list"))
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ttl_zero_disables_caching() {
        let inner = Arc::new(CountingExecutor {
            calls: AtomicUsize::new(0),
            value: serde_json::json!({"tools": []}),
        });
        let ttls = CacheTtls {
            tools_list: Duration::ZERO,
            ..CacheTtls::default()
        };
        let exec = CachingExecutor::new(inner.clone(), ttls);
        let _ = exec
            .execute(&target("a"), &list_req("a", "tools/list"))
            .await
            .unwrap();
        let _ = exec
            .execute(&target("a"), &list_req("a", "tools/list"))
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn invalidation_evicts_affected_method() {
        let inner = Arc::new(CountingExecutor {
            calls: AtomicUsize::new(0),
            value: serde_json::json!({"tools": []}),
        });
        let (tx, rx) = broadcast::channel(8);
        let exec = CachingExecutor::new(inner.clone(), CacheTtls::default()).with_invalidation(rx);
        let _ = exec
            .execute(&target("a"), &list_req("a", "tools/list"))
            .await
            .unwrap();
        // Warm cache.
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        tx.send(InvalidationEvent {
            server_name: "a".into(),
            kind: InvalidationKind::ToolsListChanged,
        })
        .unwrap();
        // Give the spawned receiver task a chance to drain.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = exec
            .execute(&target("a"), &list_req("a", "tools/list"))
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn non_cacheable_method_passes_through() {
        let inner = Arc::new(CountingExecutor {
            calls: AtomicUsize::new(0),
            value: serde_json::json!({"ok": true}),
        });
        let exec = CachingExecutor::new(inner.clone(), CacheTtls::default());
        let _ = exec
            .execute(&target("a"), &list_req("a", "tools/call"))
            .await
            .unwrap();
        let _ = exec
            .execute(&target("a"), &list_req("a", "tools/call"))
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn aggregate_target_passes_through() {
        let inner = Arc::new(CountingExecutor {
            calls: AtomicUsize::new(0),
            value: serde_json::json!({"tools": []}),
        });
        let exec = CachingExecutor::new(inner.clone(), CacheTtls::default());
        let target = McpTarget::Aggregate { members: vec![] };
        let _ = exec
            .execute(&target, &list_req("anything", "tools/list"))
            .await
            .unwrap();
        let _ = exec
            .execute(&target, &list_req("anything", "tools/list"))
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }

    #[cfg(feature = "config_file")]
    #[test]
    fn mcp_cache_config_defaults_match_cache_ttls() {
        // The two Default impls (config-layer YAML shape vs. runtime cache
        // type) MUST agree so a `mcp.cache:` block with all-defaults behaves
        // identically to no `mcp.cache:` block at all.
        let cfg = crate::config::McpCacheConfig::default();
        let derived: CacheTtls = (&cfg).into();
        let coded = CacheTtls::default();
        assert_eq!(derived.tools_list, coded.tools_list);
        assert_eq!(derived.resources_list, coded.resources_list);
        assert_eq!(
            derived.resources_templates_list,
            coded.resources_templates_list
        );
        assert_eq!(derived.prompts_list, coded.prompts_list);
        assert_eq!(derived.max_entries_per_server, coded.max_entries_per_server);
    }

    #[test]
    fn params_hash_is_canonical_across_key_order() {
        // Two semantically-equal params with different key insertion order
        // must hash identically — otherwise `resources/list` with a
        // `{cursor, filter}` shape would suffer surprising cache misses
        // depending on how the inbound client built the JSON.
        let a = serde_json::json!({"a": 1, "b": {"x": [1, 2], "y": "z"}});
        let b = serde_json::json!({"b": {"y": "z", "x": [1, 2]}, "a": 1});
        assert_eq!(params_hash(&a), params_hash(&b));
        // Different values must still differ.
        let c = serde_json::json!({"a": 1, "b": {"y": "z", "x": [1, 3]}});
        assert_ne!(params_hash(&a), params_hash(&c));
        // Type discrimination: `null` vs the string "null".
        assert_ne!(
            params_hash(&serde_json::Value::Null),
            params_hash(&serde_json::json!("null"))
        );
    }

    /// SEP-2549 puts `ttlMs` at the *top level* of the result, beside `_meta`
    /// rather than inside it. Reading the wrong location is silent — the cache
    /// just falls back to its configured default — so pin both spellings.
    #[test]
    fn cache_hint_reads_top_level_ttl_ms_and_falls_back_to_meta() {
        let ttl = |v: serde_json::Value| match extract_cache_hint(&v) {
            CacheHint::Cacheable(hint) => hint,
            CacheHint::Uncacheable => panic!("expected cacheable, got uncacheable"),
        };

        // The shipped SEP-2549 shape.
        assert_eq!(
            ttl(serde_json::json!({ "tools": [], "ttlMs": 50 })),
            Some(Duration::from_millis(50)),
        );
        // The earlier draft shape, still honoured as a fallback.
        assert_eq!(
            ttl(serde_json::json!({ "tools": [], "_meta": { "ttlMs": 50 } })),
            Some(Duration::from_millis(50)),
        );
        // Top level wins when a server somehow sends both.
        assert_eq!(
            ttl(serde_json::json!({
                "ttlMs": 50,
                "_meta": { "ttlMs": 9000 }
            })),
            Some(Duration::from_millis(50)),
        );
        // No hint at all → fall back to the configured default.
        assert_eq!(ttl(serde_json::json!({ "tools": [] })), None);
    }

    #[test]
    fn cache_hint_declines_private_scope_and_non_positive_ttl() {
        let uncacheable =
            |v: serde_json::Value| matches!(extract_cache_hint(&v), CacheHint::Uncacheable);

        // `private` means only the requesting user's client may cache. Our
        // cache is shared across every downstream caller, so we must not.
        assert!(uncacheable(
            serde_json::json!({ "tools": [], "cacheScope": "private" })
        ));
        // ...even when the upstream also supplies a generous TTL.
        assert!(uncacheable(serde_json::json!({
            "cacheScope": "private",
            "ttlMs": 60_000
        })));
        // `public` is the default and stays cacheable.
        assert!(!uncacheable(
            serde_json::json!({ "tools": [], "cacheScope": "public" })
        ));
        // Zero means immediately stale.
        assert!(uncacheable(serde_json::json!({ "ttlMs": 0 })));
        // Per spec a negative value is clamped to zero, not an error.
        assert!(uncacheable(serde_json::json!({ "ttlMs": -42 })));
    }

    #[tokio::test]
    async fn private_scoped_result_is_not_cached() {
        let inner = Arc::new(CountingExecutor {
            calls: AtomicUsize::new(0),
            value: serde_json::json!({ "tools": [], "cacheScope": "private" }),
        });
        let exec = CachingExecutor::new(inner.clone(), CacheTtls::default());
        for _ in 0..2 {
            let _ = exec
                .execute(&target("a"), &list_req("a", "tools/list"))
                .await
                .unwrap();
        }
        // Both calls went upstream — no entry was ever stamped.
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }

    /// SEP-2640 `skills/list` is relayed through the gateway as an extension
    /// method. It must still be subject to the cache-scope check: the cache
    /// key carries no caller identity, so a cached private catalog would be
    /// served to every downstream caller of the daemon.
    #[tokio::test]
    async fn private_scoped_skills_list_is_not_cached() {
        let inner = Arc::new(CountingExecutor {
            calls: AtomicUsize::new(0),
            value: serde_json::json!({ "skills": [], "cacheScope": "private" }),
        });
        let exec = CachingExecutor::new(inner.clone(), CacheTtls::default());
        for _ in 0..2 {
            let _ = exec
                .execute(&target("a"), &list_req("a", "skills/list"))
                .await
                .unwrap();
        }
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            2,
            "a private skills catalog must never be served from the shared cache"
        );
    }

    #[tokio::test]
    async fn public_skills_list_is_cached() {
        let inner = Arc::new(CountingExecutor {
            calls: AtomicUsize::new(0),
            value: serde_json::json!({ "skills": [] }),
        });
        let exec = CachingExecutor::new(inner.clone(), CacheTtls::default());
        for _ in 0..2 {
            let _ = exec
                .execute(&target("a"), &list_req("a", "skills/list"))
                .await
                .unwrap();
        }
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            1,
            "second call is a hit"
        );
    }

    /// `skills/get` is a point-in-time snapshot the SEP explicitly exempts
    /// from list caching — it is how a host refreshes one skill's digests, so
    /// caching it would defeat the method.
    #[tokio::test]
    async fn skills_get_is_never_cached() {
        let inner = Arc::new(CountingExecutor {
            calls: AtomicUsize::new(0),
            value: serde_json::json!({ "skill": {} }),
        });
        let exec = CachingExecutor::new(inner.clone(), CacheTtls::default());
        for _ in 0..2 {
            let _ = exec
                .execute(&target("a"), &list_req("a", "skills/get"))
                .await
                .unwrap();
        }
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn upstream_ttl_hint_is_honoured() {
        let inner = Arc::new(CountingExecutor {
            calls: AtomicUsize::new(0),
            value: serde_json::json!({
                "tools": [],
                "ttlMs": 50
            }),
        });
        let exec = CachingExecutor::new(inner.clone(), CacheTtls::default());
        let _ = exec
            .execute(&target("a"), &list_req("a", "tools/list"))
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        // First call cached with 50ms TTL — wait past it and confirm refetch.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = exec
            .execute(&target("a"), &list_req("a", "tools/list"))
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }
}
