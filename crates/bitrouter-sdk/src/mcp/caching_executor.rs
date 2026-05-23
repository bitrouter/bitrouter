//! TTL-cached [`Executor`](super::Executor) — wraps another executor with a
//! per-server cache for cheap list calls (`tools/list`, `resources/list`,
//! `resources/templates/list`, `prompts/list`).
//!
//! Non-list methods (`tools/call`, `resources/read`, `prompts/get`) and
//! aggregate targets pass straight through.
//!
//! Cache entries expire on TTL. When the inner executor is hooked up to a
//! `notifications/*_list_changed` source via [`with_invalidation`], affected
//! entries are also evicted on demand. The TTL on each entry honours the MCP
//! spec's `ttlMs` cache-control hint when present in the upstream `_meta`.
//!
//! Caching applies per [`McpTarget::Direct`] member; when used inside an
//! [`super::AggregatingExecutor`], the cache key includes the per-member
//! server name so a cold aggregate fan-out warms each leaf cache
//! independently.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use tokio::sync::broadcast;

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
}

impl<E: Executor + 'static> CachingExecutor<E> {
    /// Build a cache around `inner` with the per-method TTLs in `ttls`.
    pub fn new(inner: Arc<E>, ttls: CacheTtls) -> Self {
        Self {
            inner,
            ttls,
            caches: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Subscribe the cache to an [`InvalidationEvent`] stream — typically
    /// [`super::RmcpExecutor::invalidation_receiver`]. Returns `self` so the
    /// builder reads naturally.
    pub fn with_invalidation(self, mut rx: broadcast::Receiver<InvalidationEvent>) -> Self {
        let caches = self.caches.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => apply_invalidation(&caches, &event),
                    // Receiver closed — nothing more will come.
                    Err(broadcast::error::RecvError::Closed) => break,
                    // Lagged — invalidate everything we know about to stay
                    // safe (silent stale data is worse than a fresh re-fetch).
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Ok(mut map) = caches.lock() {
                            for (_, sc) in map.iter_mut() {
                                sc.clear();
                            }
                        }
                    }
                }
            }
        });
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

/// Identify which method the executor will cache for, if any, and the TTL to
/// stamp on the entry. Honours the MCP spec's `_meta.ttlMs` cache-control
/// hint when present (`upstream_hint`).
fn cached_method(method: &str) -> Option<&'static str> {
    match method {
        "tools/list" => Some("tools/list"),
        "resources/list" => Some("resources/list"),
        "resources/templates/list" => Some("resources/templates/list"),
        "prompts/list" => Some("prompts/list"),
        _ => None,
    }
}

fn extract_ttl_hint(result: &serde_json::Value) -> Option<Duration> {
    let ms = result
        .get("_meta")
        .and_then(|m| m.get("ttlMs"))
        .and_then(|v| v.as_u64())?;
    Some(Duration::from_millis(ms))
}

fn params_hash(params: &serde_json::Value) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let s = serde_json::to_string(params).unwrap_or_default();
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
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
        let ttl = extract_ttl_hint(&response.result).unwrap_or(default_ttl);
        self.cache_insert(key, response.result.clone(), ttl);
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
            if let Ok(McpStreamPart::Final(ref response)) = item {
                let ttl = extract_ttl_hint(&response.result).unwrap_or(default_ttl);
                if let Ok(mut map) = caches.lock() {
                    let sc = map
                        .entry(key_for_stream.server_name.clone())
                        .or_insert_with(|| ServerCache::new(max_entries));
                    sc.insert(
                        key_for_stream.clone(),
                        CacheEntry {
                            value: response.result.clone(),
                            inserted_at: Instant::now(),
                            ttl,
                        },
                    );
                }
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
    use crate::mcp::McpTransport;
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

    #[tokio::test]
    async fn upstream_ttl_hint_is_honoured() {
        let inner = Arc::new(CountingExecutor {
            calls: AtomicUsize::new(0),
            value: serde_json::json!({
                "tools": [],
                "_meta": { "ttlMs": 50 }
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
