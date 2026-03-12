//! Warp filter for the `GET /v1/metrics` endpoint.
//!
//! Returns per-route aggregate metrics collected from the running BitRouter
//! instance. Metrics are held in memory and reset on process restart.
//! See [`crate::metrics`] for details on the storage model and limitations.

use std::sync::Arc;

use warp::Filter;

use crate::metrics::MetricsStore;

/// Creates a warp filter for `GET /v1/metrics`.
///
/// The filter reads from the shared [`MetricsStore`] and returns a JSON
/// snapshot of per-route and per-endpoint performance data.
pub fn metrics_filter(
    store: Arc<MetricsStore>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path!("v1" / "metrics")
        .and(warp::get())
        .and(warp::any().map(move || store.clone()))
        .map(handle_metrics)
}

fn handle_metrics(store: Arc<MetricsStore>) -> impl warp::Reply {
    let snapshot = store.snapshot();
    warp::reply::json(&snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::RequestMetrics;

    #[tokio::test]
    async fn metrics_endpoint_returns_empty_snapshot() {
        let store = Arc::new(MetricsStore::new());
        let filter = metrics_filter(store);

        let res = warp::test::request()
            .method("GET")
            .path("/v1/metrics")
            .reply(&filter)
            .await;

        assert_eq!(res.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        assert!(json["uptime_seconds"].is_number());
        assert_eq!(json["routes"], serde_json::json!({}));
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_recorded_data() {
        let store = Arc::new(MetricsStore::new());
        store.record(RequestMetrics {
            route: "fast".into(),
            endpoint: "openai:gpt-4o-mini".into(),
            latency_ms: 300,
            is_error: false,
            input_tokens: Some(100),
            output_tokens: Some(50),
        });
        store.record(RequestMetrics {
            route: "fast".into(),
            endpoint: "openai:gpt-4o-mini".into(),
            latency_ms: 500,
            is_error: true,
            input_tokens: None,
            output_tokens: None,
        });

        let filter = metrics_filter(store);

        let res = warp::test::request()
            .method("GET")
            .path("/v1/metrics")
            .reply(&filter)
            .await;

        assert_eq!(res.status(), 200);
        let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        let route = &json["routes"]["fast"];
        assert_eq!(route["total_requests"], 2);
        assert_eq!(route["total_errors"], 1);
        assert_eq!(
            route["by_endpoint"]["openai:gpt-4o-mini"]["total_requests"],
            2
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_wrong_method() {
        let store = Arc::new(MetricsStore::new());
        let filter = metrics_filter(store);

        let res = warp::test::request()
            .method("POST")
            .path("/v1/metrics")
            .reply(&filter)
            .await;

        assert_eq!(res.status(), 405);
    }
}
