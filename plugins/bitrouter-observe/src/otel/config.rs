//! Configuration for OpenTelemetry exporter.

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// OpenTelemetry exporter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OtelConfig {
    /// OTLP endpoint. Defaults to OTEL_EXPORTER_OTLP_ENDPOINT env var.
    pub endpoint: String,
    
    /// Protocol: "http/protobuf" (default), "http/json", or "grpc".
    pub protocol: OtelProtocol,
    
    /// Additional headers for the OTLP exporter (e.g., API keys).
    pub headers: HashMap<String, String>,
    
    /// Trace configuration.
    pub traces: TraceConfig,
    
    /// Metrics configuration.
    pub metrics: MetricsConfig,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OtelProtocol {
    #[serde(rename = "http/protobuf")]
    HttpProtobuf,
    #[serde(rename = "http/json")]
    HttpJson,
    Grpc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceConfig {
    /// Include request/response bodies in spans (as events).
    pub include_bodies: bool,
    
    /// Batch configuration.
    pub batch: BatchConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BatchConfig {
    /// Maximum spans per batch.
    pub max_spans: usize,
    
    /// Flush interval in milliseconds.
    pub flush_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Enable metrics export.
    pub enabled: bool,
    
    /// Export interval in milliseconds.
    pub export_interval_ms: u64,
    
    /// Cardinality cap for api_key_id dimension.
    pub api_key_id_cap: usize,
    
    /// Cardinality cap for user_id dimension.
    pub user_id_cap: usize,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            endpoint: std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:4318".to_string()),
            protocol: OtelProtocol::default(),
            headers: HashMap::new(),
            traces: TraceConfig::default(),
            metrics: MetricsConfig::default(),
        }
    }
}

impl Default for OtelProtocol {
    fn default() -> Self {
        Self::HttpProtobuf
    }
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            include_bodies: false,
            batch: BatchConfig::default(),
        }
    }
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_spans: 512,
            flush_ms: 5000,
        }
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            export_interval_ms: 60000,
            api_key_id_cap: 1024,
            user_id_cap: 256,
        }
    }
}

impl OtelConfig {
    /// Apply environment variable overrides per OTel spec.
    pub fn with_env_overrides(mut self) -> Self {
        // OTEL_EXPORTER_OTLP_ENDPOINT takes precedence
        if let Ok(endpoint) = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
            self.endpoint = endpoint;
        }
        
        // OTEL_EXPORTER_OTLP_HEADERS adds headers
        if let Ok(headers_str) = std::env::var("OTEL_EXPORTER_OTLP_HEADERS") {
            for pair in headers_str.split(',') {
                if let Some((key, value)) = pair.split_once('=') {
                    self.headers.insert(key.trim().to_string(), value.trim().to_string());
                }
            }
        }
        
        // OTEL_EXPORTER_OTLP_PROTOCOL
        if let Ok(protocol) = std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL") {
            self.protocol = match protocol.as_str() {
                "http/protobuf" => OtelProtocol::HttpProtobuf,
                "http/json" => OtelProtocol::HttpJson,
                "grpc" => OtelProtocol::Grpc,
                _ => self.protocol,
            };
        }
        
        self
    }
}