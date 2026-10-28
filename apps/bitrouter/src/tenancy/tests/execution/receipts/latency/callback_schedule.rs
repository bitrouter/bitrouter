//! Real registered SDK callbacks, real Admission contention, real OTLP/HTTP.
//! No metric point is constructed or rearranged to manufacture either packet.
use super::*;
use crate::telemetry::receipt_latency::capacity::ReadError;
use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

pub(super) async fn real_callbacks_contend_and_straddle_seal() -> Result<()> {
    // Cover both a readable first callback and an actually contended first
    // callback. No assumption about which gauge the SDK visits first is needed.
    for contend in [false, true] {
        let collector = Collector::start().await?;
        let (exporter, sink) = exporter(&collector, 3_600_000, None)?;
        let h = harness(sink.clone(), None).await?;
        let mut stopped = false;
        let checked = async {
            completed(&h).await?;
            h.fixture.service.shutdown().await?;
            stopped = true;
            h.fixture.service.cleanup.ensure_clean()?;
            let accounted = capacity::stored_policy_bytes(&h).await?;
            let observation = sink
                .capacity_for_test()
                .context("enabled capacity observer")?;
            let weak_observation = Arc::downgrade(&observation);
            let admission = Arc::downgrade(&h.fixture.service.admission);
            let calls = Arc::new(AtomicUsize::new(0));
            let busy = Arc::new(AtomicBool::new(false));
            let sealed = Arc::new(AtomicBool::new(false));
            let failed = Arc::new(AtomicBool::new(false));
            let (called, contended, seal, failure) =
                (calls.clone(), busy.clone(), sealed.clone(), failed.clone());
            ensure!(
                observation.schedule_collection_for_test(Box::new(move |collect| {
                    let ordinal = called.fetch_add(1, Ordering::SeqCst);
                    if ordinal == 0 && contend {
                        // try_lock in the registered callback runs while THIS real
                        // Admission mutex is held. The actual result is returned
                        // untouched, so SDK omission/unavailable encoding is tested.
                        if let Some(admission) = admission.upgrade()
                            && let Ok(value) = admission.with_observation_lock_for_test(collect)
                        {
                            contended.store(
                                value[1] == Some(("current", Err(ReadError::Unavailable))),
                                Ordering::SeqCst,
                            );
                            return value;
                        }
                        failure.store(true, Ordering::SeqCst);
                    }
                    if ordinal == 1 {
                        // The first callback and its mutex guard have returned.
                        // This freezes the real closed admission before callback 2.
                        seal.store(
                            weak_observation.upgrade().is_some_and(|o| o.seal(true)),
                            Ordering::SeqCst,
                        );
                    }
                    collect()
                }))
            );
            ensure!(collector.empty()?);
            // One fixed deadline covers both exports and all intervening checks;
            // a partial does not reset it. The existing parent watchdog is 110s.
            let deadline = tokio::time::Instant::now() + TIMEOUT;
            collect_until(&exporter, deadline).await?;
            ensure!(
                calls.load(Ordering::SeqCst) == 2,
                "both registered callbacks must run"
            );
            ensure!(busy.load(Ordering::SeqCst) == contend);
            ensure!(sealed.load(Ordering::SeqCst) && !failed.load(Ordering::SeqCst));
            ensure!(
                collector.accepted_http() == 1,
                "actual OTLP HTTP 200 was required"
            );
            let partial = capacity_points(&collector.packet(0)?)?;
            let initial: BTreeMap<_, _> = partial
                .iter()
                .filter(|(key, _)| key.starts_with("initial:"))
                .map(|(key, value)| (key.clone(), *value))
                .collect();
            ensure!(initial.len() == 11);
            let finals = partial
                .keys()
                .filter(|key| key.starts_with("final:"))
                .count();
            ensure!(
                matches!(finals, 3 | 8),
                "seal did not straddle the SDK callbacks"
            );
            if contend {
                // Bytes omit all current values; state emits only available=0.
                let current: Vec<_> = partial
                    .iter()
                    .filter(|(key, _)| key.starts_with("current:"))
                    .collect();
                ensure!(
                    current.is_empty()
                        || (current.len() == 1
                            && current[0].0.as_str() == "current:available"
                            && *current[0].1 == 0)
                );
            } else {
                let current = partial
                    .keys()
                    .filter(|key| key.starts_with("current:"))
                    .count();
                ensure!(current + finals == 11);
            }
            ensure!(
                collector.view()?.frozen.is_none(),
                "a legitimate partial is incomplete"
            );
            // Capacity sealing and timing sealing remain distinct operations.
            ensure!(sink.seal(true));
            collect_until(&exporter, deadline).await?;
            ensure!(calls.load(Ordering::SeqCst) == 4 && collector.accepted_http() == 2);
            let complete = capacity_points(&collector.packet(1)?)?;
            ensure!(
                complete.len() == 22 && complete.keys().all(|key| !key.starts_with("current:"))
            );
            for (key, value) in partial
                .iter()
                .filter(|(key, _)| !key.starts_with("current:"))
            {
                ensure!(
                    complete.get(key) == Some(value),
                    "partial immutable facts changed"
                );
            }
            ensure!(
                initial
                    .iter()
                    .all(|(key, value)| complete.get(key) == Some(value))
            );
            let view = collector.view()?;
            // The receiver requires all 22 facts in ONE real packet. It must
            // recover from the valid partial, without combining current/final.
            capacity::assert_frozen(&view, 0, accounted, 0)?;
            ensure!(view.frozen.as_ref() == Some(&complete));
            ensure!(view.event("sealed_clean") == 1 && view.event("sealed_failed") == 0);
            ensure!(h.calls.load(Ordering::SeqCst) == 1);
            ensure!(tokio::time::Instant::now() < deadline);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        // Always finish the original exporter and receiver, including failed
        // assertions. There is no detached test worker or additional reader.
        let shutdown = if stopped {
            h.fixture.service.cleanup.ensure_clean()
        } else {
            h.fixture.service.shutdown().await
        };
        let exported = flush(&exporter).await;
        let closed = collector.close().await;
        checked?;
        shutdown?;
        ensure!(exported?);
        closed?;
    }
    Ok(())
}

pub(in crate::tenancy::tests::execution) async fn collect_until(
    exporter: &Arc<OtelExporter>,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let owner = exporter.clone();
    // The SDK's synchronous force_flush waits for the real HTTP response; use
    // the existing blocking execution facility, not an async runtime worker.
    let mut collect = tokio::task::spawn_blocking(move || owner.collect_receipts_for_test());
    match tokio::time::timeout_at(deadline, &mut collect).await {
        Ok(result) => result??,
        Err(expired) => {
            // Retain and join the SAME blocking owner even on failure. A late
            // successful export cannot turn the expired case into a pass; the
            // unchanged 110s parent watchdog bounds a stuck SDK/cleanup owner.
            let settled = collect.await;
            settled.context("expired collection owner failed to join")??;
            return Err(expired.into());
        }
    }
    Ok(())
}

pub(in crate::tenancy::tests::execution) fn capacity_points(
    packet: &opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest,
) -> Result<BTreeMap<String, u64>> {
    use opentelemetry_proto::tonic::common::v1::any_value;
    let mut points = BTreeMap::new();
    let mut scopes = 0;
    for resource in &packet.resource_metrics {
        for scope in &resource.scope_metrics {
            if scope
                .scope
                .as_ref()
                .is_none_or(|scope| scope.name != "bitrouter")
            {
                continue;
            }
            scopes += 1;
            for metric in &scope.metrics {
                if !metric.name.starts_with("bitrouter.receipt.capacity.") {
                    continue;
                }
                let Some(metric::Data::Gauge(gauge)) = &metric.data else {
                    anyhow::bail!("actual capacity gauge missing");
                };
                for point in &gauge.data_points {
                    let attr = |key: &str| -> Result<&str> {
                        let value = point
                            .attributes
                            .iter()
                            .find(|a| a.key == key)
                            .and_then(|a| a.value.as_ref())
                            .and_then(|v| v.value.as_ref());
                        let Some(any_value::Value::StringValue(value)) = value else {
                            anyhow::bail!("actual capacity attribute missing");
                        };
                        Ok(value.as_str())
                    };
                    let Some(number_data_point::Value::AsInt(value)) = point.value else {
                        anyhow::bail!("actual capacity integer missing");
                    };
                    let key = format!("{}:{}", attr("checkpoint")?, attr("field")?);
                    ensure!(points.insert(key, u64::try_from(value)?).is_none());
                }
            }
        }
    }
    ensure!(
        scopes == 1,
        "a single actual resource/meter batch is required"
    );
    Ok(points)
}
