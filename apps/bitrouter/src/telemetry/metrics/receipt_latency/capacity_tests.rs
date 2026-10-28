//! Real SDK collection proves omitted attributes are not carried across callbacks.
use super::*;
use crate::telemetry::{
    processor_runtime::ProcessorRuntime,
    receipt_latency::capacity::{ReadError, Snapshot},
};
use anyhow::{Result, ensure};
use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::{
    SdkMeterProvider,
    data::{AggregatedMetrics, MetricData},
    periodic_reader_with_async_runtime::PeriodicReader,
};
use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
type Values = BTreeMap<(String, String, String), u64>;
#[derive(Clone, Debug, Default)]
struct Capture(Arc<Mutex<Vec<Values>>>);
impl opentelemetry_sdk::metrics::exporter::PushMetricExporter for Capture {
    async fn export(
        &self,
        metrics: &opentelemetry_sdk::metrics::data::ResourceMetrics,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        let mut values = Values::new();
        for scope in metrics.scope_metrics() {
            for metric in scope.metrics() {
                if let AggregatedMetrics::U64(MetricData::Gauge(gauge)) = metric.data() {
                    for point in gauge.data_points() {
                        let field = point
                            .attributes()
                            .find(|v| v.key.as_str() == "field")
                            .map(|v| v.value.as_str().into_owned())
                            .unwrap_or_default();
                        let checkpoint = point
                            .attributes()
                            .find(|v| v.key.as_str() == "checkpoint")
                            .map(|v| v.value.as_str().into_owned())
                            .unwrap_or_default();
                        values.insert((metric.name().to_owned(), checkpoint, field), point.value());
                    }
                }
            }
        }
        let mut packets = self.0.lock().map_err(|_| {
            opentelemetry_sdk::error::OTelSdkError::InternalFailure("test capture lock".into())
        })?;
        if packets.len() >= 8 {
            return Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
                "test capture bound".into(),
            ));
        }
        packets.push(values);
        Ok(())
    }
    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
    fn shutdown_with_timeout(&self, _: Duration) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
    fn temporality(&self) -> opentelemetry_sdk::metrics::Temporality {
        opentelemetry_sdk::metrics::Temporality::Cumulative
    }
}
impl Capture {
    fn last(&self) -> Result<Values> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("capture"))?
            .last()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no actual collection"))
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_callbacks_remove_missing_current_fields_and_repeat_frozen_values() -> Result<()> {
    let capture = Capture::default();
    let reader = PeriodicReader::builder(capture.clone(), ProcessorRuntime::new())
        .with_interval(Duration::from_secs(3600))
        .build();
    let provider = SdkMeterProvider::builder().with_reader(reader).build();
    let observation = Arc::new(Observation::default());
    let counter = Arc::new(AtomicUsize::new(0));
    let fail_read = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    let initial = Snapshot {
        completion_reserved: 0,
        accounted: 17_000,
        ingress_reserved: 0,
        potential_unknown: 0,
        live_unknown: 0,
        active: 0,
        queued: 0,
        ingress: 0,
        closed: false,
        fenced: false,
    };
    let (reads, failing, closing) = (counter.clone(), fail_read.clone(), closed.clone());
    ensure!(observation.bind(initial, move || {
        let read = reads.fetch_add(1, Ordering::Relaxed) + 1;
        if read == failing.load(Ordering::Relaxed) {
            return Err(ReadError::Unavailable);
        }
        Ok(Snapshot {
            closed: closing.load(Ordering::Relaxed),
            ..initial
        })
    }));
    let _instruments = Instruments::new(&provider.meter("bitrouter"), observation.clone());
    let checked = (|| -> Result<()> {
        provider.force_flush()?;
        ensure!(capture.last()?.len() == 22);
        let mut unavailable = Vec::new();
        for fail in [1, 2] {
            counter.store(0, Ordering::Relaxed);
            fail_read.store(fail, Ordering::Relaxed);
            provider.force_flush()?;
            let values = capture.last()?;
            let bytes = values
                .keys()
                .filter(|(name, c, _)| name.ends_with(".bytes") && c == "current")
                .count();
            let states = values
                .keys()
                .filter(|(name, c, _)| name.ends_with(".state") && c == "current")
                .count();
            ensure!(
                matches!((bytes, states), (0, 8) | (3, 1)),
                "stale fields survived a callback omission"
            );
            if states == 1 {
                ensure!(
                    values.get(&(
                        "bitrouter.receipt.capacity.state".into(),
                        "current".into(),
                        "available".into()
                    )) == Some(&0)
                );
            }
            unavailable.push((bytes, states));
        }
        ensure!(unavailable[0] != unavailable[1]);
        fail_read.store(0, Ordering::Relaxed);
        closed.store(true, Ordering::Relaxed);
        ensure!(observation.seal(true));
        let reads = counter.load(Ordering::Relaxed);
        provider.force_flush()?;
        let frozen = capture.last()?;
        ensure!(frozen.len() == 22 && frozen.keys().all(|(_, c, _)| c != "current"));
        provider.force_flush()?;
        ensure!(capture.last()? == frozen && counter.load(Ordering::Relaxed) == reads);
        Ok(())
    })();
    let shutdown = provider.shutdown();
    checked?;
    shutdown?;
    Ok(())
}
