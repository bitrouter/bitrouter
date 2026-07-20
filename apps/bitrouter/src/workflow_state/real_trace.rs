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
use http::header::HeaderName;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::workflow_state::extractors::ExtractorInput;
use crate::workflow_state::fixture::{WorkflowTraceFixture, parse_prompt};
use crate::workflow_state::ir::{ContextTransition, HarnessId, ProtocolKind};
use crate::workflow_state::replay::extract_fixture_ir;
use crate::workflow_state::session::{WorkflowIdentityTracker, resolve_workflow_identity};

const MAX_CAPTURE_BODY_BYTES: usize = 16 * 1024 * 1024;
const BITROUTER_REQUEST_ID_HEADER: &str = "x-bitrouter-request-id";
const BITROUTER_HARNESS_HEADER: &str = "x-bitrouter-harness";
const BITROUTER_PROTOCOL_HEADER: &str = "x-bitrouter-protocol";
const BITROUTER_PARENT_SESSION_HEADER: &str = "x-bitrouter-parent-session-id";
const BITROUTER_AGENT_SESSION_HEADER: &str = "x-bitrouter-agent-session-id";
const BITROUTER_AGENT_ROLE_HEADER: &str = "x-bitrouter-agent-role";
const BITROUTER_CONTEXT_EPOCH_HEADER: &str = "x-bitrouter-context-epoch";
const BITROUTER_CONTEXT_TRANSITION_HEADER: &str = "x-bitrouter-context-transition";
const BITROUTER_SESSION_FINGERPRINT_HEADER: &str = "x-bitrouter-session-fingerprint";

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
    run_id: uuid::Uuid,
    next_id: AtomicU64,
    identity_tracker: WorkflowIdentityTracker,
}

struct PendingIngressTrace {
    capture: RealTraceCapture,
    record: Option<CapturedIngressTrace>,
}

impl PendingIngressTrace {
    fn new(capture: RealTraceCapture, record: CapturedIngressTrace) -> Self {
        Self {
            capture,
            record: Some(record),
        }
    }

    fn finish(mut self, http_status: u16, success: bool) {
        if let Some(mut record) = self.record.take() {
            record.captured_at = Some(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true));
            record.outcome = RealTraceOutcome {
                http_status,
                status: if success {
                    "completed".to_string()
                } else {
                    "failed".to_string()
                },
            };
            self.capture.push_record(record);
        }
    }
}

impl Drop for PendingIngressTrace {
    fn drop(&mut self) {
        if let Some(mut record) = self.record.take() {
            record.captured_at = Some(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true));
            record.outcome = RealTraceOutcome {
                http_status: 499,
                status: "client_cancelled".to_string(),
            };
            self.capture.push_record(record);
        }
    }
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
            "x-bitrouter-agent-role",
            "x-bitrouter-agent-session-id",
            "x-bitrouter-benchmark-run-id",
            "x-bitrouter-cloud-request-id",
            "x-bitrouter-context-epoch",
            "x-bitrouter-context-transition",
            "x-bitrouter-harness",
            "x-bitrouter-inbound-protocol",
            "x-bitrouter-parent-session-id",
            "x-bitrouter-protocol",
            "x-bitrouter-request-id",
            "x-bitrouter-session-fingerprint",
            "x-bitrouter-trial-id",
            "x-bitrouter-workflow-session",
            "x-request-id",
            "x-session-id",
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
                run_id: uuid::Uuid::new_v4(),
                next_id: AtomicU64::new(1),
                identity_tracker: WorkflowIdentityTracker::default(),
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
        if let Some(raw_body) = raw_body.as_ref()
            && let Some(session_header) = self.inner.options.session_header.as_deref()
            && !parts.headers.contains_key(session_header)
            && let Some(session) = session_from_raw_body(&self.inner.options.harness, raw_body)
            && let Ok(name) = HeaderName::from_bytes(session_header.as_bytes())
            && let Ok(value) = HeaderValue::from_str(&session)
        {
            parts.headers.insert(name, value);
        }
        if let (Some(protocol), Some(raw_body)) = (protocol.as_ref(), raw_body.as_ref()) {
            self.inject_workflow_identity(&mut parts.headers, protocol, raw_body);
        }
        let headers = headers_to_map(&parts.headers);
        let req = Request::from_parts(parts, Body::from(body_bytes));
        let pending_trace = match (protocol, raw_body) {
            (Some(protocol), Some(raw_body)) => Some(PendingIngressTrace::new(
                self.clone(),
                CapturedIngressTrace {
                    id: request_id,
                    captured_at: None,
                    harness: self.inner.options.harness.clone(),
                    protocol,
                    method,
                    path,
                    headers,
                    raw_body,
                    outcome: RealTraceOutcome {
                        http_status: 499,
                        status: "client_cancelled".to_string(),
                    },
                },
            )),
            _ => None,
        };
        let response = next.run(req).await;
        if let Some(pending_trace) = pending_trace {
            pending_trace.finish(response.status().as_u16(), response.status().is_success());
        }

        response
    }

    fn next_id(&self) -> String {
        let n = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        format!("real-agent-trace-{}-{n:04}", self.inner.run_id.simple())
    }

    fn inject_workflow_identity(
        &self,
        headers: &mut http::HeaderMap,
        protocol: &ProtocolKind,
        raw_body: &serde_json::Value,
    ) {
        if self.inner.options.harness != HarnessId::Terminus2 {
            return;
        }
        let Ok(prompt) = parse_prompt(protocol, raw_body.clone(), None) else {
            return;
        };
        let input = ExtractorInput {
            harness_hint: Some(HarnessId::Terminus2),
            protocol_hint: protocol.clone(),
            headers,
            raw_body,
            prompt: &prompt,
        };
        let identity = resolve_workflow_identity(&input, &self.inner.identity_tracker);

        if let Some(parent) = identity.parent_session_id.as_deref() {
            insert_header_if_absent(headers, BITROUTER_PARENT_SESSION_HEADER, parent);
        }
        if let Some(agent) = identity.agent_session_id.as_deref() {
            insert_header_if_absent(headers, BITROUTER_AGENT_SESSION_HEADER, agent);
        }
        insert_header_if_absent(headers, BITROUTER_AGENT_ROLE_HEADER, identity.role.as_str());
        insert_header_if_absent(
            headers,
            BITROUTER_CONTEXT_EPOCH_HEADER,
            &identity.context_epoch.to_string(),
        );
        insert_header_if_absent(
            headers,
            BITROUTER_CONTEXT_TRANSITION_HEADER,
            transition_header_value(identity.transition),
        );
        if !identity.fingerprint.is_empty() {
            insert_header_if_absent(
                headers,
                BITROUTER_SESSION_FINGERPRINT_HEADER,
                &identity.fingerprint,
            );
        }
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
            // Archive-backed daemon capture is a streaming mode. Keeping the
            // same full request body in `records` as well makes resident memory
            // grow with the lifetime trace volume and eventually OOMs long
            // benchmark runs. In-memory capture remains available when no
            // archive path is configured (the default used by replay tests).
            return;
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
        "terminus_2" | "terminus-2" | "terminus2" => HarnessId::Terminus2,
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

fn insert_header_if_absent(headers: &mut http::HeaderMap, name: &'static str, value: &str) {
    if headers.contains_key(name) {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

fn harness_header_value(harness: &HarnessId) -> &'static str {
    match harness {
        HarnessId::Generic => "generic",
        HarnessId::Hermes => "hermes",
        HarnessId::ClaudeCode => "claude_code",
        HarnessId::Codex => "codex",
        HarnessId::Terminus2 => "terminus_2",
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

fn transition_header_value(transition: ContextTransition) -> &'static str {
    match transition {
        ContextTransition::None => "none",
        ContextTransition::CompactionStart => "compaction_start",
        ContextTransition::CompactionContinuation => "compaction_continuation",
        ContextTransition::MainResume => "main_resume",
    }
}

fn session_from_raw_body(harness: &HarnessId, raw_body: &serde_json::Value) -> Option<String> {
    match harness {
        HarnessId::Codex => json_str(raw_body, &["previous_response_id"]),
        HarnessId::ClaudeCode => claude_session_from_metadata(raw_body),
        HarnessId::Hermes => json_str(raw_body, &["metadata", "job_id"]),
        HarnessId::Terminus2 => json_str(raw_body, &["session_id"]),
        HarnessId::Generic | HarnessId::OpenClaw | HarnessId::Unknown => None,
    }
}

fn claude_session_from_metadata(raw_body: &serde_json::Value) -> Option<String> {
    let user_id = json_str(raw_body, &["metadata", "user_id"])?;
    serde_json::from_str::<serde_json::Value>(&user_id)
        .ok()
        .and_then(|value| json_str(&value, &["session_id"]))
        .or(Some(user_id))
}

fn json_str(value: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
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

    #[test]
    fn generated_request_ids_are_unique_across_capture_instances() {
        let first = RealTraceCapture::new(TraceCaptureOptions::default());
        let second = RealTraceCapture::new(TraceCaptureOptions::default());

        assert_ne!(first.next_id(), second.next_id());
    }

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

    #[tokio::test]
    async fn trace_capture_injects_session_header_from_codex_previous_response_id() {
        let capture = RealTraceCapture::new(TraceCaptureOptions {
            harness: HarnessId::Codex,
            session_header: Some("x-bitrouter-workflow-session".to_string()),
            archive_path: None,
        });
        let router = (capture.router_wrapper())(Router::new().route(
            "/v1/responses",
            post(|headers: HeaderMap| async move {
                let session = headers
                    .get("x-bitrouter-workflow-session")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                Json(json!({ "session": session }))
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
                            "previous_response_id": "resp_123",
                            "input": "continue",
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

        assert_eq!(response_json["session"].as_str(), Some("resp_123"));
        let records = capture.records();
        assert_eq!(
            records[0]
                .headers
                .get("x-bitrouter-workflow-session")
                .map(String::as_str),
            Some("resp_123")
        );
    }

    #[tokio::test]
    async fn terminus_trace_capture_parses_official_subagent_session_identity() {
        let capture = RealTraceCapture::new(TraceCaptureOptions {
            harness: HarnessId::Terminus2,
            session_header: Some("x-bitrouter-workflow-session".to_string()),
            archive_path: None,
        });
        let router = (capture.router_wrapper())(Router::new().route(
            "/v1/chat/completions",
            post(|headers: HeaderMap| async move {
                let values = [
                    "x-bitrouter-workflow-session",
                    "x-bitrouter-parent-session-id",
                    "x-bitrouter-agent-session-id",
                    "x-bitrouter-agent-role",
                    "x-bitrouter-context-epoch",
                    "x-bitrouter-context-transition",
                    "x-bitrouter-session-fingerprint",
                ]
                .into_iter()
                .map(|name| {
                    (
                        name,
                        headers
                            .get(name)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("")
                            .to_string(),
                    )
                })
                .collect::<BTreeMap<_, _>>();
                Json(values)
            }),
        ));

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("x-bitrouter-benchmark-run-id", "short13-run")
                    .header("x-bitrouter-trial-id", "trial-01")
                    .body(Body::from(
                        json!({
                            "model": "inbound",
                            "session_id": "terminus-parent-summarization-1-answers",
                            "messages": [{
                                "role": "user",
                                "content": "This deliberately misleading prompt must not override the session suffix."
                            }]
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

        assert_eq!(
            response_json["x-bitrouter-workflow-session"].as_str(),
            Some("terminus-parent-summarization-1-answers")
        );
        assert_eq!(
            response_json["x-bitrouter-parent-session-id"].as_str(),
            Some("terminus-parent")
        );
        assert_eq!(
            response_json["x-bitrouter-agent-session-id"].as_str(),
            Some("terminus-parent-summarization-1-answers")
        );
        assert_eq!(
            response_json["x-bitrouter-agent-role"].as_str(),
            Some("answers")
        );
        assert_eq!(
            response_json["x-bitrouter-context-epoch"].as_str(),
            Some("1")
        );
        assert_eq!(
            response_json["x-bitrouter-context-transition"].as_str(),
            Some("compaction_continuation")
        );
        assert!(
            response_json["x-bitrouter-session-fingerprint"]
                .as_str()
                .is_some_and(|value| value.starts_with("sha256:"))
        );

        let records = capture.records();
        let sanitized = TraceSanitizer::default().sanitize_trace(&records[0]);
        assert_eq!(
            sanitized
                .headers
                .get("x-bitrouter-session-fingerprint")
                .map(String::as_str),
            response_json["x-bitrouter-session-fingerprint"].as_str()
        );
        assert_eq!(
            sanitized
                .headers
                .get("x-bitrouter-benchmark-run-id")
                .map(String::as_str),
            Some("short13-run")
        );
        assert_eq!(
            sanitized
                .headers
                .get("x-bitrouter-trial-id")
                .map(String::as_str),
            Some("trial-01")
        );
    }
}
