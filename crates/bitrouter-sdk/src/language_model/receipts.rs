//! Process-local, bounded request receipts for named-router check workflows.

use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::language_model::request_checks::{
    CheckerFailureKind, RequestCheckBinding, RequestCheckCoverage,
};
use crate::language_model::routing::RouterRequestIdentity;

/// Maximum checker results retained on one receipt.
pub const MAX_RECEIPT_CHECKS: usize = 16;
const MAX_REQUEST_ID_BYTES: usize = 256;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_DIAGNOSTIC_BYTES: usize = 1024;

/// Bounded receipt-store configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestReceiptStoreConfig {
    /// Maximum active plus completed entries retained in this process.
    pub capacity: usize,
    /// Retention for completed entries.
    pub completed_ttl: Duration,
}

impl Default for RequestReceiptStoreConfig {
    fn default() -> Self {
        Self {
            capacity: 4096,
            completed_ttl: Duration::from_secs(15 * 60),
        }
    }
}

/// Stable identity and binding evidence for one admitted entry request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequestReceiptIdentity {
    /// Unique identity of this admitted attempt. Unlike `request_id`, this is
    /// never reused by transport retries.
    pub receipt_id: String,
    /// Caller-visible request id.
    pub request_id: String,
    /// Process incarnation that owns this receipt.
    pub incarnation_id: String,
    /// Canonical named-router id.
    pub router_id: String,
    /// Redaction-safe effective router-binding digest.
    pub router_binding_digest: String,
}

/// Result of one configured request checker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequestCheckReceipt {
    /// Receipt contract version.
    pub contract_version: u16,
    /// Checker name, absent for the synthetic `not_enabled` result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checker_id: Option<String>,
    /// Redaction-safe checker-binding digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_digest: Option<String>,
    /// Per-request invocation id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    /// Stable result classification.
    pub status: RequestCheckStatus,
    /// Furthest remote-dispatch boundary reached by a started invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<RequestCheckDispatchStatus>,
    /// Time of the latest lifecycle observation in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_unix_ms: Option<u64>,
    /// Bounded ASCII denial code, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    /// Checker implementation version, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation_version: Option<String>,
    /// Stable failure kind for a failed invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<CheckerFailureKind>,
    /// Explicit entry-request coverage evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<RequestCheckCoverage>,
    /// Invocation start time in milliseconds since Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u64>,
    /// Invocation finish time in milliseconds since Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_unix_ms: Option<u64>,
}

/// Stable request-check result classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestCheckStatus {
    /// Configured but not yet invoked.
    NotRun,
    /// The invocation started but has not reached a decision.
    Pending,
    /// Local ownership ended before a checker decision was observed. This does
    /// not claim that a remote service stopped work.
    Interrupted,
    /// The named router has no entry-request checker configured.
    NotEnabled,
    /// The checker allowed the request.
    Allowed,
    /// The checker denied the request.
    Denied,
    /// The checker failed and the request failed closed.
    Failed,
    /// An earlier binding stopped the ordered checker chain.
    Skipped,
}

/// How far a real checker invocation progressed toward its remote service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestCheckDispatchStatus {
    /// The invocation was queued or rejected locally before an HTTP attempt.
    NotAttempted,
    /// An HTTP attempt began but no response headers were received.
    Attempted,
    /// Response headers were received from the checker.
    ResponseReceived,
}

/// Latest retained invocation started for one checker binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRequestCheck {
    /// Caller-visible gateway request id.
    pub request_id: String,
    /// Receipt-owned invocation evidence.
    pub check: RequestCheckReceipt,
}

/// Terminal request outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestReceiptOutcome {
    /// A successful provider result reached the server delivery boundary.
    Completed,
    /// A policy or checker denied the admitted request.
    Denied,
    /// The request failed after admission.
    Failed,
    /// A streaming caller disconnected before a successful terminal.
    ClientDisconnected,
    /// The owning future was cancelled before an explicit terminal transition.
    Cancelled,
}

/// What the server can honestly prove about downstream delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestDeliveryStatus {
    /// The request remains active and has not reached a delivery boundary.
    NotStarted,
    /// No success payload was available for delivery.
    NotApplicable,
    /// The server synchronously committed the payload to its response path.
    /// This does not prove that a remote client application consumed it.
    ServerCommitted,
    /// The response consumer disconnected before the server commit boundary.
    Disconnected,
    /// Delivery authorization or finalization failed.
    Failed,
    /// Cancellation prevented an exact delivery determination.
    Unknown,
}

/// Coarse, bounded failure location. Raw error strings are not retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestFailureStage {
    /// A normal pre-request policy or guardrail failed.
    PreRequest,
    /// External request checking failed.
    RequestCheck,
    /// Model selection or route resolution failed.
    Route,
    /// Provider dispatch or response processing failed.
    Upstream,
    /// Success-critical finalization or delivery failed.
    Delivery,
    /// No narrower classification was available.
    Internal,
}

/// Queryable process-local receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequestReceipt {
    /// Stable request and router identity.
    pub identity: RequestReceiptIdentity,
    /// Milliseconds since Unix epoch when admission succeeded.
    pub accepted_at_unix_ms: u64,
    /// Ordered entry-request checker results.
    pub checks: Vec<RequestCheckReceipt>,
    /// True once any provider attempt began.
    pub upstream_started: bool,
    /// Terminal outcome; absent while the request remains active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RequestReceiptOutcome>,
    /// Delivery evidence available to this process.
    pub delivery: RequestDeliveryStatus,
    /// Coarse failure stage for denied/failed outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_stage: Option<RequestFailureStage>,
    /// Milliseconds since Unix epoch when the request became terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_unix_ms: Option<u64>,
}

/// Result of an exact request-receipt lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RequestReceiptLookup {
    /// A receipt exists in the current process.
    Found {
        /// The retained receipt.
        receipt: RequestReceipt,
        /// Number of retained attempts carrying this caller request id.
        retained_matches: usize,
    },
    /// No matching current-process evidence exists. This deliberately does not
    /// claim whether the request never existed or aged out.
    Unknown {
        /// Requested request id.
        request_id: String,
        /// Requested incarnation, when supplied.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_incarnation: Option<String>,
        /// Incarnation served by this process.
        current_incarnation: String,
        /// Why current-process evidence is unavailable.
        reason: RequestReceiptUnknownReason,
    },
    /// The in-process store lost its health invariant and cannot answer
    /// authoritatively until the daemon restarts.
    Unavailable {
        /// Requested request id.
        request_id: String,
        /// Incarnation served by this process.
        current_incarnation: String,
    },
}

/// Reason an exact current-process receipt is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestReceiptUnknownReason {
    /// The caller asked about a different daemon incarnation.
    IncarnationMismatch,
    /// The current process retains no evidence for this request id.
    NotRetained,
}

/// Health of the process-local receipt store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestReceiptStoreHealth {
    /// Queries are authoritative within the documented retention bounds.
    Healthy,
    /// A synchronization failure made retained state non-authoritative.
    Unavailable,
}

/// Newest-first bounded receipt listing and its retention metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequestReceiptList {
    /// Current process incarnation.
    pub incarnation_id: String,
    /// Maximum retained active plus completed records.
    pub capacity: usize,
    /// Completed-record retention in seconds.
    pub completed_ttl_secs: u64,
    /// Whether the returned process-local view is authoritative.
    pub health: RequestReceiptStoreHealth,
    /// Newest-first receipts, capped by the requested limit and store capacity.
    pub receipts: Vec<RequestReceipt>,
}

/// Receipt admission failure. Admission failures occur before checker or model
/// dispatch and therefore cannot themselves create a receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestReceiptAdmissionError {
    /// All capacity is occupied by active requests.
    CapacityExhausted,
    /// The request or binding identity exceeds its public bound.
    InvalidIdentity,
    /// The store was configured with zero capacity.
    Disabled,
    /// The store lost its synchronization health invariant.
    StoreUnavailable,
}

impl fmt::Display for RequestReceiptAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CapacityExhausted => "request receipt capacity is occupied by active requests",
            Self::InvalidIdentity => "request receipt identity exceeds its allowed bound",
            Self::Disabled => "request receipt store is disabled",
            Self::StoreUnavailable => "request receipt store is unavailable",
        })
    }
}

impl std::error::Error for RequestReceiptAdmissionError {}

#[derive(Debug)]
struct StoredReceipt {
    receipt: RequestReceipt,
    sequence: u64,
    completed_at: Option<Instant>,
}

#[derive(Debug, Default)]
struct StoreState {
    records: HashMap<String, StoredReceipt>,
    order: VecDeque<String>,
    next_sequence: u64,
    latest_started: HashMap<String, LatestStartedCheck>,
}

#[derive(Debug)]
struct LatestStartedCheck {
    receipt_id: String,
    check_index: usize,
}

#[derive(Debug)]
struct RequestReceiptStoreInner {
    incarnation_id: String,
    config: RequestReceiptStoreConfig,
    state: Mutex<StoreState>,
    healthy: AtomicBool,
}

/// Cloneable process-local receipt store and independent query surface.
#[derive(Debug, Clone)]
pub struct RequestReceiptStore {
    inner: Arc<RequestReceiptStoreInner>,
}

impl RequestReceiptStore {
    /// Build a fresh store with a random process incarnation id.
    pub fn new(config: RequestReceiptStoreConfig) -> Self {
        Self::with_incarnation(config, uuid::Uuid::new_v4().to_string())
    }

    /// Build a store with an explicit incarnation id.
    pub fn with_incarnation(
        config: RequestReceiptStoreConfig,
        incarnation_id: impl Into<String>,
    ) -> Self {
        Self {
            inner: Arc::new(RequestReceiptStoreInner {
                incarnation_id: truncate(incarnation_id.into(), MAX_IDENTIFIER_BYTES),
                config,
                state: Mutex::new(StoreState::default()),
                healthy: AtomicBool::new(true),
            }),
        }
    }

    /// Current process incarnation.
    pub fn incarnation_id(&self) -> &str {
        &self.inner.incarnation_id
    }

    /// Reserve capacity and create an active receipt.
    pub fn admit(
        &self,
        request_id: &str,
        router: &RouterRequestIdentity,
        bindings: &[RequestCheckBinding],
    ) -> Result<RequestReceiptHandle, RequestReceiptAdmissionError> {
        if self.inner.config.capacity == 0 {
            return Err(RequestReceiptAdmissionError::Disabled);
        }
        if request_id.is_empty()
            || request_id.len() > MAX_REQUEST_ID_BYTES
            || router.router_id.is_empty()
            || router.router_id.len() > MAX_IDENTIFIER_BYTES
            || router.binding_digest.is_empty()
            || router.binding_digest.len() > MAX_IDENTIFIER_BYTES
            || bindings.len() > MAX_RECEIPT_CHECKS
            || bindings.iter().any(|binding| {
                binding.checker_id.is_empty()
                    || binding.checker_id.len() > MAX_IDENTIFIER_BYTES
                    || binding.binding_digest.is_empty()
                    || binding.binding_digest.len() > MAX_IDENTIFIER_BYTES
            })
        {
            return Err(RequestReceiptAdmissionError::InvalidIdentity);
        }
        let now = Instant::now();
        if !self.inner.healthy.load(Ordering::Acquire) {
            return Err(RequestReceiptAdmissionError::StoreUnavailable);
        }
        let Ok(mut state) = self.inner.state.lock() else {
            self.inner.healthy.store(false, Ordering::Release);
            return Err(RequestReceiptAdmissionError::StoreUnavailable);
        };
        prune_expired(&mut state, now, self.inner.config.completed_ttl);
        while state.records.len() >= self.inner.config.capacity {
            let Some(completed_id) = state.order.iter().find_map(|id| {
                state
                    .records
                    .get(id)
                    .and_then(|record| record.receipt.outcome.map(|_| id.clone()))
            }) else {
                return Err(RequestReceiptAdmissionError::CapacityExhausted);
            };
            remove_record(&mut state, &completed_id);
        }
        let receipt_id = format!("receipt_{}", uuid::Uuid::new_v4());
        if state.records.contains_key(&receipt_id) {
            return Err(RequestReceiptAdmissionError::StoreUnavailable);
        }
        let receipt = RequestReceipt {
            identity: RequestReceiptIdentity {
                receipt_id: receipt_id.clone(),
                request_id: request_id.to_owned(),
                incarnation_id: self.inner.incarnation_id.clone(),
                router_id: router.router_id.clone(),
                router_binding_digest: router.binding_digest.clone(),
            },
            accepted_at_unix_ms: unix_millis(),
            checks: if bindings.is_empty() {
                vec![RequestCheckReceipt {
                    contract_version: 1,
                    checker_id: None,
                    binding_digest: None,
                    invocation_id: None,
                    status: RequestCheckStatus::NotEnabled,
                    dispatch: None,
                    observed_at_unix_ms: None,
                    reason_code: None,
                    implementation_version: None,
                    failure_kind: None,
                    coverage: None,
                    started_at_unix_ms: None,
                    finished_at_unix_ms: None,
                }]
            } else {
                bindings
                    .iter()
                    .map(|binding| RequestCheckReceipt {
                        contract_version: 1,
                        checker_id: Some(binding.checker_id.clone()),
                        binding_digest: Some(binding.binding_digest.clone()),
                        invocation_id: None,
                        status: RequestCheckStatus::NotRun,
                        dispatch: None,
                        observed_at_unix_ms: None,
                        reason_code: None,
                        implementation_version: None,
                        failure_kind: None,
                        coverage: None,
                        started_at_unix_ms: None,
                        finished_at_unix_ms: None,
                    })
                    .collect()
            },
            upstream_started: false,
            outcome: None,
            delivery: RequestDeliveryStatus::NotStarted,
            failure_stage: None,
            completed_at_unix_ms: None,
        };
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        state.order.push_back(receipt_id.clone());
        state.records.insert(
            receipt_id.clone(),
            StoredReceipt {
                receipt,
                sequence,
                completed_at: None,
            },
        );
        Ok(RequestReceiptHandle {
            store: self.clone(),
            receipt_id,
            finished: AtomicBool::new(false),
        })
    }

    /// Look up one current-process receipt. A generated `receipt_...` id takes
    /// precedence over caller request-id matching; request ids return the
    /// newest retained attempt and the number of retained matches.
    pub fn get(&self, id: &str, incarnation_id: Option<&str>) -> RequestReceiptLookup {
        if !self.inner.healthy.load(Ordering::Acquire) {
            return RequestReceiptLookup::Unavailable {
                request_id: truncate(id.to_owned(), MAX_REQUEST_ID_BYTES),
                current_incarnation: self.inner.incarnation_id.clone(),
            };
        }
        let Ok(mut state) = self.inner.state.lock() else {
            self.inner.healthy.store(false, Ordering::Release);
            return RequestReceiptLookup::Unavailable {
                request_id: truncate(id.to_owned(), MAX_REQUEST_ID_BYTES),
                current_incarnation: self.inner.incarnation_id.clone(),
            };
        };
        prune_expired(&mut state, Instant::now(), self.inner.config.completed_ttl);
        if incarnation_id.is_none_or(|id| id == self.inner.incarnation_id) {
            if let Some(record) = state.records.get(id) {
                return RequestReceiptLookup::Found {
                    receipt: record.receipt.clone(),
                    retained_matches: 1,
                };
            }
            let mut matches = state
                .records
                .values()
                .filter(|record| record.receipt.identity.request_id == id)
                .collect::<Vec<_>>();
            matches.sort_unstable_by_key(|record| Reverse(record.sequence));
            if let Some(receipt) = matches.first() {
                return RequestReceiptLookup::Found {
                    receipt: receipt.receipt.clone(),
                    retained_matches: matches.len(),
                };
            }
        }
        RequestReceiptLookup::Unknown {
            request_id: truncate(id.to_owned(), MAX_REQUEST_ID_BYTES),
            requested_incarnation: incarnation_id
                .map(|value| truncate(value.to_owned(), MAX_IDENTIFIER_BYTES)),
            current_incarnation: self.inner.incarnation_id.clone(),
            reason: if incarnation_id.is_some_and(|id| id != self.inner.incarnation_id) {
                RequestReceiptUnknownReason::IncarnationMismatch
            } else {
                RequestReceiptUnknownReason::NotRetained
            },
        }
    }

    /// List retained receipts newest first.
    pub fn list(&self, limit: usize) -> RequestReceiptList {
        if !self.inner.healthy.load(Ordering::Acquire) {
            return self.unavailable_list();
        }
        let Ok(mut state) = self.inner.state.lock() else {
            self.inner.healthy.store(false, Ordering::Release);
            return self.unavailable_list();
        };
        prune_expired(&mut state, Instant::now(), self.inner.config.completed_ttl);
        let mut records = state.records.values().collect::<Vec<_>>();
        records.sort_unstable_by_key(|record| Reverse(record.sequence));
        RequestReceiptList {
            incarnation_id: self.inner.incarnation_id.clone(),
            capacity: self.inner.config.capacity,
            completed_ttl_secs: self.inner.config.completed_ttl.as_secs(),
            health: RequestReceiptStoreHealth::Healthy,
            receipts: records
                .into_iter()
                .take(limit.min(self.inner.config.capacity))
                .map(|record| record.receipt.clone())
                .collect(),
        }
    }

    /// Snapshot the latest retained invocation started for each binding.
    /// Start transitions are serialized by the store mutex, so concurrent
    /// requests have a deterministic latest-started view without timestamps.
    pub fn latest_started_checks(&self) -> HashMap<String, LatestRequestCheck> {
        if !self.inner.healthy.load(Ordering::Acquire) {
            return HashMap::new();
        }
        let Ok(mut state) = self.inner.state.lock() else {
            self.inner.healthy.store(false, Ordering::Release);
            return HashMap::new();
        };
        prune_expired(&mut state, Instant::now(), self.inner.config.completed_ttl);
        state
            .latest_started
            .iter()
            .filter_map(|(binding_digest, latest)| {
                let record = state.records.get(&latest.receipt_id)?;
                let check = record.receipt.checks.get(latest.check_index)?.clone();
                Some((
                    binding_digest.clone(),
                    LatestRequestCheck {
                        request_id: record.receipt.identity.request_id.clone(),
                        check,
                    },
                ))
            })
            .collect()
    }

    fn mutate(&self, request_id: &str, update: impl FnOnce(&mut StoredReceipt)) {
        if !self.inner.healthy.load(Ordering::Acquire) {
            return;
        }
        match self.inner.state.lock() {
            Ok(mut state) => {
                if let Some(record) = state.records.get_mut(request_id) {
                    update(record);
                }
            }
            Err(_) => self.inner.healthy.store(false, Ordering::Release),
        }
    }

    fn unavailable_list(&self) -> RequestReceiptList {
        RequestReceiptList {
            incarnation_id: self.inner.incarnation_id.clone(),
            capacity: self.inner.config.capacity,
            completed_ttl_secs: self.inner.config.completed_ttl.as_secs(),
            health: RequestReceiptStoreHealth::Unavailable,
            receipts: Vec::new(),
        }
    }
}

/// Required pipeline-owned lifecycle handle. Its drop fallback records
/// cancellation so active capacity cannot leak when a future is aborted.
pub struct RequestReceiptHandle {
    store: RequestReceiptStore,
    receipt_id: String,
    finished: AtomicBool,
}

impl RequestReceiptHandle {
    /// Mark one reserved checker invocation as started.
    pub fn mark_check_started(
        &self,
        index: usize,
        invocation_id: &str,
        coverage: RequestCheckCoverage,
    ) -> Option<RequestCheckReporter> {
        if !self.store.inner.healthy.load(Ordering::Acquire) {
            return None;
        }
        let Ok(mut state) = self.store.inner.state.lock() else {
            self.store.inner.healthy.store(false, Ordering::Release);
            return None;
        };
        let binding_digest = {
            let record = state.records.get_mut(&self.receipt_id)?;
            if record.receipt.outcome.is_some() {
                return None;
            }
            let check = record.receipt.checks.get_mut(index)?;
            if check.status != RequestCheckStatus::NotRun {
                return None;
            }
            check.invocation_id = Some(truncate(invocation_id.to_owned(), MAX_IDENTIFIER_BYTES));
            check.status = RequestCheckStatus::Pending;
            check.dispatch = Some(RequestCheckDispatchStatus::NotAttempted);
            check.coverage = Some(coverage);
            let now = unix_millis();
            check.started_at_unix_ms = Some(now);
            check.observed_at_unix_ms = Some(now);
            check.binding_digest.clone()?
        };
        state.latest_started.insert(
            binding_digest,
            LatestStartedCheck {
                receipt_id: self.receipt_id.clone(),
                check_index: index,
            },
        );
        Some(RequestCheckReporter {
            store: self.store.clone(),
            receipt_id: self.receipt_id.clone(),
            check_index: index,
        })
    }

    /// Complete one ordered checker invocation.
    pub fn mark_check_finished(
        &self,
        index: usize,
        status: RequestCheckStatus,
        reason_code: Option<String>,
        implementation_version: Option<String>,
        failure_kind: Option<CheckerFailureKind>,
    ) {
        if !matches!(
            status,
            RequestCheckStatus::Allowed | RequestCheckStatus::Denied | RequestCheckStatus::Failed
        ) {
            return;
        }
        self.store.mutate(&self.receipt_id, |record| {
            if record.receipt.outcome.is_some() {
                return;
            }
            if let Some(check) = record.receipt.checks.get_mut(index) {
                if check.status != RequestCheckStatus::Pending {
                    return;
                }
                check.status = status;
                check.reason_code = reason_code
                    .filter(|value| value.is_ascii())
                    .map(|value| truncate(value, MAX_DIAGNOSTIC_BYTES));
                check.implementation_version =
                    implementation_version.map(|value| truncate(value, MAX_IDENTIFIER_BYTES));
                check.failure_kind = failure_kind;
                let now = unix_millis();
                check.finished_at_unix_ms = Some(now);
                check.observed_at_unix_ms = Some(now);
            }
        });
    }

    /// Record that the first provider attempt began.
    pub fn mark_upstream_started(&self) {
        self.store.mutate(&self.receipt_id, |record| {
            record.receipt.upstream_started = true;
        });
    }

    /// Complete the receipt exactly once.
    pub fn finish(
        &self,
        outcome: RequestReceiptOutcome,
        delivery: RequestDeliveryStatus,
        failure_stage: Option<RequestFailureStage>,
    ) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        self.store.mutate(&self.receipt_id, |record| {
            if record.receipt.outcome.is_none() {
                record.receipt.outcome = Some(outcome);
                record.receipt.delivery = delivery;
                record.receipt.failure_stage = failure_stage;
                record.receipt.completed_at_unix_ms = Some(unix_millis());
                for check in &mut record.receipt.checks {
                    if check.status == RequestCheckStatus::NotRun {
                        check.status = RequestCheckStatus::Skipped;
                    } else if check.status == RequestCheckStatus::Pending {
                        check.status = RequestCheckStatus::Interrupted;
                        check.finished_at_unix_ms = record.receipt.completed_at_unix_ms;
                        check.observed_at_unix_ms = record.receipt.completed_at_unix_ms;
                    }
                }
                record.completed_at = Some(Instant::now());
            }
        });
    }
}

/// Receipt-owned progress reporter for one real checker invocation.
#[derive(Debug, Clone)]
pub struct RequestCheckReporter {
    store: RequestReceiptStore,
    receipt_id: String,
    check_index: usize,
}

impl RequestCheckReporter {
    /// Record that the HTTP attempt began.
    pub fn mark_dispatched(&self) {
        self.advance(RequestCheckDispatchStatus::Attempted);
    }

    /// Record that response headers arrived from the checker.
    pub fn mark_response_received(&self) {
        self.advance(RequestCheckDispatchStatus::ResponseReceived);
    }

    /// Read the current receipt-owned dispatch boundary.
    pub fn dispatch_status(&self) -> Option<RequestCheckDispatchStatus> {
        if !self.store.inner.healthy.load(Ordering::Acquire) {
            return None;
        }
        let Ok(state) = self.store.inner.state.lock() else {
            self.store.inner.healthy.store(false, Ordering::Release);
            return None;
        };
        state
            .records
            .get(&self.receipt_id)
            .and_then(|record| record.receipt.checks.get(self.check_index))
            .and_then(|check| check.dispatch)
    }

    fn advance(&self, next: RequestCheckDispatchStatus) {
        self.store.mutate(&self.receipt_id, |record| {
            let Some(check) = record.receipt.checks.get_mut(self.check_index) else {
                return;
            };
            if check.status != RequestCheckStatus::Pending {
                return;
            }
            let advances = match (check.dispatch, next) {
                (Some(RequestCheckDispatchStatus::ResponseReceived), _) => false,
                (
                    Some(RequestCheckDispatchStatus::Attempted),
                    RequestCheckDispatchStatus::NotAttempted,
                ) => false,
                (current, next) => current != Some(next),
            };
            if advances {
                check.dispatch = Some(next);
                check.observed_at_unix_ms = Some(unix_millis());
            }
        });
    }
}

impl Drop for RequestReceiptHandle {
    fn drop(&mut self) {
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        self.finish(
            RequestReceiptOutcome::Cancelled,
            RequestDeliveryStatus::Unknown,
            Some(RequestFailureStage::Internal),
        );
    }
}

fn prune_expired(state: &mut StoreState, now: Instant, ttl: Duration) {
    let expired = state
        .records
        .iter()
        .filter_map(|(id, record)| {
            record
                .completed_at
                .filter(|completed| now.saturating_duration_since(*completed) >= ttl)
                .map(|_| id.clone())
        })
        .collect::<Vec<_>>();
    for id in expired {
        remove_record(state, &id);
    }
}

fn remove_record(state: &mut StoreState, request_id: &str) {
    state.records.remove(request_id);
    state.order.retain(|id| id != request_id);
    state
        .latest_started
        .retain(|_, latest| latest.receipt_id != request_id);
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

fn truncate(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    value.truncate(boundary);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router(name: &str) -> RouterRequestIdentity {
        RouterRequestIdentity {
            router_id: name.to_owned(),
            original_selector: format!("bitrouter/{name}"),
            binding_digest: format!("digest-{name}"),
        }
    }

    fn binding(id: &str) -> RequestCheckBinding {
        RequestCheckBinding {
            checker_id: id.to_owned(),
            binding_digest: format!("digest-{id}"),
            max_input_bytes: 1024,
            timeout_ms: 100,
        }
    }

    fn coverage() -> RequestCheckCoverage {
        RequestCheckCoverage {
            scope: crate::language_model::request_checks::RequestCheckCoverageScope::EntryRequestText,
            text_bytes: 4,
            text_fragments: 1,
            excluded_media_fragments: 0,
            status: crate::language_model::request_checks::RequestCheckCoverageStatus::CompleteWithinScope,
        }
    }

    #[test]
    fn unavailable_store_rejects_admission_and_never_reports_success()
    -> Result<(), Box<dyn std::error::Error>> {
        let store = RequestReceiptStore::new(RequestReceiptStoreConfig::default());
        let active = store.admit("request-1", &router("coding"), &[])?;
        store.inner.healthy.store(false, Ordering::Release);
        active.finish(
            RequestReceiptOutcome::Completed,
            RequestDeliveryStatus::ServerCommitted,
            None,
        );
        assert!(matches!(
            store.get("request-1", None),
            RequestReceiptLookup::Unavailable { .. }
        ));
        let listed = store.list(10);
        assert_eq!(listed.health, RequestReceiptStoreHealth::Unavailable);
        assert!(listed.receipts.is_empty());
        assert!(matches!(
            store.admit("request-2", &router("coding"), &[]),
            Err(RequestReceiptAdmissionError::StoreUnavailable)
        ));
        Ok(())
    }

    #[test]
    fn active_receipts_are_never_evicted() -> Result<(), Box<dyn std::error::Error>> {
        let store = RequestReceiptStore::with_incarnation(
            RequestReceiptStoreConfig {
                capacity: 1,
                completed_ttl: Duration::from_secs(60),
            },
            "process-1",
        );
        let _active = store.admit("request-1", &router("coding"), &[])?;
        let error = store
            .admit("request-2", &router("coding"), &[])
            .err()
            .ok_or("second active receipt unexpectedly admitted")?;
        assert_eq!(error, RequestReceiptAdmissionError::CapacityExhausted);
        assert!(matches!(
            store.get("request-1", Some("process-1")),
            RequestReceiptLookup::Found { .. }
        ));
        Ok(())
    }

    #[test]
    fn completed_receipt_is_evicted_for_new_admission() -> Result<(), Box<dyn std::error::Error>> {
        let store = RequestReceiptStore::with_incarnation(
            RequestReceiptStoreConfig {
                capacity: 1,
                completed_ttl: Duration::from_secs(60),
            },
            "process-1",
        );
        let first = store.admit("request-1", &router("coding"), &[])?;
        first.finish(
            RequestReceiptOutcome::Denied,
            RequestDeliveryStatus::NotApplicable,
            Some(RequestFailureStage::PreRequest),
        );
        let _second = store.admit("request-2", &router("coding"), &[])?;
        assert!(matches!(
            store.get("request-1", Some("process-1")),
            RequestReceiptLookup::Unknown { .. }
        ));
        Ok(())
    }

    #[test]
    fn zero_ttl_expires_completed_but_not_active_receipts() -> Result<(), Box<dyn std::error::Error>>
    {
        let store = RequestReceiptStore::with_incarnation(
            RequestReceiptStoreConfig {
                capacity: 1,
                completed_ttl: Duration::ZERO,
            },
            "process-1",
        );
        let active = store.admit("request-1", &router("coding"), &[])?;
        assert!(matches!(
            store.get("request-1", Some("process-1")),
            RequestReceiptLookup::Found { .. }
        ));
        active.finish(
            RequestReceiptOutcome::Completed,
            RequestDeliveryStatus::ServerCommitted,
            None,
        );
        assert!(matches!(
            store.get("request-1", Some("process-1")),
            RequestReceiptLookup::Unknown {
                reason: RequestReceiptUnknownReason::NotRetained,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn old_incarnation_is_unknown_even_when_request_id_matches()
    -> Result<(), Box<dyn std::error::Error>> {
        let store = RequestReceiptStore::with_incarnation(
            RequestReceiptStoreConfig::default(),
            "process-2",
        );
        let _active = store.admit("request-1", &router("coding"), &[])?;
        assert!(matches!(
            store.get("request-1", Some("process-1")),
            RequestReceiptLookup::Unknown { .. }
        ));
        Ok(())
    }

    #[test]
    fn dropped_handle_finalizes_cancellation() -> Result<(), Box<dyn std::error::Error>> {
        let store = RequestReceiptStore::with_incarnation(
            RequestReceiptStoreConfig::default(),
            "process-1",
        );
        let active = store.admit("request-1", &router("coding"), &[])?;
        drop(active);
        let RequestReceiptLookup::Found { receipt, .. } = store.get("request-1", None) else {
            return Err("receipt missing".into());
        };
        assert_eq!(receipt.outcome, Some(RequestReceiptOutcome::Cancelled));
        assert_eq!(receipt.delivery, RequestDeliveryStatus::Unknown);
        Ok(())
    }

    #[test]
    fn repeated_request_id_retains_each_attempt() -> Result<(), Box<dyn std::error::Error>> {
        let store = RequestReceiptStore::with_incarnation(
            RequestReceiptStoreConfig::default(),
            "process-1",
        );
        let first = store.admit("request-1", &router("coding"), &[])?;
        first.finish(
            RequestReceiptOutcome::Completed,
            RequestDeliveryStatus::ServerCommitted,
            None,
        );
        let second = store.admit("request-1", &router("restricted"), &[])?;
        second.finish(
            RequestReceiptOutcome::Denied,
            RequestDeliveryStatus::NotApplicable,
            Some(RequestFailureStage::PreRequest),
        );
        let RequestReceiptLookup::Found {
            receipt,
            retained_matches,
        } = store.get("request-1", None)
        else {
            return Err("repeated receipt missing".into());
        };
        assert_eq!(retained_matches, 2);
        assert_eq!(receipt.identity.router_id, "restricted");
        let retained = store.list(10).receipts;
        assert_eq!(retained.len(), 2);
        let older = retained
            .iter()
            .find(|candidate| candidate.identity.router_id == "coding")
            .ok_or("older receipt missing")?;
        let RequestReceiptLookup::Found {
            receipt: exact,
            retained_matches: exact_matches,
        } = store.get(&older.identity.receipt_id, None)
        else {
            return Err("exact receipt lookup missing".into());
        };
        assert_eq!(exact_matches, 1);
        assert_eq!(exact.identity.receipt_id, older.identity.receipt_id);
        Ok(())
    }

    #[test]
    fn checker_slots_show_started_and_skipped_states() -> Result<(), Box<dyn std::error::Error>> {
        let store = RequestReceiptStore::with_incarnation(
            RequestReceiptStoreConfig::default(),
            "process-1",
        );
        let bindings = ["first", "second"].map(binding);
        let handle = store.admit("request-1", &router("coding"), &bindings)?;
        handle.mark_check_started(0, "invocation-1", coverage());
        handle.mark_check_finished(
            0,
            RequestCheckStatus::Denied,
            Some("blocked".to_owned()),
            None,
            None,
        );
        handle.finish(
            RequestReceiptOutcome::Denied,
            RequestDeliveryStatus::NotApplicable,
            Some(RequestFailureStage::RequestCheck),
        );
        let RequestReceiptLookup::Found { receipt, .. } = store.get("request-1", None) else {
            return Err("receipt missing".into());
        };
        assert_eq!(receipt.checks[0].status, RequestCheckStatus::Denied);
        assert!(receipt.checks[0].started_at_unix_ms.is_some());
        assert!(receipt.checks[0].finished_at_unix_ms.is_some());
        assert_eq!(receipt.checks[1].status, RequestCheckStatus::Skipped);
        Ok(())
    }

    #[test]
    fn reporter_is_monotonic_and_cannot_mutate_terminal_check()
    -> Result<(), Box<dyn std::error::Error>> {
        let store = RequestReceiptStore::with_incarnation(
            RequestReceiptStoreConfig::default(),
            "process-1",
        );
        let handle = store.admit("request-1", &router("coding"), &[binding("safety")])?;
        let reporter = handle
            .mark_check_started(0, "invocation-1", coverage())
            .ok_or("check did not start")?;
        reporter.mark_response_received();
        reporter.mark_dispatched();
        assert_eq!(
            reporter.dispatch_status(),
            Some(RequestCheckDispatchStatus::ResponseReceived)
        );
        handle.mark_check_finished(0, RequestCheckStatus::Allowed, None, None, None);
        reporter.mark_dispatched();
        assert!(
            handle
                .mark_check_started(0, "invocation-2", coverage())
                .is_none()
        );
        let RequestReceiptLookup::Found { receipt, .. } = store.get("request-1", None) else {
            return Err("receipt missing".into());
        };
        assert_eq!(receipt.checks[0].status, RequestCheckStatus::Allowed);
        assert_eq!(
            receipt.checks[0].dispatch,
            Some(RequestCheckDispatchStatus::ResponseReceived)
        );

        let terminal = store.admit("request-2", &router("coding"), &[binding("safety")])?;
        let terminal_reporter = terminal
            .mark_check_started(0, "invocation-2", coverage())
            .ok_or("terminal check did not start")?;
        terminal.mark_check_finished(0, RequestCheckStatus::Allowed, None, None, None);
        terminal.finish(
            RequestReceiptOutcome::Completed,
            RequestDeliveryStatus::ServerCommitted,
            None,
        );
        terminal_reporter.mark_response_received();
        assert_eq!(
            terminal_reporter.dispatch_status(),
            Some(RequestCheckDispatchStatus::NotAttempted)
        );
        Ok(())
    }

    #[test]
    fn evicting_latest_started_check_does_not_revive_older_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let store = RequestReceiptStore::with_incarnation(
            RequestReceiptStoreConfig {
                capacity: 2,
                completed_ttl: Duration::from_secs(60),
            },
            "process-1",
        );
        let checker = binding("safety");
        let older = store.admit(
            "request-older",
            &router("coding"),
            std::slice::from_ref(&checker),
        )?;
        older
            .mark_check_started(0, "invocation-older", coverage())
            .ok_or("older check did not start")?;
        older.mark_check_finished(0, RequestCheckStatus::Allowed, None, None, None);
        let newer = store.admit(
            "request-newer",
            &router("coding"),
            std::slice::from_ref(&checker),
        )?;
        newer
            .mark_check_started(0, "invocation-newer", coverage())
            .ok_or("newer check did not start")?;
        newer.mark_check_finished(0, RequestCheckStatus::Denied, None, None, None);
        newer.finish(
            RequestReceiptOutcome::Denied,
            RequestDeliveryStatus::NotApplicable,
            Some(RequestFailureStage::RequestCheck),
        );

        let latest = store.latest_started_checks();
        assert_eq!(
            latest
                .get(&checker.binding_digest)
                .and_then(|view| view.check.invocation_id.as_deref()),
            Some("invocation-newer")
        );
        let _third = store.admit("request-third", &router("coding"), &[])?;

        assert!(matches!(
            store.get("request-older", None),
            RequestReceiptLookup::Found { .. }
        ));
        assert!(matches!(
            store.get("request-newer", None),
            RequestReceiptLookup::Unknown { .. }
        ));
        assert!(
            !store
                .latest_started_checks()
                .contains_key(&checker.binding_digest)
        );
        Ok(())
    }
}
