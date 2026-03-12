//! In-memory per-route metrics for BitRouter.
//!
//! This module provides a thread-safe, in-memory metrics store that records
//! per-route and per-endpoint performance data. Metrics accumulate during the
//! lifetime of the process and are reset on restart.
//!
//! # Persistence
//!
//! Metrics are currently held **in memory only** and are lost on process
//! restart. This is intentional for the initial release — the consuming plugin
//! layer can handle its own persistence if needed. A future release may back
//! this store with a database or Redis cache.
//!
//! # Latency tracking
//!
//! Latency samples are stored in a bounded vector (up to
//! [`MAX_LATENCY_SAMPLES`] per route and per endpoint). When the cap is
//! reached the oldest half of the samples are discarded so percentile
//! calculations remain representative of recent traffic. A future optimisation
//! could replace this with an HDR histogram or t-digest sketch for constant
//! memory overhead.
//!
//! # Streaming requests
//!
//! For streaming requests only the time-to-stream-start latency and
//! request/error counts are recorded — token usage is not available until the
//! stream completes, so `avg_input_tokens` and `avg_output_tokens` only
//! reflect non-streaming (generate) requests.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Maximum number of latency samples retained per route or endpoint.
///
/// When this limit is reached the oldest half of samples are discarded to keep
/// memory usage bounded while preserving recent data for percentile accuracy.
///
/// This value is intentionally conservative to avoid excessive memory usage in
/// deployments with many routes/endpoints while still keeping enough samples
/// for stable percentile estimates.
const MAX_LATENCY_SAMPLES: usize = 10_000;

// ── Public types ────────────────────────────────────────────────────────────

/// Thread-safe, in-memory store for per-route request metrics.
///
/// Create a single instance at server startup and share it (via `Arc`) with
/// all request-handling filters. Call [`MetricsStore::record`] after each
/// upstream request completes, and [`MetricsStore::snapshot`] to produce a
/// serializable view for the `GET /v1/metrics` endpoint.
///
/// # Persistence
///
/// Metrics are currently held in memory only and are lost on process restart.
/// This is intentional for the initial release — the consuming plugin layer
/// (OpenClaw) can handle its own persistence if needed. A future release may
/// back this store with a database or Redis cache.
pub struct MetricsStore {
    started_at: Instant,
    inner: RwLock<StoreInner>,
}

/// Data captured from a single completed request, used to update the store.
pub struct RequestMetrics {
    /// The route name (incoming model name).
    pub route: String,
    /// The endpoint identifier, typically `"provider:model_id"`.
    pub endpoint: String,
    /// Request latency in milliseconds.
    pub latency_ms: u64,
    /// Whether the request resulted in an error.
    pub is_error: bool,
    /// Input token count from the response (if available).
    pub input_tokens: Option<u32>,
    /// Output token count from the response (if available).
    pub output_tokens: Option<u32>,
}

/// Formats a `"provider:model_id"` endpoint identifier from routing target
/// components. Used by all provider handlers to build the endpoint key before
/// routing consumes the target.
pub fn format_endpoint(provider_name: &str, model_id: &str) -> String {
    format!("{provider_name}:{model_id}")
}

// ── Serialisable snapshot types ─────────────────────────────────────────────

/// Top-level metrics snapshot returned by `GET /v1/metrics`.
#[derive(Debug, Serialize)]
pub struct MetricsSnapshot {
    /// Seconds since the metrics store was created (≈ process uptime).
    pub uptime_seconds: u64,
    /// Per-route aggregate metrics keyed by route name.
    pub routes: HashMap<String, RouteSnapshot>,
}

/// Aggregate metrics for a single route.
#[derive(Debug, Serialize)]
pub struct RouteSnapshot {
    pub total_requests: u64,
    pub total_errors: u64,
    pub error_rate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_p50_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_p99_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_output_tokens: Option<u64>,
    /// Unix timestamp (seconds) of the most recent request on this route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used: Option<u64>,
    /// Per-endpoint breakdown within this route.
    pub by_endpoint: HashMap<String, EndpointSnapshot>,
}

/// Aggregate metrics for a single endpoint within a route.
#[derive(Debug, Serialize)]
pub struct EndpointSnapshot {
    pub total_requests: u64,
    pub total_errors: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_p50_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_p99_ms: Option<u64>,
}

// ── Internal storage ────────────────────────────────────────────────────────

struct StoreInner {
    routes: HashMap<String, RouteData>,
}

struct RouteData {
    total_requests: u64,
    total_errors: u64,
    latencies_ms: Vec<u64>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    /// Number of requests that reported token data (for averaging).
    token_request_count: u64,
    last_used: Option<SystemTime>,
    endpoints: HashMap<String, EndpointData>,
}

struct EndpointData {
    total_requests: u64,
    total_errors: u64,
    latencies_ms: Vec<u64>,
}

// ── Implementation ──────────────────────────────────────────────────────────

impl MetricsStore {
    /// Creates a new, empty metrics store. The uptime clock starts now.
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            inner: RwLock::new(StoreInner {
                routes: HashMap::new(),
            }),
        }
    }

    /// Records a completed request into the store.
    ///
    /// This method acquires a write-lock for a very short duration (counter
    /// increments + a `Vec::push`) so contention should be negligible.
    pub fn record(&self, event: RequestMetrics) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());

        let route = inner
            .routes
            .entry(event.route)
            .or_insert_with(RouteData::new);

        route.total_requests += 1;
        if event.is_error {
            route.total_errors += 1;
        }
        push_latency(&mut route.latencies_ms, event.latency_ms);
        route.last_used = Some(SystemTime::now());

        if let (Some(input), Some(output)) = (event.input_tokens, event.output_tokens) {
            route.total_input_tokens += input as u64;
            route.total_output_tokens += output as u64;
            route.token_request_count += 1;
        }

        let ep = route
            .endpoints
            .entry(event.endpoint)
            .or_insert_with(EndpointData::new);
        ep.total_requests += 1;
        if event.is_error {
            ep.total_errors += 1;
        }
        push_latency(&mut ep.latencies_ms, event.latency_ms);
    }

    /// Records a successful generate (non-streaming) request.
    ///
    /// This is a convenience wrapper used by provider handlers to reduce
    /// duplicated recording logic across OpenAI, Anthropic and Google filters.
    pub fn record_success(
        &self,
        route: String,
        endpoint: String,
        start: Instant,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    ) {
        self.record(RequestMetrics {
            route,
            endpoint,
            latency_ms: start.elapsed().as_millis() as u64,
            is_error: false,
            input_tokens,
            output_tokens,
        });
    }

    /// Records a failed or stream-only request.
    ///
    /// `is_error` should be `true` for upstream failures and `false` for
    /// streaming requests where token counts are unavailable.
    pub fn record_outcome(&self, route: String, endpoint: String, start: Instant, is_error: bool) {
        self.record(RequestMetrics {
            route,
            endpoint,
            latency_ms: start.elapsed().as_millis() as u64,
            is_error,
            input_tokens: None,
            output_tokens: None,
        });
    }

    /// Produces a serialisable snapshot of all collected metrics.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let uptime_seconds = self.started_at.elapsed().as_secs();

        let routes = inner
            .routes
            .iter()
            .map(|(name, data)| {
                let route_snapshot = RouteSnapshot {
                    total_requests: data.total_requests,
                    total_errors: data.total_errors,
                    error_rate: error_rate(data.total_requests, data.total_errors),
                    latency_p50_ms: percentile(&data.latencies_ms, 50.0),
                    latency_p99_ms: percentile(&data.latencies_ms, 99.0),
                    avg_input_tokens: avg(data.total_input_tokens, data.token_request_count),
                    avg_output_tokens: avg(data.total_output_tokens, data.token_request_count),
                    last_used: data.last_used.map(system_time_to_unix_secs),
                    by_endpoint: data
                        .endpoints
                        .iter()
                        .map(|(ep_name, ep)| {
                            let ep_snap = EndpointSnapshot {
                                total_requests: ep.total_requests,
                                total_errors: ep.total_errors,
                                latency_p50_ms: percentile(&ep.latencies_ms, 50.0),
                                latency_p99_ms: percentile(&ep.latencies_ms, 99.0),
                            };
                            (ep_name.clone(), ep_snap)
                        })
                        .collect(),
                };
                (name.clone(), route_snapshot)
            })
            .collect();

        MetricsSnapshot {
            uptime_seconds,
            routes,
        }
    }
}

impl Default for MetricsStore {
    fn default() -> Self {
        Self::new()
    }
}

// ── Private helpers ─────────────────────────────────────────────────────────

impl RouteData {
    fn new() -> Self {
        Self {
            total_requests: 0,
            total_errors: 0,
            latencies_ms: Vec::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            token_request_count: 0,
            last_used: None,
            endpoints: HashMap::new(),
        }
    }
}

impl EndpointData {
    fn new() -> Self {
        Self {
            total_requests: 0,
            total_errors: 0,
            latencies_ms: Vec::new(),
        }
    }
}

/// Appends a latency sample, evicting the oldest half when the buffer is full.
fn push_latency(latencies: &mut Vec<u64>, value: u64) {
    if latencies.len() >= MAX_LATENCY_SAMPLES {
        let half = latencies.len() / 2;
        latencies.drain(..half);
    }
    latencies.push(value);
}

/// Computes a percentile from an unsorted slice using the ceiling-rank method.
fn percentile(latencies: &[u64], p: f64) -> Option<u64> {
    if latencies.is_empty() {
        return None;
    }
    let mut sorted = latencies.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let rank = (p / 100.0 * n as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    Some(sorted[idx])
}

fn error_rate(total: u64, errors: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        errors as f64 / total as f64
    }
}

fn avg(total: u64, count: u64) -> Option<u64> {
    if count == 0 {
        None
    } else {
        Some(total / count)
    }
}

fn system_time_to_unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_returns_empty_snapshot() {
        let store = MetricsStore::new();
        let snap = store.snapshot();
        assert!(snap.routes.is_empty());
        assert!(snap.uptime_seconds < 2);
    }

    #[test]
    fn record_single_success() {
        let store = MetricsStore::new();
        store.record(RequestMetrics {
            route: "fast".into(),
            endpoint: "openai:gpt-4o-mini".into(),
            latency_ms: 300,
            is_error: false,
            input_tokens: Some(100),
            output_tokens: Some(50),
        });

        let snap = store.snapshot();
        let route = snap.routes.get("fast").expect("route should exist");
        assert_eq!(route.total_requests, 1);
        assert_eq!(route.total_errors, 0);
        assert!((route.error_rate - 0.0).abs() < f64::EPSILON);
        assert_eq!(route.latency_p50_ms, Some(300));
        assert_eq!(route.latency_p99_ms, Some(300));
        assert_eq!(route.avg_input_tokens, Some(100));
        assert_eq!(route.avg_output_tokens, Some(50));
        assert!(route.last_used.is_some());

        let ep = route
            .by_endpoint
            .get("openai:gpt-4o-mini")
            .expect("endpoint should exist");
        assert_eq!(ep.total_requests, 1);
        assert_eq!(ep.total_errors, 0);
    }

    #[test]
    fn record_error_increments_counters() {
        let store = MetricsStore::new();
        store.record(RequestMetrics {
            route: "fast".into(),
            endpoint: "openai:gpt-4o-mini".into(),
            latency_ms: 500,
            is_error: true,
            input_tokens: None,
            output_tokens: None,
        });

        let snap = store.snapshot();
        let route = &snap.routes["fast"];
        assert_eq!(route.total_requests, 1);
        assert_eq!(route.total_errors, 1);
        assert!((route.error_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(route.avg_input_tokens, None);
    }

    #[test]
    fn multiple_endpoints_tracked_separately() {
        let store = MetricsStore::new();
        for _ in 0..3 {
            store.record(RequestMetrics {
                route: "fast".into(),
                endpoint: "openai:gpt-4o-mini".into(),
                latency_ms: 200,
                is_error: false,
                input_tokens: Some(50),
                output_tokens: Some(25),
            });
        }
        for _ in 0..2 {
            store.record(RequestMetrics {
                route: "fast".into(),
                endpoint: "anthropic:claude-haiku".into(),
                latency_ms: 400,
                is_error: false,
                input_tokens: Some(60),
                output_tokens: Some(30),
            });
        }

        let snap = store.snapshot();
        let route = &snap.routes["fast"];
        assert_eq!(route.total_requests, 5);
        assert_eq!(route.by_endpoint.len(), 2);
        assert_eq!(route.by_endpoint["openai:gpt-4o-mini"].total_requests, 3);
        assert_eq!(
            route.by_endpoint["anthropic:claude-haiku"].total_requests,
            2
        );
    }

    #[test]
    fn percentile_calculation() {
        // Deterministic check with known values.
        let latencies: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&latencies, 50.0), Some(50));
        assert_eq!(percentile(&latencies, 99.0), Some(99));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn latency_buffer_eviction() {
        let store = MetricsStore::new();
        for i in 0..(MAX_LATENCY_SAMPLES + 10) {
            store.record(RequestMetrics {
                route: "r".into(),
                endpoint: "e".into(),
                latency_ms: i as u64,
                is_error: false,
                input_tokens: None,
                output_tokens: None,
            });
        }
        let inner = store.inner.read().unwrap_or_else(|e| e.into_inner());
        let route = &inner.routes["r"];
        assert!(route.latencies_ms.len() <= MAX_LATENCY_SAMPLES);
    }

    #[test]
    fn snapshot_serialises_to_json() {
        let store = MetricsStore::new();
        store.record(RequestMetrics {
            route: "default".into(),
            endpoint: "openai:gpt-4o".into(),
            latency_ms: 250,
            is_error: false,
            input_tokens: Some(10),
            output_tokens: Some(5),
        });
        let snap = store.snapshot();
        let json = serde_json::to_value(&snap).expect("should serialise");
        assert!(json["uptime_seconds"].is_number());
        assert_eq!(json["routes"]["default"]["total_requests"], 1);
        assert_eq!(
            json["routes"]["default"]["by_endpoint"]["openai:gpt-4o"]["total_requests"],
            1
        );
    }
}
