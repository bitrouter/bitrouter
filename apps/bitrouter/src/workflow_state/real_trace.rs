use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use bitrouter_sdk::Result;
use chrono::{SecondsFormat, Utc};
use http::HeaderValue;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::workflow_state::fixture::WorkflowTraceFixture;
use crate::workflow_state::ir::{HarnessId, ProtocolKind};
use crate::workflow_state::replay::extract_fixture_ir;

const MAX_CAPTURE_BODY_BYTES: usize = 16 * 1024 * 1024;
const BITROUTER_REQUEST_ID_HEADER: &str = "x-bitrouter-request-id";
const BITROUTER_HARNESS_HEADER: &str = "x-bitrouter-harness";
const BITROUTER_PROTOCOL_HEADER: &str = "x-bitrouter-protocol";

#[derive(Debug, Clone)]
pub struct TraceCaptureOptions {
    pub harness: HarnessId,
    pub session_header: Option<String>,
    pub archive_path: Option<PathBuf>,
}

impl Default for TraceCaptureOptions {
    fn default() -> Self {
        Self {
            harness: HarnessId::Generic,
            session_header: Some("x-bitrouter-workflow-session".to_string()),
            archive_path: None,
        }
    }
}

#[derive(Clone)]
pub struct RealTraceCapture {
    inner: Arc<CaptureInner>,
}

struct CaptureInner {
    options: TraceCaptureOptions,
    records: Mutex<Vec<CapturedIngressTrace>>,
    archive_lock: Mutex<()>,
    next_id: AtomicU64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedIngressTrace {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    pub harness: HarnessId,
    pub protocol: ProtocolKind,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub raw_body: serde_json::Value,
    pub outcome: RealTraceOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealTraceOutcome {
    pub http_status: u16,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct TraceSanitizer {
    allowed_headers: BTreeSet<String>,
}

impl Default for TraceSanitizer {
    fn default() -> Self {
        let allowed_headers = [
            "anthropic-beta",
            "content-type",
            "openai-beta",
            "originator",
            "user-agent",
            "x-bitrouter-agent",
            "x-bitrouter-cloud-request-id",
            "x-bitrouter-harness",
            "x-bitrouter-inbound-protocol",
            "x-bitrouter-protocol",
            "x-bitrouter-request-id",
            "x-bitrouter-workflow-session",
            "x-request-id",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        Self { allowed_headers }
    }
}

impl RealTraceCapture {
    pub fn new(options: TraceCaptureOptions) -> Self {
        Self {
            inner: Arc::new(CaptureInner {
                options,
                records: Mutex::new(Vec::new()),
                archive_lock: Mutex::new(()),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    pub fn router_wrapper(&self) -> impl Fn(Router) -> Router + Clone + Send + Sync + 'static {
        let capture = self.clone();
        move |router: Router| {
            let capture = capture.clone();
            router.layer(middleware::from_fn(move |req: Request, next: Next| {
                capture.clone().capture_request(req, next)
            }))
        }
    }

    pub fn records(&self) -> Vec<CapturedIngressTrace> {
        self.inner
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn replay_fixtures(&self) -> Result<Vec<WorkflowTraceFixture>> {
        self.records()
            .into_iter()
            .map(|trace| {
                trace
                    .to_replay_fixture_json(&TraceSanitizer::default())
                    .and_then(WorkflowTraceFixture::from_value)
            })
            .collect()
    }

    async fn capture_request(self, req: Request, next: Next) -> Response {
        let (mut parts, body) = req.into_parts();
        let method = parts.method.to_string();
        let path = parts.uri.path().to_string();
        let request_id = request_id_from_headers(&parts.headers).unwrap_or_else(|| self.next_id());
        if let Ok(value) = HeaderValue::from_str(&request_id) {
            parts.headers.insert(BITROUTER_REQUEST_ID_HEADER, value);
        }
        let protocol = protocol_for_path(&path);
        if !parts.headers.contains_key(BITROUTER_HARNESS_HEADER)
            && let Ok(value) =
                HeaderValue::from_str(harness_header_value(&self.inner.options.harness))
        {
            parts.headers.insert(BITROUTER_HARNESS_HEADER, value);
        }
        if !parts.headers.contains_key(BITROUTER_PROTOCOL_HEADER)
            && let Some(protocol) = protocol.as_ref()
            && let Ok(value) = HeaderValue::from_str(protocol_header_value(protocol))
        {
            parts.headers.insert(BITROUTER_PROTOCOL_HEADER, value);
        }
        let headers = headers_to_map(&parts.headers);

        let body_bytes = match to_bytes(body, MAX_CAPTURE_BODY_BYTES).await {
            Ok(bytes) => bytes,
            Err(e) => {
                let req = Request::from_parts(parts, Body::empty());
                let response = next.run(req).await;
                tracing::warn!(%method, %path, %e, "workflow trace capture skipped unreadable body");
                return response;
            }
        };
        let raw_body = serde_json::from_slice::<serde_json::Value>(&body_bytes).ok();
        let req = Request::from_parts(parts, Body::from(body_bytes));
        let response = next.run(req).await;

        if let (Some(protocol), Some(raw_body)) = (protocol, raw_body) {
            self.push_record(CapturedIngressTrace {
                id: request_id,
                captured_at: Some(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
                harness: self.inner.options.harness.clone(),
                protocol,
                method,
                path,
                headers,
                raw_body,
                outcome: RealTraceOutcome {
                    http_status: response.status().as_u16(),
                    status: if response.status().is_success() {
                        "completed".to_string()
                    } else {
                        "failed".to_string()
                    },
                },
            });
        }

        response
    }

    fn next_id(&self) -> String {
        let n = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        format!("real-agent-trace-{n:04}")
    }

    fn push_record(&self, record: CapturedIngressTrace) {
        if let Some(path) = self.inner.options.archive_path.as_ref() {
            let _guard = self
                .inner
                .archive_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(e) = append_sanitized_trace(path, &record, &TraceSanitizer::default()) {
                tracing::warn!(path = %path.display(), error = %e, "workflow trace archive append failed");
            }
        }
        self.inner
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(record);
    }
}

pub const WORKFLOW_TRACE_JSONL_ENV: &str = "BITROUTER_WORKFLOW_TRACE_JSONL";
pub const WORKFLOW_TRACE_HARNESS_ENV: &str = "BITROUTER_WORKFLOW_TRACE_HARNESS";

pub fn capture_from_env() -> Result<Option<RealTraceCapture>> {
    let Some(path) = std::env::var_os(WORKFLOW_TRACE_JSONL_ENV).filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let harness = match std::env::var(WORKFLOW_TRACE_HARNESS_ENV)
        .unwrap_or_else(|_| "generic".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "generic" => HarnessId::Generic,
        "hermes" => HarnessId::Hermes,
        "claude" | "claude_code" | "claude-code" => HarnessId::ClaudeCode,
        "codex" => HarnessId::Codex,
        "openclaw" | "open_claw" | "open-claw" => HarnessId::OpenClaw,
        "unknown" => HarnessId::Unknown,
        other => {
            return Err(bitrouter_sdk::BitrouterError::bad_request(format!(
                "{WORKFLOW_TRACE_HARNESS_ENV} has unsupported value '{other}'"
            )));
        }
    };

    Ok(Some(RealTraceCapture::new(TraceCaptureOptions {
        harness,
        session_header: Some("x-bitrouter-workflow-session".to_string()),
        archive_path: Some(PathBuf::from(path)),
    })))
}

impl CapturedIngressTrace {
    pub fn to_replay_fixture_json(&self, sanitizer: &TraceSanitizer) -> Result<serde_json::Value> {
        let mut fixture_json = json!({
            "id": self.id,
            "harness": self.harness,
            "protocol": self.protocol,
            "headers": sanitizer.sanitize_headers(&self.headers),
            "raw_body": self.raw_body,
            "capture": {
                "source": "real_agent_http",
                "method": self.method,
                "path": self.path,
                "captured_at": self.captured_at,
                "outcome": self.outcome,
            },
            "expected": {
                "state_kind": "unknown",
                "baseline_fingerprint": "unknown",
                "confidence_min": 0.0
            }
        });

        let preliminary = WorkflowTraceFixture::from_value(fixture_json.clone())?;
        let ir = extract_fixture_ir(&preliminary);
        fixture_json["expected"] = json!({
            "state_kind": ir.state_kind,
            "baseline_fingerprint": preliminary.baseline_fingerprint(),
            "confidence_min": 0.0
        });
        Ok(fixture_json)
    }
}

impl TraceSanitizer {
    pub fn sanitize_trace(&self, trace: &CapturedIngressTrace) -> CapturedIngressTrace {
        let mut sanitized = trace.clone();
        sanitized.headers = self.sanitize_headers(&trace.headers);
        sanitized
    }

    pub fn sanitize_headers(&self, headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        headers
            .iter()
            .filter_map(|(name, value)| {
                let normalized = name.to_ascii_lowercase();
                self.allowed_headers
                    .contains(&normalized)
                    .then(|| (normalized, value.clone()))
            })
            .collect()
    }
}

fn protocol_for_path(path: &str) -> Option<ProtocolKind> {
    match path {
        "/v1/chat/completions" => Some(ProtocolKind::ChatCompletions),
        "/v1/messages" => Some(ProtocolKind::Messages),
        "/v1/responses" => Some(ProtocolKind::Responses),
        _ if path.starts_with("/v1beta/models/") => Some(ProtocolKind::Unknown),
        _ => None,
    }
}

fn headers_to_map(headers: &http::HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_ascii_lowercase(), v.to_string()))
        })
        .collect()
}

fn request_id_from_headers(headers: &http::HeaderMap) -> Option<String> {
    headers
        .get(BITROUTER_REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn harness_header_value(harness: &HarnessId) -> &'static str {
    match harness {
        HarnessId::Generic => "generic",
        HarnessId::Hermes => "hermes",
        HarnessId::ClaudeCode => "claude_code",
        HarnessId::Codex => "codex",
        HarnessId::OpenClaw => "openclaw",
        HarnessId::Unknown => "unknown",
    }
}

fn protocol_header_value(protocol: &ProtocolKind) -> &'static str {
    match protocol {
        ProtocolKind::ChatCompletions => "chat_completions",
        ProtocolKind::Messages => "messages",
        ProtocolKind::Responses => "responses",
        ProtocolKind::OpenClawRuntime => "openclaw_runtime",
        ProtocolKind::Unknown => "unknown",
    }
}

fn append_sanitized_trace(
    path: &PathBuf,
    trace: &CapturedIngressTrace,
    sanitizer: &TraceSanitizer,
) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| {
            bitrouter_sdk::BitrouterError::internal(format!(
                "workflow trace archive mkdir {}: {e}",
                parent.display()
            ))
        })?;
    }

    let sanitized = sanitizer.sanitize_trace(trace);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| {
            bitrouter_sdk::BitrouterError::internal(format!(
                "workflow trace archive append open {}: {e}",
                path.display()
            ))
        })?;
    serde_json::to_writer(&mut file, &sanitized).map_err(|e| {
        bitrouter_sdk::BitrouterError::internal(format!("workflow trace archive serialize: {e}"))
    })?;
    file.write_all(b"\n").map_err(|e| {
        bitrouter_sdk::BitrouterError::internal(format!("workflow trace archive append: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::Json;
    use axum::http::{HeaderMap, Request};
    use axum::routing::post;
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn trace_capture_injects_request_id_visible_to_downstream_and_archive() {
        let capture = RealTraceCapture::new(TraceCaptureOptions {
            harness: HarnessId::Codex,
            session_header: None,
            archive_path: None,
        });
        let router = (capture.router_wrapper())(Router::new().route(
            "/v1/responses",
            post(|headers: HeaderMap| async move {
                let request_id = headers
                    .get("x-bitrouter-request-id")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                let harness = headers
                    .get("x-bitrouter-harness")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                let protocol = headers
                    .get("x-bitrouter-protocol")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                Json(json!({
                    "request_id": request_id,
                    "harness": harness,
                    "protocol": protocol,
                }))
            }),
        ));

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "gpt-5.5",
                            "input": "say ok",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let response_body = to_bytes(response.into_body(), MAX_CAPTURE_BODY_BYTES)
            .await
            .unwrap();
        let response_json: serde_json::Value = serde_json::from_slice(&response_body).unwrap();
        let downstream_request_id = response_json["request_id"].as_str().unwrap();
        assert!(
            !downstream_request_id.is_empty(),
            "capture must inject a request id before the SDK server handles the request"
        );

        let records = capture.records();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0]
                .headers
                .get("x-bitrouter-request-id")
                .map(String::as_str),
            Some(downstream_request_id)
        );
        assert_eq!(records[0].id, downstream_request_id);
        assert_eq!(response_json["harness"].as_str(), Some("codex"));
        assert_eq!(response_json["protocol"].as_str(), Some("responses"));
        let captured_at = records[0]
            .captured_at
            .as_deref()
            .expect("trace capture records timestamp");
        chrono::DateTime::parse_from_rfc3339(captured_at).expect("captured_at is RFC3339");
    }
}
