//! In-memory per-route metrics collector for BitRouter.
//!
//! Moved from `bitrouter-api::metrics` and adapted to implement
//! [`ObserveCallback`] so that all observability flows through a single trait.
//!
//! # Persistence
//!
//! Metrics are held **in memory only** and are lost on process restart.
//!
//! # Latency tracking
//!
//! Latency samples are stored in a bounded vector (up to
//! [`MAX_LATENCY_SAMPLES`] per route and per endpoint). When the cap is
//! reached the oldest half of the samples are discarded so percentile
//! calculations remain representative of recent traffic.
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

use std::future::Future;
use std::pin::Pin;

use bitrouter_core::observe::{ObserveCallback, RequestFailureEvent, RequestSuccessEvent};
use serde::Serialize;

/// Maximum number of latency samples retained per route or endpoint.
const MAX_LATENCY_SAMPLES: usize = 10_000;

// ── Public types ────────────────────────────────────────────────────────────

/// Thread-safe, in-memory metrics collector.
///
/// Create a single instance at server startup and share it (via `Arc`) as
/// part of a [`CompositeObserver`](crate::composite::CompositeObserver).
/// Call [`MetricsCollector::snapshot`] to produce a serializable view.
pub struct MetricsCollector {
    started_at: Instant,
    inner: RwLock<StoreInner>,
}

/// Formats a `"provider:model_id"` endpoint identifier.
pub fn format_endpoint(provider_name: &str, model_id: &str) -> String {
    format!("{provider_name}:{model_id}")
}

// ── Serialisable snapshot types ─────────────────────────────────────────────

/// Top-level metrics snapshot.
#[derive(Debug, Serialize)]
pub struct MetricsSnapshot {
    /// Seconds since the metrics collector was created.
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
    token_request_count: u64,
    last_used: Option<SystemTime>,
    endpoints: HashMap<String, EndpointData>,
}

struct EndpointData {
    total_requests: u64,
    total_errors: u64,
    latencies_ms: Vec<u64>,
}

struct ClonedRouteData {
    total_requests: u64,
    total_errors: u64,
    latencies_ms: Vec<u64>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    token_request_count: u64,
    last_used: Option<SystemTime>,
    endpoints: Vec<(String, ClonedEndpointData)>,
}

struct ClonedEndpointData {
    total_requests: u64,
    total_errors: u64,
    latencies_ms: Vec<u64>,
}

// ── Implementation ──────────────────────────────────────────────────────────

impl MetricsCollector {
    /// Creates a new, empty metrics collector. The uptime clock starts now.
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            inner: RwLock::new(StoreInner {
                routes: HashMap::new(),
            }),
        }
    }

    /// Records a request event into the collector.
    fn record(
        &self,
        route: String,
        endpoint: String,
        latency_ms: u64,
        is_error: bool,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    ) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());

        let route_data = inner.routes.entry(route).or_insert_with(RouteData::new);

        route_data.total_requests += 1;
        if is_error {
            route_data.total_errors += 1;
        }
        push_latency(&mut route_data.latencies_ms, latency_ms);
        route_data.last_used = Some(SystemTime::now());

        if let (Some(input), Some(output)) = (input_tokens, output_tokens) {
            route_data.total_input_tokens += input as u64;
            route_data.total_output_tokens += output as u64;
            route_data.token_request_count += 1;
        }

        let ep = route_data
            .endpoints
            .entry(endpoint)
            .or_insert_with(EndpointData::new);
        ep.total_requests += 1;
        if is_error {
            ep.total_errors += 1;
        }
        push_latency(&mut ep.latencies_ms, latency_ms);
    }

    /// Produces a serialisable snapshot of all collected metrics.
    ///
    /// The read-lock is held only long enough to clone raw counters and latency
    /// vectors. Expensive work (sorting for percentiles, computing averages) is
    /// performed after the lock is released.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let uptime_seconds = self.started_at.elapsed().as_secs();

        let cloned_routes: Vec<(String, ClonedRouteData)> = {
            let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
            inner
                .routes
                .iter()
                .map(|(name, data)| {
                    let endpoints: Vec<(String, ClonedEndpointData)> = data
                        .endpoints
                        .iter()
                        .map(|(ep_name, ep)| {
                            (
                                ep_name.clone(),
                                ClonedEndpointData {
                                    total_requests: ep.total_requests,
                                    total_errors: ep.total_errors,
                                    latencies_ms: ep.latencies_ms.clone(),
                                },
                            )
                        })
                        .collect();

                    (
                        name.clone(),
                        ClonedRouteData {
                            total_requests: data.total_requests,
                            total_errors: data.total_errors,
                            latencies_ms: data.latencies_ms.clone(),
                            total_input_tokens: data.total_input_tokens,
                            total_output_tokens: data.total_output_tokens,
                            token_request_count: data.token_request_count,
                            last_used: data.last_used,
                            endpoints,
                        },
                    )
                })
                .collect()
        };

        let routes = cloned_routes
            .into_iter()
            .map(|(name, data)| {
                let by_endpoint = data
                    .endpoints
                    .into_iter()
                    .map(|(ep_name, ep)| {
                        let ep_snap = EndpointSnapshot {
                            total_requests: ep.total_requests,
                            total_errors: ep.total_errors,
                            latency_p50_ms: percentile(&ep.latencies_ms, 50.0),
                            latency_p99_ms: percentile(&ep.latencies_ms, 99.0),
                        };
                        (ep_name, ep_snap)
                    })
                    .collect();

                let route_snapshot = RouteSnapshot {
                    total_requests: data.total_requests,
                    total_errors: data.total_errors,
                    error_rate: error_rate(data.total_requests, data.total_errors),
                    latency_p50_ms: percentile(&data.latencies_ms, 50.0),
                    latency_p99_ms: percentile(&data.latencies_ms, 99.0),
                    avg_input_tokens: avg(data.total_input_tokens, data.token_request_count),
                    avg_output_tokens: avg(data.total_output_tokens, data.token_request_count),
                    last_used: data.last_used.map(system_time_to_unix_secs),
                    by_endpoint,
                };
                (name, route_snapshot)
            })
            .collect();

        MetricsSnapshot {
            uptime_seconds,
            routes,
        }
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl ObserveCallback for MetricsCollector {
    fn on_request_success(
        &self,
        event: RequestSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let endpoint = format_endpoint(&event.ctx.provider, &event.ctx.model);
        self.record(
            event.ctx.route,
            endpoint,
            event.ctx.latency_ms,
            false,
            event.usage.input_tokens.total,
            event.usage.output_tokens.total,
        );
        Box::pin(async {})
    }

    fn on_request_failure(
        &self,
        event: RequestFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let endpoint = format_endpoint(&event.ctx.provider, &event.ctx.model);
        self.record(
            event.ctx.route,
            endpoint,
            event.ctx.latency_ms,
            true,
            None,
            None,
        );
        Box::pin(async {})
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

fn push_latency(latencies: &mut Vec<u64>, value: u64) {
    if latencies.len() >= MAX_LATENCY_SAMPLES {
        let half = latencies.len() / 2;
        latencies.drain(..half);
    }
    latencies.push(value);
}

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
    use bitrouter_core::models::language::usage::{
        LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage,
    };
    use bitrouter_core::observe::{RequestContext, RequestSuccessEvent};

    use super::*;

    fn test_ctx(route: &str, provider: &str, model: &str) -> RequestContext {
        RequestContext {
            route: route.into(),
            provider: provider.into(),
            model: model.into(),
            account_id: None,
            latency_ms: 300,
        }
    }

    fn test_usage(input: u32, output: u32) -> LanguageModelUsage {
        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: Some(input),
                no_cache: None,
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: Some(output),
                text: None,
                reasoning: None,
            },
            raw: None,
        }
    }

    #[test]
    fn empty_collector_returns_empty_snapshot() {
        let collector = MetricsCollector::new();
        let snap = collector.snapshot();
        assert!(snap.routes.is_empty());
        assert!(snap.uptime_seconds < 2);
    }

    #[tokio::test]
    async fn record_single_success_via_callback() {
        let collector = MetricsCollector::new();
        collector
            .on_request_success(RequestSuccessEvent {
                ctx: test_ctx("fast", "openai", "gpt-4o-mini"),
                usage: test_usage(100, 50),
            })
            .await;

        let snap = collector.snapshot();
        let route = snap.routes.get("fast").expect("route should exist");
        assert_eq!(route.total_requests, 1);
        assert_eq!(route.total_errors, 0);
        assert!((route.error_rate - 0.0).abs() < f64::EPSILON);
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

    #[tokio::test]
    async fn record_failure_via_callback() {
        let collector = MetricsCollector::new();
        collector
            .on_request_failure(bitrouter_core::observe::RequestFailureEvent {
                ctx: RequestContext {
                    route: "fast".into(),
                    provider: "openai".into(),
                    model: "gpt-4o-mini".into(),
                    account_id: None,
                    latency_ms: 500,
                },
                error: bitrouter_core::errors::BitrouterError::transport(None, "timeout"),
            })
            .await;

        let snap = collector.snapshot();
        let route = &snap.routes["fast"];
        assert_eq!(route.total_requests, 1);
        assert_eq!(route.total_errors, 1);
        assert!((route.error_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(route.avg_input_tokens, None);
    }

    #[tokio::test]
    async fn multiple_endpoints_tracked_separately() {
        let collector = MetricsCollector::new();
        for _ in 0..3 {
            collector
                .on_request_success(RequestSuccessEvent {
                    ctx: test_ctx("fast", "openai", "gpt-4o-mini"),
                    usage: test_usage(50, 25),
                })
                .await;
        }
        for _ in 0..2 {
            collector
                .on_request_success(RequestSuccessEvent {
                    ctx: test_ctx("fast", "anthropic", "claude-haiku"),
                    usage: test_usage(60, 30),
                })
                .await;
        }

        let snap = collector.snapshot();
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
        let latencies: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&latencies, 50.0), Some(50));
        assert_eq!(percentile(&latencies, 99.0), Some(99));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn snapshot_serialises_to_json() {
        let collector = MetricsCollector::new();
        collector.record(
            "default".into(),
            "openai:gpt-4o".into(),
            250,
            false,
            Some(10),
            Some(5),
        );
        let snap = collector.snapshot();
        let json = serde_json::to_value(&snap).expect("should serialise");
        assert!(json["uptime_seconds"].is_number());
        assert_eq!(json["routes"]["default"]["total_requests"], 1);
        assert_eq!(
            json["routes"]["default"]["by_endpoint"]["openai:gpt-4o"]["total_requests"],
            1
        );
    }
}
