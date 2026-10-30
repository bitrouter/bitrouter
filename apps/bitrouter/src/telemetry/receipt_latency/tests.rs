use super::*;
use anyhow::{Result, ensure};

#[test]
fn disabled_has_no_timing_storage_or_clock_tokens() -> Result<()> {
    let sink = Sink::default();
    let timing = sink.timing(&"ordinary".try_into()?);
    timing.admitted();
    timing.ready();
    timing.queued();
    timing.dequeue();
    timing.entered();
    timing.returned(true);
    ensure!(timing.0.is_none());
    ensure!(timing.span(Phase::SqlCommit).0.is_none());
    ensure!(sink.seal(false));
    Ok(())
}

#[test]
fn strict_configuration_is_bounded_and_requires_available_metrics() -> Result<()> {
    let mut config = crate::config::Config::default();
    ensure!(!Config::read(&config)?.enabled);
    config.plugins.insert(
        "bitrouter-telemetry".into(),
        serde_json::json!({"receipt_latency":{"enabled":false,"surprise":"secret"}}),
    );
    ensure!(Config::read(&config).is_err());
    config.tenancy.enabled = true;
    for block in [
        serde_json::json!({"enabled":true}),
        serde_json::json!({"enabled":false}),
    ] {
        config.plugins.insert("bitrouter-telemetry".into(), serde_json::json!({"otel":{"metrics":block},"receipt_latency":{"enabled":true,"correlation_id":"cohort"}}));
        let enabled = block.get("enabled").and_then(serde_json::Value::as_bool) == Some(true);
        ensure!(Config::read(&config).is_ok() == (crate::telemetry::OTEL_ENABLED && enabled));
    }
    config.plugins.insert(
        "bitrouter-telemetry".into(),
        serde_json::json!({"receipt_latency":{"enabled":false,"correlation_id":"x".repeat(129)}}),
    );
    ensure!(Config::read(&config).is_err());
    Ok(())
}

#[test]
fn nanoseconds_reject_overflow_instead_of_clipping() -> Result<()> {
    ensure!(bounded_ns(Duration::from_nanos(19_999_999)) == Some(19_999_999));
    ensure!(bounded_ns(Duration::from_nanos(20_000_000)) == Some(20_000_000));
    ensure!(bounded_ns(Duration::from_nanos(20_000_001)) == Some(20_000_001));
    ensure!(bounded_ns(Duration::from_nanos(MAX_NS)) == Some(MAX_NS));
    ensure!(bounded_ns(Duration::from_nanos(MAX_NS + 1)).is_none());
    ensure!(bounded_ns(Duration::MAX).is_none());
    ensure!(u128::from(MAX_NS) * u128::from(UPDATE_LIMIT) < u128::from(u64::MAX));
    Ok(())
}

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
mod enabled {
    use super::*;
    use std::sync::Mutex;
    #[derive(Default)]
    struct Capture(Mutex<Vec<(&'static str, u64)>>);
    impl Recorder for Capture {
        fn phase(&self, value: u64, _: Phase, _: ResultKind) {
            if let Ok(mut rows) = self.0.lock() {
                rows.push(("phase", value));
            }
        }
        fn durable(&self, value: u64, _: Class) {
            if let Ok(mut rows) = self.0.lock() {
                rows.push(("durable", value));
            }
        }
        fn event(&self, event: &'static str, value: u64) {
            if let Ok(mut rows) = self.0.lock() {
                rows.push((event, value));
            }
        }
    }
    #[test]
    fn exhausted_budget_and_saturation_have_reserved_final_evidence() -> Result<()> {
        let recorder = Arc::new(Capture::default());
        let sink = Sink::new(
            &Config::default(),
            recorder.clone(),
            Arc::new(capacity::Observation::default()),
        );
        let state = sink.0.as_ref().ok_or_else(|| anyhow::anyhow!("state"))?;
        state.updates.store(UPDATE_LIMIT, Ordering::Relaxed);
        state.rejected.store(UPDATE_LIMIT, Ordering::Relaxed);
        sink.event(Event::Retry);
        ensure!(!sink.seal(true));
        ensure!(!sink.seal(true));
        let rows = recorder.0.lock().map_err(|_| anyhow::anyhow!("capture"))?;
        ensure!(
            rows.as_slice()
                == [
                    ("enabled", 1),
                    ("sample_limit", UPDATE_LIMIT),
                    ("sample_limit_saturated", 1),
                    ("sealed_failed", 1)
                ]
        );
        Ok(())
    }
    #[test]
    fn selector_no_match_and_reversed_clock_cannot_manufacture_success() -> Result<()> {
        let recorder = Arc::new(Capture::default());
        let config: Config = serde_json::from_value(
            serde_json::json!({"enabled":true,"correlation_id":"selected"}),
        )?;
        let sink = Sink::new(
            &config,
            recorder.clone(),
            Arc::new(super::super::capacity::Observation::default()),
        );
        ensure!(sink.timing(&"other".try_into()?).0.is_none());
        let before = Instant::now();
        let after = before + Duration::from_millis(1);
        sink.duration(after, before, |r, ns| {
            r.durable(ns, Class::CompleteCompleted)
        });
        ensure!(!sink.seal(true));
        let rows = recorder.0.lock().map_err(|_| anyhow::anyhow!("capture"))?;
        ensure!(rows.contains(&("clock_invalid", 1)));
        ensure!(!rows.iter().any(|r| r.0 == "durable"));
        Ok(())
    }
    #[test]
    fn retry_keeps_original_start_and_drop_is_never_an_ack() -> Result<()> {
        let recorder = Arc::new(Capture::default());
        let sink = Sink::new(
            &Config::default(),
            recorder.clone(),
            Arc::new(capacity::Observation::default()),
        );
        let timing = sink.timing(&"cohort".try_into()?);
        timing.admitted();
        timing.ready();
        timing.queued();
        let t = timing.0.as_ref().ok_or_else(|| anyhow::anyhow!("timing"))?;
        let start = t
            .ready
            .get()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("ready"))?;
        timing.event(Event::Retry);
        timing.ready();
        timing.dequeue();
        timing.dequeue();
        ensure!(t.ready.get() == Some(&start));
        drop(timing.span(Phase::SqlCommit));
        ensure!(!sink.seal(true));
        let rows = recorder.0.lock().map_err(|_| anyhow::anyhow!("capture"))?;
        ensure!(rows.iter().filter(|r| r.0 == "sql_unconfirmed").count() == 1);
        ensure!(!rows.iter().any(|r| r.0 == "sql_ack"));
        Ok(())
    }
}

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
#[test]
fn an_empty_selected_cohort_cannot_seal_cleanly() -> Result<()> {
    struct Empty;
    impl Recorder for Empty {
        fn phase(&self, _: u64, _: Phase, _: ResultKind) {}
        fn durable(&self, _: u64, _: Class) {}
        fn event(&self, _: &'static str, _: u64) {}
    }
    let config: Config =
        serde_json::from_value(serde_json::json!({"enabled":true,"correlation_id":"selected"}))?;
    let sink = Sink::new(
        &config,
        Arc::new(Empty),
        Arc::new(capacity::Observation::default()),
    );
    let other = sink.timing(&"warmup".try_into()?);
    other.admitted();
    other.ready();
    other.queued();
    ensure!(!sink.seal(true));
    Ok(())
}
