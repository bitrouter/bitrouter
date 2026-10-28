//! Fixed instruments on the existing provider, reader and transport.
use crate::telemetry::receipt_latency::{
    Class, Phase, Recorder, ResultKind, capacity::Observation,
};
use opentelemetry::{
    KeyValue,
    metrics::{Counter, Histogram, Meter, ObservableGauge},
};
use std::sync::Arc;

pub(super) struct Instruments {
    durable: Histogram<u64>,
    phase: Histogram<u64>,
    events: Counter<u64>,
    _capacity_bytes: ObservableGauge<u64>,
    _capacity_state: ObservableGauge<u64>,
}
pub(crate) fn boundaries() -> Vec<f64> {
    (0..=500)
        .map(|n| f64::from(n) * 100_000.0)
        .chain([
            75_000_000.0,
            100_000_000.0,
            250_000_000.0,
            500_000_000.0,
            1_000_000_000.0,
            2_000_000_000.0,
            5_000_000_000.0,
            10_000_000_000.0,
            30_000_000_000.0,
            60_000_000_000.0,
            120_000_000_000.0,
        ])
        .collect()
}
impl Instruments {
    pub(super) fn new(meter: &Meter, capacity: Arc<Observation>) -> Self {
        let byte_capacity = capacity.clone();
        // Explicit bounds, inclusive upper edges; no fake zero warmup samples.
        // https://opentelemetry.io/docs/specs/otel/metrics/data-model/#histogram
        Self {
            _capacity_bytes: meter
                .u64_observable_gauge("bitrouter.receipt.capacity.bytes")
                .with_unit("By")
                .with_description("Admission policy byte accounting snapshots")
                .with_callback(move |observer| {
                    for (checkpoint, value) in byte_capacity.collect().into_iter().flatten() {
                        if let Ok(snapshot) = value {
                            for (field, value) in snapshot.bytes() {
                                observer.observe(
                                    value,
                                    &[
                                        KeyValue::new("field", field),
                                        KeyValue::new("checkpoint", checkpoint),
                                    ],
                                );
                            }
                        }
                    }
                })
                .build(),
            _capacity_state: meter
                .u64_observable_gauge("bitrouter.receipt.capacity.state")
                .with_unit("1")
                .with_description("Admission and durable Unknown state snapshots")
                .with_callback(move |observer| {
                    for (checkpoint, value) in capacity.collect().into_iter().flatten() {
                        match value {
                            Ok(snapshot) => {
                                for (field, value) in snapshot.state() {
                                    observer.observe(
                                        value,
                                        &[
                                            KeyValue::new("field", field),
                                            KeyValue::new("checkpoint", checkpoint),
                                        ],
                                    );
                                }
                            }
                            Err(_) => observer.observe(
                                0,
                                &[
                                    KeyValue::new("field", "available"),
                                    KeyValue::new("checkpoint", checkpoint),
                                ],
                            ),
                        }
                    }
                })
                .build(),
            durable: meter
                .u64_histogram("bitrouter.receipt.queue_ready_to_durable")
                .with_unit("ns")
                .with_description("First terminal queue to published durable receipt")
                .with_boundaries(boundaries())
                .build(),
            phase: meter
                .u64_histogram("bitrouter.receipt.phase.duration")
                .with_unit("ns")
                .with_description("Receipt custody phase duration")
                .with_boundaries(boundaries())
                .build(),
            events: meter
                .u64_counter("bitrouter.receipt.observation")
                .with_unit("1")
                .with_description("Bounded receipt observation lifecycle facts")
                .build(),
        }
    }
}
impl Recorder for Instruments {
    fn phase(&self, value: u64, phase: Phase, result: ResultKind) {
        self.phase.record(
            value,
            &[
                KeyValue::new("phase", phase.label()),
                KeyValue::new("result", result.label()),
            ],
        );
    }
    fn durable(&self, value: u64, class: Class) {
        self.durable
            .record(value, &[KeyValue::new("receipt_class", class.label())]);
    }
    fn event(&self, event: &'static str, value: u64) {
        self.events.add(value, &[KeyValue::new("event", event)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_boundaries_do_not_round_across_twenty_milliseconds() {
        let edges = boundaries();
        assert_eq!(edges.len(), 512);
        assert_eq!(edges[200], 20_000_000.0);
        assert!(edges.windows(2).all(|p| p[0] < p[1]));
        assert_eq!(edges.partition_point(|b| *b < 19_999_999.0), 200);
        assert_eq!(edges.partition_point(|b| *b < 20_000_000.0), 200);
        assert_eq!(edges.partition_point(|b| *b < 20_000_001.0), 201);
        assert_eq!(edges.last().copied(), Some(120_000_000_000.0));
    }
}

#[cfg(test)]
mod capacity_tests;
