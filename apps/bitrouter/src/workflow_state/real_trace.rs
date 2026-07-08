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
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::workflow_state::fixture::WorkflowTraceFixture;
use crate::workflow_state::ir::{HarnessId, ProtocolKind};
use crate::workflow_state::replay::extract_fixture_ir;

const MAX_CAPTURE_BODY_BYTES: usize = 16 * 1024 * 1024;

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
        let (parts, body) = req.into_parts();
        let method = parts.method.to_string();
        let path = parts.uri.path().to_string();
        let headers = headers_to_map(&parts.headers);
        let protocol = protocol_for_path(&path);

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
                id: self.next_id(),
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
