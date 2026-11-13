use std::{error::Error, sync::Arc, time::Duration};

use axum::{Router, body::Body, http::Response, routing::get};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

use super::{
    config::ConnectorConfig,
    metrics::{self, MetricError},
    observer::{ObservationStatus, Observer, SourceFreshness, UnknownReason},
    report,
};

type TestResult = Result<(), Box<dyn Error>>;

fn yaml(metrics: &str) -> String {
    format!(
        "version: 1\npool:\n  id: test\n  model: test/model\n  scope:\n    kind: static_members\n  members:\n    - id: a\n      inference_endpoint: http://127.0.0.1:9999/v1\n      metrics_endpoint: {metrics}\n      engines: ['0']\n"
    )
}

fn payload(running: &str, waiting: &str, kv: &str, timestamp: Option<i64>) -> String {
    let suffix = timestamp.map_or(String::new(), |value| format!(" {value}"));
    format!(
        "# TYPE vllm:num_requests_running gauge\nvllm:num_requests_running{{model_name=\"test/model\",engine=\"0\"}} {running}{suffix}\nvllm:num_requests_waiting{{engine=\"0\",model_name=\"test/model\"}} {waiting}{suffix}\nvllm:kv_cache_usage_perc{{model_name=\"test/model\",engine=\"0\"}} {kv}{suffix}\n"
    )
}

#[test]
fn configuration_is_strict_and_redacts_invalid_input() -> TestResult {
    let valid = yaml("http://localhost:8000/metrics");
    let config = ConnectorConfig::from_yaml(&valid)?;
    assert_eq!(config.observation.timeout_ms, 2_000);
    for invalid in [
        valid.replace("version: 1", "version: 2"),
        format!("{valid}mystery: super-secret\n"),
        valid.replace(
            "http://localhost:8000/metrics",
            "http://user:super-secret@localhost/metrics",
        ),
        valid.replace(
            "http://localhost:8000/metrics",
            "http://localhost/metrics?token=super-secret",
        ),
        valid.replace("engines: ['0']", "engines: ['0', '0']"),
        valid.replace("kind: static_members", "kind: all_router_backends"),
        valid.replace(
            "kind: static_members",
            "kind: static_members\n    unknown: true",
        ),
        valid.replace("model: test/model", "model: ''"),
        valid.replace("version: 1", "version: 1\nversion: 1"),
        "super-secret: [broken".to_string(),
    ] {
        let error = ConnectorConfig::from_yaml(&invalid)
            .err()
            .ok_or("invalid configuration accepted")?;
        assert!(!error.to_string().contains("super-secret"));
    }
    Ok(())
}

#[test]
fn duplicate_normalized_metric_scope_is_rejected() -> TestResult {
    let mut config = ConnectorConfig::from_yaml(&yaml("http://localhost:80/metrics"))?;
    let mut second = config.pool.members[0].clone();
    second.id = "b".to_string();
    second.inference_endpoint = "http://localhost:9998/v1".to_string();
    second.metrics_endpoint = "http://localhost/metrics".to_string();
    config.pool.members.push(second);
    assert!(config.validate().is_err());
    assert!(Observer::new(config).is_err());
    Ok(())
}

#[test]
fn parser_validates_units_scope_and_freshness() -> TestResult {
    let config = ConnectorConfig::from_yaml(&yaml("http://localhost/metrics"))?;
    let member = &config.pool.members[0];
    let now = 1_800_000_000_000;
    let parsed = metrics::parse(
        &payload("2", "3", "0.25", Some(now - 100)),
        &config.pool.model,
        member,
        now,
        1000,
    )
    .map_err(|error| format!("{error:?}"))?;
    assert_eq!(parsed.running, 2);
    assert_eq!(parsed.waiting, 3);
    assert_eq!(parsed.kv_fraction_max, 0.25);
    assert!(parsed.all_samples_timestamped);
    let good = payload("0", "0", "0", None);
    let cases = [
        (payload("NaN", "0", "0", None), MetricError::NonFiniteMetric),
        (
            payload("0", "+Inf", "0", None),
            MetricError::NonFiniteMetric,
        ),
        (payload("-1", "0", "0", None), MetricError::OutOfRangeMetric),
        (
            payload("1.5", "0", "0", None),
            MetricError::OutOfRangeMetric,
        ),
        (
            payload("0", "0", "1.1", None),
            MetricError::OutOfRangeMetric,
        ),
        (
            payload("0", "0", "0", Some(now - 1001)),
            MetricError::StaleSample,
        ),
        (
            payload("0", "0", "0", Some(now + 1)),
            MetricError::FutureSample,
        ),
        (
            good.replace("test/model", "another/model"),
            MetricError::ScopeMismatch,
        ),
        (
            good.replace("engine=\"0\"", "engine=\"1\""),
            MetricError::ScopeMismatch,
        ),
        (format!("{good}{good}"), MetricError::AmbiguousMetric),
        (
            good.replace(
                "# TYPE vllm:num_requests_running gauge",
                "# TYPE vllm:num_requests_running counter",
            ),
            MetricError::MalformedMetric,
        ),
        (
            good.replace(
                "# TYPE vllm:num_requests_running gauge",
                "# TYPE\tvllm:num_requests_running counter",
            ),
            MetricError::MalformedMetric,
        ),
        (
            good.replace(
                "# TYPE vllm:num_requests_running gauge",
                "#  TYPE vllm:num_requests_running counter",
            ),
            MetricError::MalformedMetric,
        ),
        (
            good.lines()
                .filter(|line| !line.starts_with("vllm:num_requests_waiting"))
                .collect::<Vec<_>>()
                .join("\n"),
            MetricError::MissingMetric,
        ),
        (
            good.replace("engine=\"0\"", "engine=\"0\",pool=\"hidden\""),
            MetricError::ScopeMismatch,
        ),
        (
            good.replace(
                "model_name=\"test/model\"",
                "model_name=\"test/model\",model_name=\"test/model\"",
            ),
            MetricError::AmbiguousMetric,
        ),
        (
            good.replace("} 0", "} invalid"),
            MetricError::MalformedMetric,
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(
            metrics::parse(&input, &config.pool.model, member, now, 1000).err(),
            Some(expected)
        );
    }
    Ok(())
}

#[test]
fn parser_requires_every_engine_and_uses_max_kv() -> TestResult {
    let mut config = ConnectorConfig::from_yaml(&yaml("http://localhost/metrics"))?;
    config.pool.members[0].engines.push("1".to_string());
    let first = payload("2", "3", "0.25", None);
    let second = payload("4", "5", "0.8", None).replace("engine=\"0\"", "engine=\"1\"");
    assert_eq!(
        metrics::parse(
            &first,
            &config.pool.model,
            &config.pool.members[0],
            1000,
            1000
        )
        .err(),
        Some(MetricError::MissingMetric)
    );
    let reading = metrics::parse(
        &format!("{first}{second}"),
        &config.pool.model,
        &config.pool.members[0],
        1000,
        1000,
    )
    .map_err(|error| format!("{error:?}"))?;
    assert_eq!(reading.running, 6);
    assert_eq!(reading.waiting, 8);
    assert_eq!(reading.kv_fraction_max, 0.8);
    Ok(())
}

struct Fixture {
    url: String,
    state: Arc<Mutex<(u16, String)>>,
    task: JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture(body: String) -> Result<Fixture, Box<dyn Error>> {
    let state = Arc::new(Mutex::new((200, body)));
    let handler_state = state.clone();
    let app = Router::new().route(
        "/metrics",
        get(move || {
            let state = handler_state.clone();
            async move {
                let (status, body) = state.lock().await.clone();
                Response::builder()
                    .status(status)
                    .header("Location", "http://127.0.0.1:1/secret")
                    .body(Body::from(body))
                    .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(Fixture {
        url: format!("http://{address}/metrics"),
        state,
        task,
    })
}

#[tokio::test]
async fn http_fixture_reports_collection_only_and_detects_restart() -> TestResult {
    let fixture = fixture(format!(
        "{}process_start_time_seconds 100\n",
        payload("0", "0", "0.1", None)
    ))
    .await?;
    let config = ConnectorConfig::from_yaml(&yaml(&fixture.url))?;
    let check = report::dry_run(&config)?;
    assert_eq!(check.network_requests, 0);
    assert!(!check.capabilities.inference_dispatch);
    assert!(!check.scope_evidence.physical_membership_verified);
    let mut observer = Observer::new(config)?;
    let first = observer.observe().await;
    assert!(first.is_ready());
    assert_eq!(
        first.members[0].freshness.source,
        SourceFreshness::CollectionOnly
    );
    assert!(report::render_human(&first).contains("source sample age unverified"));
    fixture.state.lock().await.1 = format!(
        "{}process_start_time_seconds 200\n",
        payload("0", "0", "0", None)
    );
    let reset = observer.observe().await;
    assert!(!reset.is_ready());
    assert_eq!(reset.members[0].reason, Some(UnknownReason::ProcessRestart));
    assert!(reset.members[0].metrics.is_none());
    assert!(reset.summary.metrics.is_none());
    assert!(observer.observe().await.is_ready());
    Ok(())
}

#[tokio::test]
async fn http_errors_stale_samples_and_oversize_never_produce_pool_totals() -> TestResult {
    let fixture = fixture(payload("0", "0", "0", Some(1))).await?;
    let mut config = ConnectorConfig::from_yaml(&yaml(&fixture.url))?;
    config.observation.max_body_bytes = 1024;
    let mut observer = Observer::new(config)?;
    let stale = observer.observe().await;
    assert_eq!(stale.members[0].status, ObservationStatus::Stale);
    assert_eq!(stale.members[0].reason, Some(UnknownReason::StaleSample));
    for (status, body, reason) in [
        (
            503,
            "do not expose secret body".to_string(),
            UnknownReason::HttpStatus { status: 503 },
        ),
        (302, "redirect".to_string(), UnknownReason::Redirect),
        (200, "x".repeat(1025), UnknownReason::BodyTooLarge),
        (
            200,
            payload("NaN", "0", "0", None),
            UnknownReason::NonFiniteMetric,
        ),
        (200, String::new(), UnknownReason::MissingMetric),
    ] {
        *fixture.state.lock().await = (status, body);
        let snapshot = observer.observe().await;
        assert!(!snapshot.is_ready());
        assert_eq!(snapshot.members[0].reason, Some(reason));
        assert!(snapshot.members[0].metrics.is_none());
        assert!(snapshot.summary.metrics.is_none());
        assert!(!serde_json::to_string(&snapshot)?.contains("secret body"));
    }
    Ok(())
}

#[tokio::test]
async fn timeout_is_bounded() -> TestResult {
    let app = Router::new().route(
        "/metrics",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            payload("0", "0", "0", None)
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut config = ConnectorConfig::from_yaml(&yaml(&format!("http://{address}/metrics")))?;
    config.observation.timeout_ms = 20;
    let mut observer = Observer::new(config)?;
    let snapshot = observer.observe().await;
    task.abort();
    assert_eq!(snapshot.members[0].reason, Some(UnknownReason::Timeout));
    assert!(snapshot.summary.metrics.is_none());
    Ok(())
}

#[tokio::test]
async fn complete_pool_aggregates_and_missing_member_suppresses_totals() -> TestResult {
    let first = fixture(payload("2", "3", "0.25", None)).await?;
    let second = fixture(payload("4", "5", "0.8", None)).await?;
    let mut config = ConnectorConfig::from_yaml(&yaml(&first.url))?;
    let mut member = config.pool.members[0].clone();
    member.id = "b".to_string();
    member.inference_endpoint = "http://127.0.0.1:9998/v1".to_string();
    member.metrics_endpoint = second.url.clone();
    config.pool.members.push(member);
    let mut observer = Observer::new(config)?;
    let complete = observer.observe().await;
    assert!(complete.is_ready());
    let totals = complete
        .summary
        .metrics
        .ok_or("missing complete-pool totals")?;
    assert_eq!(totals.running_requests, 6);
    assert_eq!(totals.waiting_requests, 8);
    assert_eq!(totals.kv_cache_usage_fraction_max, 0.8);
    second.state.lock().await.1 = String::new();
    let partial = observer.observe().await;
    assert!(!partial.is_ready());
    assert_eq!(partial.summary.observed_members, 1);
    assert!(partial.members[0].metrics.is_some());
    assert!(partial.members[1].metrics.is_none());
    assert!(partial.summary.metrics.is_none());
    Ok(())
}

#[tokio::test]
async fn final_snapshot_rechecks_age_and_clock_regression() -> TestResult {
    let now = chrono::Utc::now().timestamp_millis();
    let first = fixture(payload("0", "0", "0", Some(now))).await?;
    let config = ConnectorConfig::from_yaml(&yaml(&first.url))?;
    let mut observer = Observer::new(config)?;
    let snapshot = observer.observe().await;
    assert!(snapshot.is_ready());
    let collected_at = snapshot.members[0]
        .freshness
        .collection_completed_at_ms
        .ok_or("missing collection timestamp")?;
    let mut stale_source = snapshot.members[0].clone();
    super::observer::refresh_freshness(&mut stale_source, collected_at + 1001, Some(20), 1000);
    assert_eq!(stale_source.reason, Some(UnknownReason::StaleSample));
    assert!(stale_source.metrics.is_none());
    let mut stale_collection = snapshot.members[0].clone();
    stale_collection.freshness.oldest_source_sample_at_ms = None;
    super::observer::refresh_freshness(&mut stale_collection, collected_at + 10, Some(1001), 1000);
    assert_eq!(
        stale_collection.reason,
        Some(UnknownReason::CollectionStale)
    );
    assert!(stale_collection.metrics.is_none());
    let mut regressed = snapshot.members[0].clone();
    super::observer::refresh_freshness(&mut regressed, collected_at - 1, Some(20), 1000);
    assert_eq!(regressed.reason, Some(UnknownReason::ClockRegression));
    assert!(regressed.metrics.is_none());
    Ok(())
}

#[tokio::test]
async fn mixed_timestamp_provenance_does_not_claim_full_source_freshness() -> TestResult {
    let now = chrono::Utc::now().timestamp_millis();
    let body = payload("0", "0", "0", Some(now)).replacen(&format!(" 0 {now}"), " 0", 1);
    let fixture = fixture(body).await?;
    let mut observer = Observer::new(ConnectorConfig::from_yaml(&yaml(&fixture.url))?)?;
    let snapshot = observer.observe().await;
    assert!(snapshot.is_ready());
    assert_eq!(
        snapshot.members[0].freshness.source,
        SourceFreshness::PartialTimestamps
    );
    Ok(())
}
