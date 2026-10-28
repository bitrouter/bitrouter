//! Bounded actual OTLP receiver. No metrics are synthesized here.
//! https://github.com/open-telemetry/opentelemetry-proto/tree/main/opentelemetry/proto/collector
use super::*;
use opentelemetry_proto::tonic::{
    collector::metrics::v1::{ExportMetricsServiceRequest, ExportMetricsServiceResponse},
    metrics::v1::{HistogramDataPoint, metric, number_data_point},
};
use prost::Message;
use std::sync::{Mutex, atomic::AtomicBool};
use tokio_util::sync::CancellationToken;
const PACKET_LIMIT: usize = 128;
const BYTE_LIMIT: usize = 256 * 1024;
#[derive(Clone, Default)]
struct Capture {
    packets: Arc<Mutex<Vec<ExportMetricsServiceRequest>>>,
    invalid: Arc<AtomicBool>,
    refuse: Arc<AtomicBool>,
    #[cfg(feature = "otel-http")]
    http_ok: Arc<AtomicUsize>,
}
impl Capture {
    fn accept(&self, packet: ExportMetricsServiceRequest) -> Result<()> {
        ensure!(packet.encoded_len() <= BYTE_LIMIT);
        if self.refuse.load(Ordering::Relaxed) {
            anyhow::bail!("receiver refusal");
        }
        let mut packets = self
            .packets
            .lock()
            .map_err(|_| anyhow::anyhow!("receiver lock"))?;
        if packets.len() >= PACKET_LIMIT {
            self.invalid.store(true, Ordering::Relaxed);
            anyhow::bail!("bounded receiver exhausted");
        }
        packets.push(packet);
        Ok(())
    }
}
pub(in crate::tenancy::tests::execution) struct Collector {
    pub endpoint: String,
    capture: Capture,
    stop: CancellationToken,
    _release: tokio_util::sync::DropGuard,
    task: AbortOnDropHandle<Result<()>>,
}
impl Collector {
    pub async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let capture = Capture::default();
        let stop = CancellationToken::new();
        let release = stop.clone().drop_guard();
        let done = stop.clone();
        let state = capture.clone();
        #[cfg(feature = "otel-http")]
        let task = tokio::spawn(async move {
            async fn metrics(
                axum::extract::State(state): axum::extract::State<Capture>,
                request: axum::extract::Request,
            ) -> Response {
                let Ok(bytes) = axum::body::to_bytes(request.into_body(), BYTE_LIMIT).await else {
                    state.invalid.store(true, Ordering::Relaxed);
                    return StatusCode::PAYLOAD_TOO_LARGE.into_response();
                };
                let result = ExportMetricsServiceRequest::decode(bytes)
                    .map_err(anyhow::Error::from)
                    .and_then(|packet| state.accept(packet));
                if result.is_err() {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                state.http_ok.fetch_add(1, Ordering::SeqCst);
                (
                    [("content-type", "application/x-protobuf")],
                    ExportMetricsServiceResponse::default().encode_to_vec(),
                )
                    .into_response()
            }
            let router = Router::new()
                .route("/v1/metrics", post(metrics))
                .route("/v1/traces", post(|| async { StatusCode::OK }))
                .with_state(state);
            axum::serve(listener, router)
                .with_graceful_shutdown(done.cancelled_owned())
                .await?;
            Ok(())
        });
        #[cfg(all(feature = "otel-grpc", not(feature = "otel-http")))]
        let task = tokio::spawn(async move {
            use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::MetricsServiceServer;
            tonic::transport::Server::builder()
                .add_service(MetricsServiceServer::new(state).max_decoding_message_size(BYTE_LIMIT))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    done.cancelled_owned(),
                )
                .await?;
            Ok(())
        });
        Ok(Self {
            endpoint,
            capture,
            stop,
            _release: release,
            task: AbortOnDropHandle::new(task),
        })
    }
    #[cfg(feature = "otel-http")]
    pub fn accepted_http(&self) -> usize {
        self.capture.http_ok.load(Ordering::SeqCst)
    }
    #[cfg(feature = "otel-http")]
    pub fn packet(&self, index: usize) -> Result<ExportMetricsServiceRequest> {
        self.capture
            .packets
            .lock()
            .map_err(|_| anyhow::anyhow!("receiver lock"))?
            .get(index)
            .cloned()
            .context("actual received SDK packet")
    }
    pub fn refuse(&self) {
        self.capture.refuse.store(true, Ordering::Relaxed);
    }
    pub fn empty(&self) -> Result<bool> {
        Ok(self
            .capture
            .packets
            .lock()
            .map_err(|_| anyhow::anyhow!("receiver lock"))?
            .is_empty())
    }
    pub fn view(&self) -> Result<View> {
        ensure!(!self.capture.invalid.load(Ordering::Relaxed));
        let packets = self
            .capture
            .packets
            .lock()
            .map_err(|_| anyhow::anyhow!("receiver lock"))?;
        let mut view = View::default();
        for packet in packets.iter() {
            let mut capacity_batch = std::collections::BTreeMap::new();
            for resource in &packet.resource_metrics {
                for scope in &resource.scope_metrics {
                    for m in &scope.metrics {
                        if !m.name.starts_with("bitrouter.receipt.") {
                            continue;
                        }
                        ensure!(scope.scope.as_ref().is_some_and(|s| s.name == "bitrouter"));
                        match m.data.as_ref().context("metric data")? {
                            metric::Data::Sum(sum) => {
                                ensure!(
                                    m.name == "bitrouter.receipt.observation"
                                        && sum.aggregation_temporality == 2
                                        && sum.is_monotonic
                                );
                                for p in &sum.data_points {
                                    ensure!(
                                        p.attributes.len() == 1 && p.attributes[0].key == "event"
                                    );
                                    let key = attribute(&p.attributes[0])?;
                                    ensure!(
                                        [
                                            "enabled",
                                            "admitted",
                                            "first_queued",
                                            "retry",
                                            "queue_failure",
                                            "predeadline",
                                            "sql_ack",
                                            "sql_unconfirmed",
                                            "rollback_ack",
                                            "publication_failure",
                                            "caller_wait_failure",
                                            "unclassified",
                                            "clock_invalid",
                                            "range_overflow",
                                            "sealed_clean",
                                            "sealed_failed",
                                            "sample_limit",
                                            "sample_limit_saturated"
                                        ]
                                        .contains(&key.as_str()),
                                        "undeclared observation event"
                                    );
                                    let Some(number_data_point::Value::AsInt(n)) = p.value else {
                                        anyhow::bail!("integer counter required")
                                    };
                                    let n = u64::try_from(n)?;
                                    ensure!(n <= 100_000_000);
                                    if let Some(old) = view.events.insert(key, n) {
                                        ensure!(n >= old, "cumulative counter reset");
                                    }
                                }
                            }
                            metric::Data::Histogram(hist) => {
                                ensure!(hist.aggregation_temporality == 2 && m.unit == "ns");
                                for p in &hist.data_points {
                                    ensure!(p.explicit_bounds == expected_bounds());
                                    ensure!(
                                        p.bucket_counts.len() == 513
                                            && p.bucket_counts
                                                .iter()
                                                .try_fold(0u64, |n, v| n.checked_add(*v))
                                                == Some(p.count)
                                            && p.count <= 100_000_000
                                    );
                                    ensure!(
                                        p.bucket_counts.last() == Some(&0),
                                        "overflow bucket is not a fast sample"
                                    );
                                    let key =
                                        if m.name == "bitrouter.receipt.queue_ready_to_durable" {
                                            ensure!(
                                                p.attributes.len() == 1
                                                    && p.attributes[0].key == "receipt_class"
                                            );
                                            format!("durable:{}", attribute(&p.attributes[0])?)
                                        } else {
                                            ensure!(
                                                m.name == "bitrouter.receipt.phase.duration"
                                                    && p.attributes.len() == 2
                                            );
                                            let phase = p
                                                .attributes
                                                .iter()
                                                .find(|a| a.key == "phase")
                                                .context("phase dimension")?;
                                            let result = p
                                                .attributes
                                                .iter()
                                                .find(|a| a.key == "result")
                                                .context("result dimension")?;
                                            format!("{}:{}", attribute(phase)?, attribute(result)?)
                                        };
                                    ensure!(
                                        [
                                            "durable:complete_completed",
                                            "durable:terminal_other",
                                            "durable:evidence_unknown",
                                            "first_queue:observed",
                                            "prepare:observed",
                                            "prepare:failed",
                                            "prepare:unconfirmed",
                                            "transaction:acknowledged",
                                            "transaction:failed",
                                            "transaction:unconfirmed",
                                            "sql_commit:acknowledged",
                                            "sql_commit:unconfirmed"
                                        ]
                                        .contains(&key.as_str()),
                                        "undeclared receipt histogram series"
                                    );
                                    if let Some(old) = view.histograms.insert(key, p.clone()) {
                                        ensure!(
                                            p.count >= old.count
                                                && p.bucket_counts
                                                    .iter()
                                                    .zip(&old.bucket_counts)
                                                    .all(|(n, old)| n >= old),
                                            "cumulative histogram reset"
                                        );
                                    }
                                }
                            }
                            metric::Data::Gauge(gauge) => {
                                let fields: &[&str] = match (m.name.as_str(), m.unit.as_str()) {
                                    ("bitrouter.receipt.capacity.bytes", "By") => {
                                        &["completion_reserved", "accounted", "ingress_reserved"]
                                    }
                                    ("bitrouter.receipt.capacity.state", "1") => &[
                                        "potential_unknown",
                                        "live_unknown",
                                        "active",
                                        "queued",
                                        "ingress",
                                        "closed",
                                        "fenced",
                                        "available",
                                    ],
                                    _ => anyhow::bail!("unexpected gauge"),
                                };
                                ensure!(gauge.data_points.len() <= fields.len() * 3);
                                for point in &gauge.data_points {
                                    ensure!(
                                        point.attributes.len() == 2
                                            && point.flags == 0
                                            && point.time_unix_nano > 0
                                    );
                                    let field = attribute(
                                        point
                                            .attributes
                                            .iter()
                                            .find(|a| a.key == "field")
                                            .context("field")?,
                                    )?;
                                    let checkpoint = attribute(
                                        point
                                            .attributes
                                            .iter()
                                            .find(|a| a.key == "checkpoint")
                                            .context("checkpoint")?,
                                    )?;
                                    ensure!(
                                        fields.contains(&field.as_str())
                                            && ["initial", "current", "final"]
                                                .contains(&checkpoint.as_str())
                                    );
                                    let Some(number_data_point::Value::AsInt(n)) = point.value
                                    else {
                                        anyhow::bail!("integer gauge required")
                                    };
                                    let value = u64::try_from(n)?;
                                    let key = format!("{checkpoint}:{field}");
                                    ensure!(capacity_batch.insert(key.clone(), value).is_none());
                                    if checkpoint == "current" {
                                        view.current.insert(field, value);
                                    } else if let Some(old) = view.immutable.insert(key, value) {
                                        ensure!(old == value, "immutable capacity conflict");
                                    }
                                }
                            }
                            _ => anyhow::bail!("unexpected metric kind"),
                        }
                    }
                }
            }
            let required = ["initial", "final"].into_iter().flat_map(|c| {
                [
                    "completion_reserved",
                    "accounted",
                    "ingress_reserved",
                    "potential_unknown",
                    "live_unknown",
                    "active",
                    "queued",
                    "ingress",
                    "closed",
                    "fenced",
                    "available",
                ]
                .map(|f| format!("{c}:{f}"))
            });
            if required
                .clone()
                .all(|key| capacity_batch.contains_key(&key))
            {
                view.frozen = Some(
                    required
                        .map(|key| {
                            let value = capacity_batch[&key];
                            (key, value)
                        })
                        .collect(),
                );
            }
        }
        Ok(view)
    }
    pub async fn close(self) -> Result<()> {
        self.stop.cancel();
        tokio::time::timeout(TIMEOUT, self.task).await???;
        Ok(())
    }
}
#[cfg(all(feature = "otel-grpc", not(feature = "otel-http")))]
#[tonic::async_trait]
impl opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::MetricsService
    for Capture
{
    async fn export(
        &self,
        request: tonic::Request<ExportMetricsServiceRequest>,
    ) -> std::result::Result<tonic::Response<ExportMetricsServiceResponse>, tonic::Status> {
        self.accept(request.into_inner())
            .map_err(|_| tonic::Status::invalid_argument("bounded receiver refused"))?;
        Ok(tonic::Response::new(ExportMetricsServiceResponse::default()))
    }
}
fn attribute(value: &opentelemetry_proto::tonic::common::v1::KeyValue) -> Result<String> {
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    match value.value.as_ref().and_then(|v| v.value.as_ref()) {
        Some(Value::StringValue(s)) => Ok(s.clone()),
        _ => anyhow::bail!("string dimension required"),
    }
}
fn expected_bounds() -> Vec<f64> {
    let mut bounds: Vec<f64> = (0..=500).map(|n| f64::from(n) * 100_000.0).collect();
    bounds.extend(
        [
            75., 100., 250., 500., 1_000., 2_000., 5_000., 10_000., 30_000., 60_000., 120_000.,
        ]
        .map(|ms| ms * 1_000_000.),
    );
    bounds
}
#[derive(Default)]
pub(in crate::tenancy::tests::execution) struct View {
    pub events: std::collections::BTreeMap<String, u64>,
    pub current: std::collections::BTreeMap<String, u64>,
    pub immutable: std::collections::BTreeMap<String, u64>,
    pub frozen: Option<std::collections::BTreeMap<String, u64>>,
    pub histograms: std::collections::BTreeMap<String, HistogramDataPoint>,
}
impl View {
    pub fn event(&self, name: &str) -> u64 {
        self.events.get(name).copied().unwrap_or(0)
    }
    pub fn count(&self, name: &str) -> u64 {
        self.histograms.get(name).map_or(0, |h| h.count)
    }
    pub fn sum(&self, name: &str) -> Result<f64> {
        self.histograms
            .get(name)
            .and_then(|h| h.sum)
            .context("nonempty histogram sum")
    }
}

fn rank99(count: u64) -> Result<u64> {
    ensure!(
        count > 0 && count <= 100_000_000,
        "empty or unbounded cohort"
    );
    Ok(count
        .checked_mul(99)
        .context("rank multiplication")?
        .div_ceil(100))
}
impl View {
    pub fn p99_upper(&self, name: &str) -> Result<f64> {
        let h = self.histograms.get(name).context("nonempty histogram")?;
        let rank = rank99(h.count)?;
        let mut total = 0u64;
        for (i, n) in h.bucket_counts.iter().enumerate() {
            total = total.checked_add(*n).context("bucket count overflow")?;
            if total >= rank {
                return h
                    .explicit_bounds
                    .get(i)
                    .copied()
                    .context("p99 overflow bucket");
            }
        }
        anyhow::bail!("incomplete buckets")
    }
}
#[test]
fn conservative_rank_and_empty_cohort_refusal() -> Result<()> {
    for (n, rank) in [(1, 1), (99, 99), (100, 99), (101, 100)] {
        ensure!(rank99(n)? == rank);
    }
    ensure!(rank99(0).is_err() && rank99(u64::MAX).is_err());
    Ok(())
}
