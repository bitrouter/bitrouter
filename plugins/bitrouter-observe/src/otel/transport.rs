//! OTLP wire-transport selection.
//!
//! The OpenTelemetry stack ships several transports behind upstream
//! `opentelemetry-otlp` features. This crate surfaces two of them as cargo
//! features so a build only compiles the dependency stack it uses:
//!
//! - `otel-http` — OTLP/HTTP + protobuf over reqwest + rustls
//!   (`opentelemetry-otlp/{http-proto,reqwest-client,reqwest-rustls}`).
//! - `otel-grpc` — OTLP/gRPC over tonic + rustls
//!   (`opentelemetry-otlp/{grpc-tonic,tls-roots}`).
//!
//! Both can be compiled in at once (e.g. under `--all-features`); when they
//! are, **HTTP takes precedence** so the default OTLP endpoint
//! (`http://localhost:4318`) keeps working out of the box. To run gRPC,
//! compile only `otel-grpc` (`--no-default-features --features otel-grpc`).
//! `with_endpoint`/header forwarding behave identically across the two, so a
//! deterministic winner keeps behaviour predictable. The `otel` module only
//! compiles when `otel-base` is on, and `lib.rs` guarantees at least one
//! transport accompanies it, so exactly one `imp` below is ever active.
//!
//! Both builders forward [`OtelConfig::endpoint`] and [`OtelConfig::headers`].
//! HTTP carries the headers as HTTP headers; gRPC carries them as request
//! metadata (the wire equivalent).

use crate::otel::config::OtelConfig;

#[cfg(all(feature = "otel-grpc", not(feature = "otel-http")))]
mod imp {
    use super::OtelConfig;

    use opentelemetry_otlp::{MetricExporter, SpanExporter, WithExportConfig, WithTonicConfig};
    use tonic::metadata::MetadataMap;

    /// Translate the configured headers into gRPC metadata. Invalid header
    /// names/values are skipped rather than failing the whole build of the
    /// exporter — observability must never take down request processing, and a
    /// malformed custom header should not be fatal.
    fn metadata(config: &OtelConfig) -> MetadataMap {
        let mut headers = http::HeaderMap::new();
        for (k, v) in &config.headers {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(k.as_bytes()),
                http::HeaderValue::from_str(v),
            ) {
                headers.insert(name, value);
            }
        }
        MetadataMap::from_headers(headers)
    }

    pub(crate) fn span_exporter(
        config: &OtelConfig,
    ) -> Result<SpanExporter, Box<dyn std::error::Error>> {
        Ok(SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&config.endpoint)
            .with_metadata(metadata(config))
            .build()?)
    }

    pub(crate) fn metric_exporter(
        config: &OtelConfig,
    ) -> Result<MetricExporter, Box<dyn std::error::Error>> {
        Ok(MetricExporter::builder()
            .with_tonic()
            .with_endpoint(&config.endpoint)
            .with_metadata(metadata(config))
            .build()?)
    }
}

#[cfg(feature = "otel-http")]
mod imp {
    use super::OtelConfig;

    use opentelemetry_otlp::{MetricExporter, SpanExporter, WithExportConfig, WithHttpConfig};

    pub(crate) fn span_exporter(
        config: &OtelConfig,
    ) -> Result<SpanExporter, Box<dyn std::error::Error>> {
        Ok(SpanExporter::builder()
            .with_http()
            .with_endpoint(&config.endpoint)
            .with_headers(config.headers.clone())
            .build()?)
    }

    pub(crate) fn metric_exporter(
        config: &OtelConfig,
    ) -> Result<MetricExporter, Box<dyn std::error::Error>> {
        Ok(MetricExporter::builder()
            .with_http()
            .with_endpoint(&config.endpoint)
            .with_headers(config.headers.clone())
            .build()?)
    }
}

pub(crate) use imp::{metric_exporter, span_exporter};
