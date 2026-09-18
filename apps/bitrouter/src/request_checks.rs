//! Fail-closed HTTP runtime for named-router request checks.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bitrouter_checker_protocol::v1 as checker_protocol;
use bitrouter_sdk::config::Config;
use bitrouter_sdk::config::router::{DEFAULT_CHECKER_MAX_INPUT_BYTES, MAX_CHECKER_TIMEOUT_MS};
use bitrouter_sdk::language_model::receipts::{
    LatestRequestCheck, RequestCheckDispatchStatus, RequestCheckReporter, RequestCheckStatus,
    RequestReceiptStore, RequestReceiptStoreConfig,
};
use bitrouter_sdk::language_model::request_checks::{
    CheckerDecision, CheckerFailure, CheckerFailureKind, CheckerInvocation, ContentFragment,
    ContentFragmentKind, ContentRole, RequestCheckBinding, RequestCheckCoverage,
    RequestCheckCoverageScope, RequestCheckCoverageStatus, RequestCheckerRunner,
};
use futures::StreamExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use url::Url;

/// Maximum concurrent HTTP invocations admitted for one checker.
pub const MAX_CONCURRENT_INVOCATIONS_PER_CHECKER: usize = 32;
/// Maximum checker response body accepted by the daemon.
pub const MAX_CHECKER_RESPONSE_BYTES: usize = checker_protocol::MAX_RESPONSE_BYTES;
/// Absolute bound for the encoded checker request envelope.
pub const MAX_CHECKER_REQUEST_BYTES: u64 = checker_protocol::MAX_REQUEST_BYTES as u64;

const PROBE_TIMEOUT_MS: u64 = 500;

struct ActiveChecker {
    endpoint: Url,
    credential_env: Option<String>,
    credential: Option<String>,
    contract_version: u16,
    semaphore: Arc<Semaphore>,
}

struct ActiveBinding {
    checker_id: String,
    timeout_ms: u64,
    max_input_bytes: u64,
}

/// Non-secret metadata for one running router binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckerBindingInfo {
    /// Named router that owns this binding.
    pub router_id: String,
    /// Redaction-safe identity of the effective checker binding.
    pub binding_digest: String,
    /// Total invocation deadline from the running config.
    pub timeout_ms: u64,
    /// Projected text byte limit from the running config.
    pub max_input_bytes: u64,
    /// Latest real request observation for this exact binding.
    pub last_actual: Option<CheckerActualUsage>,
}

/// Latest real request observed for one running checker binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckerActualUsage {
    /// Caller-visible gateway request id.
    pub request_id: String,
    /// Per-check invocation id sent to the remote service.
    pub invocation_id: String,
    /// Binding identity used for this invocation.
    pub binding_digest: String,
    /// Current or terminal checker outcome.
    pub status: RequestCheckStatus,
    /// Furthest remote-dispatch boundary reached.
    pub dispatch: RequestCheckDispatchStatus,
    /// Validated remote implementation version, when returned.
    pub implementation_version: Option<String>,
    /// Observation time in Unix milliseconds.
    pub observed_at_unix_ms: u64,
}

/// Non-secret metadata for one checker in the running configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckerInfo {
    /// Top-level checker id.
    pub checker_id: String,
    /// Opaque digest of the configured endpoint; the URL is not exposed.
    pub endpoint_fingerprint: String,
    /// Dedicated credential environment variable name, never its value.
    pub credential_env: Option<String>,
    /// Whether the configured credential is available to this daemon.
    pub credential_ready: bool,
    /// Configured checker wire-contract version.
    pub contract_version: u16,
    /// Per-checker concurrency ceiling.
    pub max_concurrent_invocations: usize,
    /// Running router bindings that reference this checker.
    pub bindings: Vec<CheckerBindingInfo>,
    /// Latest synthetic probe, kept separate from real request observations.
    pub last_probe: Option<ProbeResult>,
}

/// Whether a synthetic checker probe reached the configured service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProbeReachability {
    /// No network request was needed or possible.
    NotAttempted,
    /// A request began, but no evidence proves whether the service received it.
    Unknown,
    /// Response headers were received.
    Reachable,
    /// The HTTP transport reported that the endpoint was unavailable.
    Unreachable,
}

/// Result of validating the checker wire contract during a synthetic probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProbeProtocolStatus {
    /// No checker response was available to validate.
    NotChecked,
    /// A response began but did not complete within the probe boundary.
    Incomplete,
    /// A complete, correlated v1 response was decoded.
    Valid,
    /// A complete response violated the v1 contract.
    Invalid,
}

/// Sanitized, out-of-band checker diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProbeResult {
    /// Probed checker id.
    pub checker_id: String,
    /// Evidence about network reachability.
    pub reachability: ProbeReachability,
    /// Evidence about the response wire contract.
    pub protocol: ProbeProtocolStatus,
    /// End-to-end probe duration.
    pub latency_ms: Option<u64>,
    /// Validated remote implementation version, when returned.
    pub implementation_version: Option<String>,
    /// Synthetic decision, when the checker returned a valid response.
    pub decision: Option<CheckerProbeDecision>,
    /// Stable sanitized diagnostic code.
    pub error_code: Option<String>,
    /// Probe observation time in Unix milliseconds.
    pub observed_at_unix_ms: u64,
}

/// Decision returned by a synthetically probed checker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckerProbeDecision {
    /// Synthetic content was allowed.
    Allow,
    /// Synthetic content was denied.
    Deny,
}

/// Activated HTTP checkers plus the receipt store shared with the pipeline.
pub struct RequestCheckRuntime {
    http: reqwest::Client,
    checkers: HashMap<String, ActiveChecker>,
    active_bindings: HashMap<String, ActiveBinding>,
    inventory: Vec<CheckerInfo>,
    receipts: RequestReceiptStore,
    probes: Mutex<HashMap<String, ProbeResult>>,
}

impl RequestCheckRuntime {
    /// Activate a running config. Bound checkers must have their dedicated
    /// credential available; an unused checker may remain unready for probing.
    pub fn activate(config: &Config) -> anyhow::Result<Self> {
        config.validate_router_config()?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| anyhow::anyhow!("build request checker HTTP client"))?;

        let bound_checker_ids = config
            .routers
            .values()
            .flat_map(|router| &router.checks.request)
            .map(|binding| binding.checker.as_str())
            .collect::<HashSet<_>>();
        let mut checkers = HashMap::with_capacity(config.checkers.len());
        for (checker_id, checker) in &config.checkers {
            let endpoint = Url::parse(&checker.endpoint)
                .map_err(|_| anyhow::anyhow!("checker '{checker_id}' endpoint is invalid"))?;
            let credential = checker
                .credential_env
                .as_deref()
                .and_then(bitrouter_sdk::config::env_lookup)
                .filter(|value| !value.is_empty());
            if bound_checker_ids.contains(checker_id.as_str())
                && credential.is_none()
                && let Some(env_name) = checker.credential_env.as_deref()
            {
                anyhow::bail!(
                    "checker '{checker_id}' requires credential environment variable {env_name}"
                );
            }
            checkers.insert(
                checker_id.clone(),
                ActiveChecker {
                    endpoint,
                    credential_env: checker.credential_env.clone(),
                    credential,
                    contract_version: checker.contract_version,
                    semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_INVOCATIONS_PER_CHECKER)),
                },
            );
        }

        let (inventory, active_bindings) = build_inventory(config)?;
        Ok(Self {
            http,
            checkers,
            active_bindings,
            inventory,
            receipts: RequestReceiptStore::new(RequestReceiptStoreConfig::default()),
            probes: Mutex::new(HashMap::new()),
        })
    }

    /// Process-local receipts for the same daemon incarnation as this runtime.
    pub fn receipts(&self) -> RequestReceiptStore {
        self.receipts.clone()
    }

    /// Sorted, redaction-safe running checker inventory.
    pub fn configured(&self) -> Vec<CheckerInfo> {
        let latest_actual = self.receipts.latest_started_checks();
        let probes = lock_probes(&self.probes);
        self.inventory
            .iter()
            .cloned()
            .map(|mut checker| {
                checker.last_probe = probes.get(&checker.checker_id).cloned();
                for binding in &mut checker.bindings {
                    binding.last_actual = latest_actual
                        .get(&binding.binding_digest)
                        .cloned()
                        .and_then(actual_usage);
                }
                checker
            })
            .collect()
    }

    /// Probe a checker with fixed synthetic content without recording a request receipt.
    pub async fn probe(&self, checker_id: &str) -> ProbeResult {
        let Some(checker) = self.checkers.get(checker_id) else {
            return ProbeResult {
                checker_id: checker_id.to_owned(),
                reachability: ProbeReachability::NotAttempted,
                protocol: ProbeProtocolStatus::NotChecked,
                latency_ms: None,
                implementation_version: None,
                decision: None,
                error_code: Some("not_configured".to_owned()),
                observed_at_unix_ms: unix_millis(),
            };
        };
        if checker.credential_env.is_some() && checker.credential.is_none() {
            let result = ProbeResult {
                checker_id: checker_id.to_owned(),
                reachability: ProbeReachability::NotAttempted,
                protocol: ProbeProtocolStatus::NotChecked,
                latency_ms: None,
                implementation_version: None,
                decision: None,
                error_code: Some("credential_missing".to_owned()),
                observed_at_unix_ms: unix_millis(),
            };
            self.record_probe(result.clone());
            return result;
        }

        let invocation = CheckerInvocation {
            invocation_id: uuid::Uuid::new_v4().to_string(),
            request_id: "bitrouter-checker-probe".to_owned(),
            router_id: "bitrouter-checker-probe".to_owned(),
            router_binding_digest: "probe-v1".to_owned(),
            checker: RequestCheckBinding {
                checker_id: checker_id.to_owned(),
                binding_digest: "probe-v1".to_owned(),
                max_input_bytes: DEFAULT_CHECKER_MAX_INPUT_BYTES,
                timeout_ms: PROBE_TIMEOUT_MS,
            },
            content: vec![ContentFragment {
                role: ContentRole::User,
                kind: ContentFragmentKind::Text,
                text: Some("bitrouter request checker probe".to_owned()),
            }],
            coverage: RequestCheckCoverage {
                scope: RequestCheckCoverageScope::EntryRequestText,
                text_bytes: 31,
                text_fragments: 1,
                excluded_media_fragments: 0,
                status: RequestCheckCoverageStatus::CompleteWithinScope,
            },
        };
        let started = Instant::now();
        let progress = ProbeProgress::default();
        let result = match self
            .invoke(invocation, false, InvocationProgress::Probe(&progress))
            .await
        {
            Ok(outcome) => ProbeResult {
                checker_id: checker_id.to_owned(),
                reachability: ProbeReachability::Reachable,
                protocol: ProbeProtocolStatus::Valid,
                latency_ms: Some(elapsed_millis(started)),
                implementation_version: outcome.implementation_version,
                decision: Some(if outcome.allowed {
                    CheckerProbeDecision::Allow
                } else {
                    CheckerProbeDecision::Deny
                }),
                error_code: None,
                observed_at_unix_ms: unix_millis(),
            },
            Err(error) => ProbeResult {
                checker_id: checker_id.to_owned(),
                reachability: if error.response_received {
                    ProbeReachability::Reachable
                } else if !error.request_dispatched {
                    ProbeReachability::NotAttempted
                } else if error.code == "unavailable" {
                    ProbeReachability::Unreachable
                } else {
                    ProbeReachability::Unknown
                },
                protocol: if error.response_received
                    && matches!(error.code, "timeout" | "body_read_failed")
                {
                    ProbeProtocolStatus::Incomplete
                } else if error.response_received {
                    ProbeProtocolStatus::Invalid
                } else {
                    ProbeProtocolStatus::NotChecked
                },
                latency_ms: Some(elapsed_millis(started)),
                implementation_version: None,
                decision: None,
                error_code: Some(error.code.to_owned()),
                observed_at_unix_ms: unix_millis(),
            },
        };
        self.record_probe(result.clone());
        result
    }

    async fn invoke(
        &self,
        invocation: CheckerInvocation,
        require_active_binding: bool,
        progress: InvocationProgress<'_>,
    ) -> Result<WireOutcome, InvokeError> {
        let checker = self
            .checkers
            .get(&invocation.checker.checker_id)
            .ok_or_else(|| {
                InvokeError::failure(CheckerFailureKind::NotConfigured, "not_configured", false)
            })?;
        if require_active_binding {
            let binding = self
                .active_bindings
                .get(&invocation.checker.binding_digest)
                .ok_or_else(|| {
                    InvokeError::failure(
                        CheckerFailureKind::NotConfigured,
                        "binding_not_active",
                        false,
                    )
                })?;
            if binding.checker_id != invocation.checker.checker_id
                || binding.timeout_ms != invocation.checker.timeout_ms
                || binding.max_input_bytes != invocation.checker.max_input_bytes
            {
                return Err(InvokeError::failure(
                    CheckerFailureKind::NotConfigured,
                    "binding_mismatch",
                    false,
                ));
            }
        }
        if checker.credential_env.is_some() && checker.credential.is_none() {
            return Err(InvokeError::failure(
                CheckerFailureKind::NotConfigured,
                "credential_missing",
                false,
            ));
        }
        if invocation.checker.timeout_ms == 0
            || invocation.checker.timeout_ms > MAX_CHECKER_TIMEOUT_MS
        {
            return Err(InvokeError::failure(
                CheckerFailureKind::Internal,
                "invalid_deadline",
                false,
            ));
        }
        if invocation.coverage.text_bytes > invocation.checker.max_input_bytes
            || invocation.coverage.status == RequestCheckCoverageStatus::InputTooLarge
        {
            return Err(InvokeError::failure(
                CheckerFailureKind::InputTooLarge,
                "input_too_large",
                false,
            ));
        }

        let deadline = Duration::from_millis(invocation.checker.timeout_ms);
        let expected_invocation_id = invocation.invocation_id.clone();
        let request = protocol_request(checker.contract_version, invocation);
        let body = checker_protocol::encode_request(&request).map_err(encode_error)?;
        let operation = async move {
            let _permit = checker.semaphore.acquire().await.map_err(|_| {
                InvokeError::failure(CheckerFailureKind::Internal, "runtime_closed", false)
            })?;
            let mut request = self
                .http
                .post(checker.endpoint.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "application/json")
                .body(body);
            if let Some(credential) = checker.credential.as_deref() {
                request = request.bearer_auth(credential);
            }
            let request = request.build().map_err(|_| {
                InvokeError::failure(
                    CheckerFailureKind::NotConfigured,
                    "invalid_request_configuration",
                    false,
                )
            })?;
            progress.mark_dispatched();
            let response = self.http.execute(request).await.map_err(|_| {
                InvokeError::dispatched(CheckerFailureKind::Unavailable, "unavailable", false)
            })?;
            progress.mark_response_received();
            if !response.status().is_success() {
                return Err(InvokeError::failure(
                    CheckerFailureKind::InvalidResponse,
                    "http_status",
                    true,
                ));
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_CHECKER_RESPONSE_BYTES as u64)
            {
                return Err(InvokeError::failure(
                    CheckerFailureKind::InvalidResponse,
                    "response_too_large",
                    true,
                ));
            }
            let mut stream = response.bytes_stream();
            let mut response_body = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| {
                    InvokeError::failure(
                        CheckerFailureKind::InvalidResponse,
                        "body_read_failed",
                        true,
                    )
                })?;
                if response_body.len().saturating_add(chunk.len()) > MAX_CHECKER_RESPONSE_BYTES {
                    return Err(InvokeError::failure(
                        CheckerFailureKind::InvalidResponse,
                        "response_too_large",
                        true,
                    ));
                }
                response_body.extend_from_slice(&chunk);
            }
            decode_response(&expected_invocation_id, &response_body)
        };

        tokio::time::timeout(deadline, operation)
            .await
            .map_err(|_| {
                InvokeError::with_progress(
                    CheckerFailureKind::Timeout,
                    "timeout",
                    progress.request_dispatched(),
                    progress.response_received(),
                )
            })?
    }

    fn record_probe(&self, result: ProbeResult) {
        let mut probes = lock_probes(&self.probes);
        if self.checkers.contains_key(&result.checker_id) {
            probes.insert(result.checker_id.clone(), result);
        }
    }
}

fn actual_usage(latest: LatestRequestCheck) -> Option<CheckerActualUsage> {
    let status = match latest.check.status {
        RequestCheckStatus::Pending
        | RequestCheckStatus::Interrupted
        | RequestCheckStatus::Allowed
        | RequestCheckStatus::Denied
        | RequestCheckStatus::Failed => latest.check.status,
        RequestCheckStatus::NotRun
        | RequestCheckStatus::NotEnabled
        | RequestCheckStatus::Skipped => return None,
    };
    Some(CheckerActualUsage {
        request_id: latest.request_id,
        invocation_id: latest.check.invocation_id?,
        binding_digest: latest.check.binding_digest?,
        status,
        dispatch: latest
            .check
            .dispatch
            .unwrap_or(RequestCheckDispatchStatus::NotAttempted),
        implementation_version: latest.check.implementation_version,
        observed_at_unix_ms: latest
            .check
            .observed_at_unix_ms
            .or(latest.check.finished_at_unix_ms)
            .or(latest.check.started_at_unix_ms)?,
    })
}

#[derive(Default)]
struct ProbeProgress {
    request_dispatched: AtomicBool,
    response_received: AtomicBool,
}

#[derive(Clone, Copy)]
enum InvocationProgress<'a> {
    Actual(&'a RequestCheckReporter),
    Probe(&'a ProbeProgress),
}

impl InvocationProgress<'_> {
    fn mark_dispatched(self) {
        match self {
            Self::Actual(reporter) => reporter.mark_dispatched(),
            Self::Probe(progress) => progress.request_dispatched.store(true, Ordering::Relaxed),
        }
    }

    fn mark_response_received(self) {
        match self {
            Self::Actual(reporter) => reporter.mark_response_received(),
            Self::Probe(progress) => progress.response_received.store(true, Ordering::Relaxed),
        }
    }

    fn request_dispatched(self) -> bool {
        match self {
            Self::Actual(reporter) => reporter.dispatch_status().is_some_and(|status| {
                matches!(
                    status,
                    RequestCheckDispatchStatus::Attempted
                        | RequestCheckDispatchStatus::ResponseReceived
                )
            }),
            Self::Probe(progress) => progress.request_dispatched.load(Ordering::Relaxed),
        }
    }

    fn response_received(self) -> bool {
        match self {
            Self::Actual(reporter) => {
                reporter.dispatch_status() == Some(RequestCheckDispatchStatus::ResponseReceived)
            }
            Self::Probe(progress) => progress.response_received.load(Ordering::Relaxed),
        }
    }
}

#[async_trait]
impl RequestCheckerRunner for RequestCheckRuntime {
    async fn check(
        &self,
        invocation: CheckerInvocation,
        reporter: RequestCheckReporter,
    ) -> Result<CheckerDecision, CheckerFailure> {
        self.invoke(invocation, true, InvocationProgress::Actual(&reporter))
            .await
            .map(WireOutcome::into_decision)
            .map_err(|error| error.failure)
    }
}

fn protocol_request(
    contract_version: u16,
    invocation: CheckerInvocation,
) -> checker_protocol::Request {
    checker_protocol::Request {
        contract_version,
        invocation_id: invocation.invocation_id,
        request_id: invocation.request_id,
        router_id: invocation.router_id,
        router_binding_digest: invocation.router_binding_digest,
        checker: checker_protocol::CheckerBinding {
            checker_id: invocation.checker.checker_id,
            binding_digest: invocation.checker.binding_digest,
            max_input_bytes: invocation.checker.max_input_bytes,
            timeout_ms: invocation.checker.timeout_ms,
        },
        content: invocation
            .content
            .into_iter()
            .map(|fragment| checker_protocol::ContentFragment {
                role: match fragment.role {
                    ContentRole::System => checker_protocol::ContentRole::System,
                    ContentRole::User => checker_protocol::ContentRole::User,
                    ContentRole::Assistant => checker_protocol::ContentRole::Assistant,
                    ContentRole::Tool => checker_protocol::ContentRole::Tool,
                },
                kind: match fragment.kind {
                    ContentFragmentKind::Text => checker_protocol::ContentFragmentKind::Text,
                    ContentFragmentKind::Reasoning => {
                        checker_protocol::ContentFragmentKind::Reasoning
                    }
                    ContentFragmentKind::ToolCall => {
                        checker_protocol::ContentFragmentKind::ToolCall
                    }
                    ContentFragmentKind::ToolResult => {
                        checker_protocol::ContentFragmentKind::ToolResult
                    }
                    ContentFragmentKind::ToolApproval => {
                        checker_protocol::ContentFragmentKind::ToolApproval
                    }
                },
                text: fragment.text,
            })
            .collect(),
        coverage: checker_protocol::Coverage {
            scope: match invocation.coverage.scope {
                RequestCheckCoverageScope::EntryRequestText => {
                    checker_protocol::CoverageScope::EntryRequestText
                }
            },
            text_bytes: invocation.coverage.text_bytes,
            text_fragments: invocation.coverage.text_fragments,
            excluded_media_fragments: invocation.coverage.excluded_media_fragments,
            status: match invocation.coverage.status {
                RequestCheckCoverageStatus::CompleteWithinScope => {
                    checker_protocol::CoverageStatus::CompleteWithinScope
                }
                RequestCheckCoverageStatus::InputTooLarge => {
                    checker_protocol::CoverageStatus::InputTooLarge
                }
            },
        },
    }
}

fn encode_error(error: checker_protocol::ProtocolError) -> InvokeError {
    if error == checker_protocol::ProtocolError::RequestTooLarge {
        InvokeError::failure(CheckerFailureKind::InputTooLarge, error.code(), false)
    } else {
        InvokeError::failure(CheckerFailureKind::Internal, error.code(), false)
    }
}

struct WireOutcome {
    allowed: bool,
    reason_code: Option<String>,
    implementation_version: Option<String>,
}

impl WireOutcome {
    fn into_decision(self) -> CheckerDecision {
        if self.allowed {
            CheckerDecision::Allow {
                implementation_version: self.implementation_version,
            }
        } else {
            CheckerDecision::Deny {
                reason_code: self.reason_code,
                implementation_version: self.implementation_version,
            }
        }
    }
}

struct InvokeError {
    failure: CheckerFailure,
    code: &'static str,
    request_dispatched: bool,
    response_received: bool,
}

impl InvokeError {
    fn failure(kind: CheckerFailureKind, code: &'static str, response_received: bool) -> Self {
        Self::with_progress(kind, code, response_received, response_received)
    }

    fn dispatched(kind: CheckerFailureKind, code: &'static str, response_received: bool) -> Self {
        Self::with_progress(kind, code, true, response_received)
    }

    fn with_progress(
        kind: CheckerFailureKind,
        code: &'static str,
        request_dispatched: bool,
        response_received: bool,
    ) -> Self {
        Self {
            failure: CheckerFailure {
                kind,
                detail: Some(code.to_owned()),
            },
            code,
            request_dispatched,
            response_received,
        }
    }
}

fn decode_response(invocation_id: &str, body: &[u8]) -> Result<WireOutcome, InvokeError> {
    let response = checker_protocol::decode_response(invocation_id, body).map_err(|error| {
        InvokeError::failure(CheckerFailureKind::InvalidResponse, error.code(), true)
    })?;
    Ok(WireOutcome {
        allowed: response.decision() == checker_protocol::Decision::Allow,
        reason_code: response.reason_code().map(str::to_owned),
        implementation_version: response.implementation_version().map(str::to_owned),
    })
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn build_inventory(
    config: &Config,
) -> anyhow::Result<(Vec<CheckerInfo>, HashMap<String, ActiveBinding>)> {
    let mut bindings_by_checker = HashMap::<String, Vec<CheckerBindingInfo>>::new();
    let mut active_bindings = HashMap::new();
    for (router_id, router) in &config.routers {
        for binding in &router.checks.request {
            let checker = config.checkers.get(&binding.checker).ok_or_else(|| {
                anyhow::anyhow!(
                    "router '{router_id}' references missing checker '{}'",
                    binding.checker
                )
            })?;
            let resolved = binding.resolve(router_id, checker)?;
            active_bindings.insert(
                resolved.binding_digest.clone(),
                ActiveBinding {
                    checker_id: resolved.checker_id,
                    timeout_ms: resolved.timeout_ms,
                    max_input_bytes: resolved.max_input_bytes,
                },
            );
            bindings_by_checker
                .entry(binding.checker.clone())
                .or_default()
                .push(CheckerBindingInfo {
                    router_id: router_id.clone(),
                    binding_digest: resolved.binding_digest,
                    timeout_ms: binding.timeout_ms,
                    max_input_bytes: binding.max_input_bytes,
                    last_actual: None,
                });
        }
    }
    let mut inventory = config
        .checkers
        .iter()
        .map(|(checker_id, checker)| {
            let mut bindings = bindings_by_checker.remove(checker_id).unwrap_or_default();
            bindings.sort_by(|left, right| left.router_id.cmp(&right.router_id));
            CheckerInfo {
                checker_id: checker_id.clone(),
                endpoint_fingerprint: format!(
                    "sha256:{}",
                    hex::encode(Sha256::digest(checker.endpoint.as_bytes()))
                ),
                credential_env: checker.credential_env.clone(),
                credential_ready: checker.credential_env.as_deref().is_none_or(|name| {
                    bitrouter_sdk::config::env_lookup(name).is_some_and(|value| !value.is_empty())
                }),
                contract_version: checker.contract_version,
                max_concurrent_invocations: MAX_CONCURRENT_INVOCATIONS_PER_CHECKER,
                bindings,
                last_probe: None,
            }
        })
        .collect::<Vec<_>>();
    inventory.sort_by(|left, right| left.checker_id.cmp(&right.checker_id));
    Ok((inventory, active_bindings))
}

fn lock_probes(
    probes: &Mutex<HashMap<String, ProbeResult>>,
) -> std::sync::MutexGuard<'_, HashMap<String, ProbeResult>> {
    match probes.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn unix_millis() -> u64 {
    let millis = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis(),
        Err(_) => 0,
    };
    u64::try_from(millis).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::config::RoutingConfig;
    use bitrouter_sdk::config::checker::CheckerConfig;
    use bitrouter_sdk::config::router::{
        RouterChecks, RouterConfig, RouterDefaults, RouterRequestCheck, RouterSelection,
    };
    use bitrouter_sdk::language_model::receipts::{
        RequestDeliveryStatus, RequestFailureStage, RequestReceiptHandle, RequestReceiptOutcome,
    };
    use bitrouter_sdk::language_model::routing::RouterRequestIdentity;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    #[derive(Clone)]
    struct EchoDecision {
        decision: &'static str,
    }

    impl Respond for EchoDecision {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let invocation_id = match serde_json::from_slice::<Value>(&request.body) {
                Ok(body) => match body.get("invocation_id").and_then(Value::as_str) {
                    Some(value) => value.to_owned(),
                    None => return ResponseTemplate::new(400),
                },
                Err(_) => return ResponseTemplate::new(400),
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "contract_version": 1,
                "invocation_id": invocation_id,
                "decision": self.decision,
                "implementation_version": "fixture-v1"
            }))
        }
    }

    fn config(
        endpoint: String,
        credential_env: Option<String>,
        bind: bool,
        timeout_ms: u64,
    ) -> Config {
        let mut config = Config::default();
        config.checkers.insert(
            "safety".to_owned(),
            CheckerConfig {
                endpoint,
                credential_env,
                contract_version: checker_protocol::CONTRACT_VERSION,
            },
        );
        if bind {
            config.routers.insert(
                "guarded".to_owned(),
                RouterConfig {
                    selection: RouterSelection::Model {
                        model: "vendor:model".to_owned(),
                        routing: RoutingConfig::default(),
                    },
                    defaults: RouterDefaults::default(),
                    checks: RouterChecks {
                        request: vec![RouterRequestCheck {
                            checker: "safety".to_owned(),
                            timeout_ms,
                            max_input_bytes: DEFAULT_CHECKER_MAX_INPUT_BYTES,
                        }],
                    },
                },
            );
        }
        config
    }

    fn binding(config: &Config) -> anyhow::Result<RequestCheckBinding> {
        let router = config
            .routers
            .get("guarded")
            .ok_or_else(|| anyhow::anyhow!("guarded router is missing"))?;
        let configured = router
            .checks
            .request
            .first()
            .ok_or_else(|| anyhow::anyhow!("request checker binding is missing"))?;
        let checker = config
            .checkers
            .get(&configured.checker)
            .ok_or_else(|| anyhow::anyhow!("request checker is missing"))?;
        configured.resolve("guarded", checker).map_err(Into::into)
    }

    fn invocation(binding: RequestCheckBinding, invocation_id: &str) -> CheckerInvocation {
        const TEXT: &str = "inspect this text";
        CheckerInvocation {
            invocation_id: invocation_id.to_owned(),
            request_id: format!("request-{invocation_id}"),
            router_id: "guarded".to_owned(),
            router_binding_digest: "router-v2:sha256:test".to_owned(),
            checker: binding,
            content: vec![ContentFragment {
                role: ContentRole::User,
                kind: ContentFragmentKind::Text,
                text: Some(TEXT.to_owned()),
            }],
            coverage: RequestCheckCoverage {
                scope: RequestCheckCoverageScope::EntryRequestText,
                text_bytes: TEXT.len() as u64,
                text_fragments: 1,
                excluded_media_fragments: 0,
                status: RequestCheckCoverageStatus::CompleteWithinScope,
            },
        }
    }

    fn started_invocation(
        runtime: &RequestCheckRuntime,
        config: &Config,
        invocation_id: &str,
    ) -> anyhow::Result<(
        CheckerInvocation,
        RequestCheckReporter,
        RequestReceiptHandle,
    )> {
        let binding = binding(config)?;
        let invocation = invocation(binding.clone(), invocation_id);
        let router = RouterRequestIdentity {
            router_id: invocation.router_id.clone(),
            original_selector: "bitrouter/guarded".to_owned(),
            binding_digest: invocation.router_binding_digest.clone(),
        };
        let receipt = runtime.receipts().admit(
            &invocation.request_id,
            &router,
            std::slice::from_ref(&binding),
        )?;
        let reporter = receipt
            .mark_check_started(0, &invocation.invocation_id, invocation.coverage.clone())
            .ok_or_else(|| anyhow::anyhow!("request check did not start"))?;
        Ok((invocation, reporter, receipt))
    }

    fn cancel(receipt: RequestReceiptHandle) {
        receipt.finish(
            RequestReceiptOutcome::Cancelled,
            RequestDeliveryStatus::Unknown,
            Some(RequestFailureStage::RequestCheck),
        );
    }

    #[tokio::test]
    async fn cancelled_and_older_invocations_cannot_leave_stale_allow_evidence()
    -> anyhow::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
            .mount(&server)
            .await;
        let config = config(format!("{}/check", server.uri()), None, true, 5_000);
        let runtime = RequestCheckRuntime::activate(&config)?;
        let checker = runtime
            .checkers
            .get("safety")
            .ok_or_else(|| anyhow::anyhow!("missing checker"))?;
        let permits = checker.semaphore.acquire_many(32).await?;
        let actual = || -> anyhow::Result<CheckerActualUsage> {
            runtime
                .configured()
                .first()
                .and_then(|checker| checker.bindings.first())
                .and_then(|binding| binding.last_actual.clone())
                .ok_or_else(|| anyhow::anyhow!("missing actual observation"))
        };
        let (older_invocation, older_reporter, older_receipt) =
            started_invocation(&runtime, &config, "older")?;
        let mut older = Box::pin(runtime.check(older_invocation, older_reporter));
        assert!(futures::poll!(&mut older).is_pending());
        assert_eq!(actual()?.status, RequestCheckStatus::Pending);
        let (newer_invocation, newer_reporter, newer_receipt) =
            started_invocation(&runtime, &config, "newer")?;
        let mut newer = Box::pin(runtime.check(newer_invocation, newer_reporter));
        assert!(futures::poll!(&mut newer).is_pending());
        drop(older);
        cancel(older_receipt);
        assert_eq!(actual()?.invocation_id, "newer");
        assert_eq!(actual()?.status, RequestCheckStatus::Pending);
        drop(newer);
        cancel(newer_receipt);
        assert_eq!(actual()?.status, RequestCheckStatus::Interrupted);
        assert_eq!(actual()?.dispatch, RequestCheckDispatchStatus::NotAttempted);
        drop(permits);

        let (in_flight_invocation, in_flight_reporter, in_flight_receipt) =
            started_invocation(&runtime, &config, "in-flight")?;
        let mut in_flight = Box::pin(runtime.check(in_flight_invocation, in_flight_reporter));
        tokio::select! {
            _ = &mut in_flight => anyhow::bail!("delayed checker unexpectedly completed"),
            observed = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if server.received_requests().await.is_some_and(|requests| !requests.is_empty()) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }) => { observed?; }
        }
        assert_eq!(actual()?.status, RequestCheckStatus::Pending);
        assert_eq!(actual()?.dispatch, RequestCheckDispatchStatus::Attempted);
        drop(in_flight);
        cancel(in_flight_receipt);
        assert_eq!(actual()?.status, RequestCheckStatus::Interrupted);
        assert_eq!(actual()?.dispatch, RequestCheckDispatchStatus::Attempted);
        Ok(())
    }

    #[tokio::test]
    async fn malformed_credential_never_claims_remote_dispatch() -> anyhow::Result<()> {
        let config = config("http://127.0.0.1:9/check".into(), None, true, 500);
        let mut runtime = RequestCheckRuntime::activate(&config)?;
        let checker = runtime
            .checkers
            .get_mut("safety")
            .ok_or_else(|| anyhow::anyhow!("missing checker"))?;
        checker.credential = Some("invalid\ncredential".into());
        let (invocation, reporter, receipt) =
            started_invocation(&runtime, &config, "bad-credential")?;
        let result = runtime.check(invocation, reporter).await;
        assert!(matches!(
            result,
            Err(CheckerFailure {
                kind: CheckerFailureKind::NotConfigured,
                ..
            })
        ));
        receipt.mark_check_finished(
            0,
            RequestCheckStatus::Failed,
            None,
            None,
            Some(CheckerFailureKind::NotConfigured),
        );
        let actual = runtime
            .configured()
            .first()
            .and_then(|checker| checker.bindings.first())
            .and_then(|binding| binding.last_actual.clone())
            .ok_or_else(|| anyhow::anyhow!("missing actual observation"))?;
        assert_eq!(actual.dispatch, RequestCheckDispatchStatus::NotAttempted);
        Ok(())
    }

    #[tokio::test]
    async fn real_use_and_probe_are_observed_separately() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/check"))
            .respond_with(EchoDecision { decision: "allow" })
            .mount(&server)
            .await;
        let config = config(format!("{}/check", server.uri()), None, true, 500);
        let runtime = RequestCheckRuntime::activate(&config)?;
        let (invocation, reporter, receipt) =
            started_invocation(&runtime, &config, "actual-invocation")?;
        let decision = runtime
            .check(invocation, reporter)
            .await
            .map_err(|failure| anyhow::anyhow!("checker failed: {failure:?}"))?;
        assert!(matches!(&decision, CheckerDecision::Allow { .. }));
        let implementation_version = match decision {
            CheckerDecision::Allow {
                implementation_version,
            } => implementation_version,
            CheckerDecision::Deny { .. } => None,
        };
        receipt.mark_check_finished(
            0,
            RequestCheckStatus::Allowed,
            None,
            implementation_version,
            None,
        );
        receipt.finish(
            RequestReceiptOutcome::Completed,
            RequestDeliveryStatus::ServerCommitted,
            None,
        );

        let before_probe = runtime.configured();
        let actual = before_probe
            .first()
            .and_then(|checker| checker.bindings.first())
            .and_then(|binding| binding.last_actual.as_ref())
            .ok_or_else(|| anyhow::anyhow!("actual usage was not observed"))?;
        assert_eq!(actual.status, RequestCheckStatus::Allowed);
        assert_eq!(actual.invocation_id, "actual-invocation");
        assert!(
            before_probe
                .first()
                .is_some_and(|checker| checker.last_probe.is_none())
        );

        let probe = runtime.probe("safety").await;
        assert_eq!(probe.reachability, ProbeReachability::Reachable);
        assert_eq!(probe.protocol, ProbeProtocolStatus::Valid);
        let after_probe = runtime.configured();
        assert!(
            after_probe
                .first()
                .and_then(|checker| checker.last_probe.as_ref())
                .is_some_and(|latest| latest.protocol == ProbeProtocolStatus::Valid)
        );
        assert_eq!(runtime.receipts().list(10).receipts.len(), 1);
        assert_eq!(
            server
                .received_requests()
                .await
                .ok_or_else(|| anyhow::anyhow!("checker requests unavailable"))?
                .len(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn hostile_response_body_is_bounded_and_never_exposed() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        let hostile = "private-checker-body".repeat(2_000);
        Mock::given(method("POST"))
            .and(path("/check"))
            .respond_with(ResponseTemplate::new(200).set_body_string(hostile.clone()))
            .mount(&server)
            .await;
        let config = config(format!("{}/check", server.uri()), None, true, 500);
        let runtime = RequestCheckRuntime::activate(&config)?;
        let (invocation, reporter, receipt) =
            started_invocation(&runtime, &config, "hostile-response")?;
        let failure = runtime
            .check(invocation, reporter)
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("hostile checker response was accepted"))?;
        assert_eq!(failure.kind, CheckerFailureKind::InvalidResponse);
        assert_eq!(failure.detail.as_deref(), Some("response_too_large"));
        assert!(!format!("{failure:?}").contains(&hostile));
        receipt.mark_check_finished(
            0,
            RequestCheckStatus::Failed,
            None,
            None,
            Some(failure.kind),
        );
        Ok(())
    }

    #[tokio::test]
    async fn total_deadline_covers_the_response_body() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/check"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_json(json!({
                        "contract_version": 1,
                        "invocation_id": "slow",
                        "decision": "allow"
                    })),
            )
            .mount(&server)
            .await;
        let config = config(format!("{}/check", server.uri()), None, true, 10);
        let runtime = RequestCheckRuntime::activate(&config)?;
        let (invocation, reporter, receipt) = started_invocation(&runtime, &config, "slow")?;
        let failure = runtime
            .check(invocation, reporter)
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("slow checker response was accepted"))?;
        assert_eq!(failure.kind, CheckerFailureKind::Timeout);
        assert_eq!(failure.detail.as_deref(), Some("timeout"));
        receipt.mark_check_finished(
            0,
            RequestCheckStatus::Failed,
            None,
            None,
            Some(failure.kind),
        );
        Ok(())
    }

    #[tokio::test]
    async fn only_bound_checkers_require_credentials_at_activation() -> anyhow::Result<()> {
        let missing_env = format!(
            "BITROUTER_TEST_MISSING_CHECKER_CREDENTIAL_{}",
            uuid::Uuid::new_v4().simple()
        );
        let bound = config(
            "https://checker.example/check".to_owned(),
            Some(missing_env.clone()),
            true,
            500,
        );
        assert!(RequestCheckRuntime::activate(&bound).is_err());

        let unbound = config(
            "https://checker.example/check".to_owned(),
            Some(missing_env),
            false,
            500,
        );
        let runtime = RequestCheckRuntime::activate(&unbound)?;
        assert!(
            runtime
                .configured()
                .first()
                .is_some_and(|checker| !checker.credential_ready)
        );
        let probe = runtime.probe("safety").await;
        assert_eq!(probe.reachability, ProbeReachability::NotAttempted);
        assert_eq!(probe.error_code.as_deref(), Some("credential_missing"));
        Ok(())
    }
}
