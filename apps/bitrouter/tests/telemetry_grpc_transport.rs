//! Actual OTLP/gRPC delivery, separate from HTTP-preferred all-feature builds.
//!
//! Collector services use the generated official OTLP protocol:
//! https://github.com/open-telemetry/opentelemetry-proto/tree/main/opentelemetry/proto/collector

#![cfg(not(feature = "otel-http"))]

use std::sync::Arc;
use std::time::Duration;

use bitrouter::gateway::caller::CallerContext;
use bitrouter::gateway::pipeline::context::PipelineContext;
use bitrouter::gateway::pipeline::hooks::{ObserveHook, RequestOutcome};
use bitrouter::gateway::pipeline::types::PipelineRequest;
use bitrouter::telemetry::config::OtelConfig;
use bitrouter::telemetry::exporter::{OtelExporter, OtelObserveHook};
use bitrouter_ai::types::{GenerationParams, Prompt};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
    metrics_service_server::{MetricsService, MetricsServiceServer},
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
    trace_service_server::{TraceService, TraceServiceServer},
};
use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
use tokio::sync::{Mutex, oneshot};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status, metadata::MetadataMap};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Captured<T> {
    metadata: MetadataMap,
    payload: T,
}

#[derive(Clone, Default)]
struct Collector {
    traces: Arc<Mutex<Vec<Captured<ExportTraceServiceRequest>>>>,
    metrics: Arc<Mutex<Vec<Captured<ExportMetricsServiceRequest>>>>,
}

#[tonic::async_trait]
impl TraceService for Collector {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        self.traces.lock().await.push(Captured {
            metadata: request.metadata().clone(),
            payload: request.into_inner(),
        });
        Ok(Response::new(ExportTraceServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl MetricsService for Collector {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        self.metrics.lock().await.push(Captured {
            metadata: request.metadata().clone(),
            payload: request.into_inner(),
        });
        Ok(Response::new(ExportMetricsServiceResponse::default()))
    }
}

#[test]
fn grpc_only_delivers_spans_metrics_and_flushes() -> TestResult {
    const CHILD: &str = "BITROUTER_TEST_GRPC_COLLECTOR_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Exercise the real constructor without adopting an operator's exporter
        // endpoint or credentials. Command-local removal needs no unsafe global
        // environment mutation and keeps other tests independent.
        let mut child = std::process::Command::new(std::env::current_exe()?);
        child.args([
            "--exact",
            "grpc_only_delivers_spans_metrics_and_flushes",
            "--nocapture",
        ]);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("OTEL_") || name.starts_with("BITROUTER_TELEMETRY_") {
                child.env_remove(key);
            }
        }
        let status = child.env(CHILD, "1").status()?;
        if !status.success() {
            return Err(format!("gRPC collector subprocess failed: {status}").into());
        }
        return Ok(());
    }

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(collect_exports())
}

async fn collect_exports() -> TestResult {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let collector = Collector::default();
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(TraceServiceServer::new(collector.clone()))
            .add_service(MetricsServiceServer::new(collector.clone()))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = stopped.await;
            }),
    );

    let mut config = OtelConfig {
        endpoint: format!("http://{address}"),
        headers: [("x-collector-scope".into(), "contract-test".into())].into(),
        bearer_token: Some("collector-token".into()),
        ..Default::default()
    };
    // Delivery must be driven by shutdown, rather than a short periodic timer.
    config.traces.batch.flush_ms = 3_600_000;
    config.metrics.enabled = true;
    config.metrics.export_interval_ms = 3_600_000;
    let exporter = Arc::new(OtelExporter::new(config, None)?);
    let hook = OtelObserveHook::new(exporter.clone());
    let prompt = Prompt {
        model: "grpc-model".into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: Vec::new(),
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let ctx = PipelineContext::new(PipelineRequest::new(
        "grpc-model",
        CallerContext::new("test-key", "test-user"),
        prompt,
    ));
    hook.on_request_start(&ctx).await;
    hook.on_request_end(&ctx, &RequestOutcome::Completed).await;
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            exporter.shutdown();
            exporter.shutdown();
        }),
    )
    .await??;

    let traces = collector.traces.lock().await;
    let metrics = collector.metrics.lock().await;
    assert!(!traces.is_empty(), "shutdown did not deliver traces");
    assert!(!metrics.is_empty(), "shutdown did not deliver metrics");
    for metadata in traces
        .iter()
        .map(|item| &item.metadata)
        .chain(metrics.iter().map(|item| &item.metadata))
    {
        assert_eq!(
            metadata
                .get("x-collector-scope")
                .and_then(|v| v.to_str().ok()),
            Some("contract-test")
        );
        assert_eq!(
            metadata.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer collector-token")
        );
    }
    assert!(traces.iter().any(|item| {
        item.payload.resource_spans.iter().any(|resource| {
            resource.scope_spans.iter().any(|scope| {
                scope
                    .scope
                    .as_ref()
                    .is_some_and(|s| s.name == "io.bitrouter.observe")
                    && scope
                        .spans
                        .iter()
                        .any(|span| span.name == "chat grpc-model")
            })
        })
    }));
    assert!(metrics.iter().any(|item| {
        item.payload.resource_metrics.iter().any(|resource| {
            resource.scope_metrics.iter().any(|scope| {
                scope.scope.as_ref().is_some_and(|s| s.name == "bitrouter")
                    && scope.metrics.iter().any(|metric| {
                        metric.name == "bitrouter.requests"
                            && matches!(&metric.data, Some(metric::Data::Sum(sum)) if
                                sum.data_points.iter().any(|point|
                                    point.value == Some(number_data_point::Value::AsInt(1))))
                    })
            })
        })
    }));
    drop(traces);
    drop(metrics);
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(5), server).await???;
    Ok(())
}
