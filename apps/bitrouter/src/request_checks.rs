//! Fail-closed runtime for compiled request-check extensions.

use async_trait::async_trait;
use bitrouter_sdk::config::Config;
use bitrouter_sdk::config::checker::CheckerConfig;
use bitrouter_sdk::config::router::MAX_CHECKER_TIMEOUT_MS;
use bitrouter_sdk::extension::request_check::{
    Input, Registration, RequestCheckCoverageStatus, validate_revision,
};
use bitrouter_sdk::language_model::receipts::{
    LatestRequestCheck, RequestCheckDispatchStatus, RequestCheckReporter, RequestCheckStatus,
    RequestReceiptStore, RequestReceiptStoreConfig,
};
use bitrouter_sdk::language_model::request_checks::{
    CheckerFailure, CheckerFailureKind, CheckerResult, RequestCheckBinding, RequestCheckerRunner,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Maximum concurrent invocations admitted for one registered capability.
pub const MAX_CONCURRENT_INVOCATIONS_PER_CHECKER: usize = 32;
struct ActiveChecker {
    registration: Registration,
    semaphore: Arc<Semaphore>,
    bindings: Vec<ActiveBinding>,
}
struct ActiveBinding {
    router_id: String,
    binding: RequestCheckBinding,
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
    /// Host-owned per-check invocation identity.
    pub invocation_id: String,
    /// Binding identity used for this invocation.
    pub binding_digest: String,
    /// Current or terminal checker outcome.
    pub status: RequestCheckStatus,
    /// Furthest execution boundary reached.
    pub dispatch: RequestCheckDispatchStatus,
    /// Registered code/rules revision, when a valid decision completed.
    pub implementation_version: Option<String>,
    /// Observation time in Unix milliseconds.
    pub observed_at_unix_ms: u64,
}

/// Running registration and configuration evidence, separate from actual usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckerInfo {
    /// Configured capability instance id.
    pub checker_id: String,
    /// Matched code and rules revision.
    pub revision: String,
    /// Whether a matching compiled callback was registered at startup.
    pub registered: bool,
    /// Per-instance concurrency ceiling.
    pub max_concurrent_invocations: usize,
    /// Running router bindings and latest actual execution evidence.
    pub bindings: Vec<CheckerBindingInfo>,
}

/// Compiled capabilities plus the receipt store shared with the pipeline.
pub struct RequestCheckRuntime {
    checkers: HashMap<String, ActiveChecker>,
    receipts: RequestReceiptStore,
}

impl RequestCheckRuntime {
    /// Activate an ordinary host without custom capability registrations.
    pub fn activate(config: &Config) -> anyhow::Result<Self> {
        Self::activate_with_registrations(config, HashMap::new())
    }

    /// Activate configured callbacks. Missing or mismatched registrations fail
    /// startup. Undeclared registrations remain inactive and allocate no runtime
    /// entries; registration alone never enables a global check.
    pub(crate) fn activate_with_registrations(
        config: &Config,
        mut registrations: HashMap<String, Registration>,
    ) -> anyhow::Result<Self> {
        config.validate_router_config()?;
        for registration in registrations.values() {
            validate_revision(&registration.revision)?;
        }
        let mut checkers = HashMap::new();
        for (id, configured) in &config.checkers {
            let CheckerConfig::Native { native } = configured;
            let registration = registrations.remove(id).ok_or_else(|| anyhow::anyhow!("native checker '{id}' is not registered; build a custom host that links the extension"))?;
            anyhow::ensure!(
                registration.revision == native.revision,
                "native checker '{id}' revision does not match configuration"
            );
            checkers.insert(
                id.clone(),
                ActiveChecker {
                    registration,
                    semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_INVOCATIONS_PER_CHECKER)),
                    bindings: Vec::new(),
                },
            );
        }
        for (router_id, router) in &config.routers {
            for binding in &router.checks.request {
                let configured = config.checkers.get(&binding.checker).ok_or_else(|| {
                    anyhow::anyhow!(
                        "router '{router_id}' references missing checker '{}'",
                        binding.checker
                    )
                })?;
                let checker = checkers.get_mut(&binding.checker).ok_or_else(|| {
                    anyhow::anyhow!("checker '{}' is not registered", binding.checker)
                })?;
                checker.bindings.push(ActiveBinding {
                    router_id: router_id.clone(),
                    binding: binding.resolve(router_id, configured)?,
                });
            }
        }
        for checker in checkers.values_mut() {
            checker
                .bindings
                .sort_by(|left, right| left.router_id.cmp(&right.router_id));
        }
        Ok(Self {
            checkers,
            receipts: RequestReceiptStore::new(RequestReceiptStoreConfig::default()),
        })
    }
    /// Process-local receipts for the same daemon incarnation as this runtime.
    pub fn receipts(&self) -> RequestReceiptStore {
        self.receipts.clone()
    }

    /// Sorted, redaction-safe running checker inventory.
    pub fn configured(&self) -> Vec<CheckerInfo> {
        let latest_actual = self.receipts.latest_started_checks();
        let mut inventory = self
            .checkers
            .iter()
            .map(|(checker_id, checker)| CheckerInfo {
                checker_id: checker_id.clone(),
                revision: checker.registration.revision.clone(),
                registered: true,
                max_concurrent_invocations: MAX_CONCURRENT_INVOCATIONS_PER_CHECKER,
                bindings: checker
                    .bindings
                    .iter()
                    .map(|active| {
                        let binding = &active.binding;
                        CheckerBindingInfo {
                            router_id: active.router_id.clone(),
                            binding_digest: binding.binding_digest.clone(),
                            timeout_ms: binding.timeout_ms,
                            max_input_bytes: binding.max_input_bytes,
                            last_actual: latest_actual
                                .get(&binding.binding_digest)
                                .cloned()
                                .and_then(actual_usage),
                        }
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        inventory.sort_by(|left, right| left.checker_id.cmp(&right.checker_id));
        inventory
    }

    async fn invoke(
        &self,
        binding: RequestCheckBinding,
        input: Input,
        reporter: &RequestCheckReporter,
    ) -> Result<CheckerResult, CheckerFailure> {
        let checker = self
            .checkers
            .get(&binding.checker_id)
            .ok_or_else(|| failure(CheckerFailureKind::NotConfigured, "not_configured"))?;
        let active = checker
            .bindings
            .iter()
            .find(|active| active.binding.binding_digest == binding.binding_digest)
            .ok_or_else(|| failure(CheckerFailureKind::NotConfigured, "binding_not_active"))?;
        if active.binding != binding {
            return Err(failure(
                CheckerFailureKind::NotConfigured,
                "binding_mismatch",
            ));
        }
        if binding.timeout_ms == 0 || binding.timeout_ms > MAX_CHECKER_TIMEOUT_MS {
            return Err(failure(CheckerFailureKind::Internal, "invalid_deadline"));
        }
        if input.coverage.text_bytes > binding.max_input_bytes
            || input.coverage.status == RequestCheckCoverageStatus::InputTooLarge
        {
            return Err(failure(
                CheckerFailureKind::InputTooLarge,
                "input_too_large",
            ));
        }
        let deadline = Duration::from_millis(binding.timeout_ms);
        let operation = async {
            let permit = checker
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| failure(CheckerFailureKind::Internal, "runtime_closed"))?;
            let callback = checker.registration.callback.clone();
            // Once started, trusted synchronous code cannot be forcibly stopped.
            // Keep admission attached to work after request timeout/cancellation.
            reporter.mark_dispatched();
            let decision = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                callback(&input)
            })
            .await
            .map_err(|_| failure(CheckerFailureKind::Internal, "native_execution_failed"))?;
            reporter.mark_response_received();
            Ok(CheckerResult {
                decision,
                revision: checker.registration.revision.clone(),
            })
        };
        tokio::time::timeout(deadline, operation)
            .await
            .map_err(|_| failure(CheckerFailureKind::Timeout, "timeout"))?
    }
}

fn failure(kind: CheckerFailureKind, detail: &str) -> CheckerFailure {
    CheckerFailure {
        kind,
        detail: Some(detail.to_owned()),
    }
}

#[async_trait]
impl RequestCheckerRunner for RequestCheckRuntime {
    async fn check(
        &self,
        binding: RequestCheckBinding,
        input: Input,
        reporter: RequestCheckReporter,
    ) -> Result<CheckerResult, CheckerFailure> {
        self.invoke(binding, input, &reporter).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::config::RoutingConfig;
    use bitrouter_sdk::config::router::{
        DEFAULT_CHECKER_MAX_INPUT_BYTES, RouterChecks, RouterConfig, RouterDefaults,
        RouterRequestCheck, RouterSelection,
    };
    use bitrouter_sdk::extension::request_check::{
        Callback as CheckCallback, ContentFragment, ContentFragmentKind, ContentRole, Decision,
        RequestCheckCoverage, RequestCheckCoverageScope,
    };
    use bitrouter_sdk::language_model::receipts::{
        RequestDeliveryStatus, RequestFailureStage, RequestReceiptHandle, RequestReceiptOutcome,
    };
    use bitrouter_sdk::language_model::routing::RouterRequestIdentity;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;
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

    fn input() -> Input {
        const TEXT: &str = "inspect this text";
        Input {
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
        RequestCheckBinding,
        Input,
        RequestCheckReporter,
        RequestReceiptHandle,
    )> {
        let binding = binding(config)?;
        let input = input();
        let router = RouterRequestIdentity {
            router_id: "guarded".to_owned(),
            original_selector: "bitrouter/guarded".to_owned(),
            binding_digest: "router-v2:sha256:test".to_owned(),
        };
        let receipt = runtime.receipts().admit(
            &format!("request-{invocation_id}"),
            &router,
            std::slice::from_ref(&binding),
        )?;
        let reporter = receipt
            .mark_check_started(0, invocation_id, input.coverage.clone())
            .ok_or_else(|| anyhow::anyhow!("request check did not start"))?;
        Ok((binding, input, reporter, receipt))
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
        let mut config = native_config();
        config
            .routers
            .get_mut("guarded")
            .ok_or_else(|| anyhow::anyhow!("no guarded router"))?
            .checks
            .request[0]
            .timeout_ms = 5_000;
        let started = Arc::new(tokio::sync::Notify::new());
        let signal = started.clone();
        let runtime = RequestCheckRuntime::activate_with_registrations(
            &config,
            HashMap::from([(
                "safety".to_owned(),
                Registration::new(
                    "rules-v1",
                    Arc::new(move |_| {
                        signal.notify_one();
                        std::thread::sleep(Duration::from_millis(100));
                        Decision::Allow
                    }),
                ),
            )]),
        )?;
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
        let (older_binding, older_input, older_reporter, older_receipt) =
            started_invocation(&runtime, &config, "older")?;
        let mut older = Box::pin(runtime.check(older_binding, older_input, older_reporter));
        assert!(futures::poll!(&mut older).is_pending());
        assert_eq!(actual()?.status, RequestCheckStatus::Pending);
        let (newer_binding, newer_input, newer_reporter, newer_receipt) =
            started_invocation(&runtime, &config, "newer")?;
        let mut newer = Box::pin(runtime.check(newer_binding, newer_input, newer_reporter));
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

        let (in_flight_binding, in_flight_input, in_flight_reporter, in_flight_receipt) =
            started_invocation(&runtime, &config, "in-flight")?;
        let mut in_flight =
            Box::pin(runtime.check(in_flight_binding, in_flight_input, in_flight_reporter));
        tokio::select! {
            _ = &mut in_flight => anyhow::bail!("delayed checker unexpectedly completed"),
            observed = tokio::time::timeout(Duration::from_secs(2), started.notified()) => { observed?; }
        }
        assert_eq!(actual()?.status, RequestCheckStatus::Pending);
        assert_eq!(actual()?.dispatch, RequestCheckDispatchStatus::Attempted);
        drop(in_flight);
        cancel(in_flight_receipt);
        assert_eq!(actual()?.status, RequestCheckStatus::Interrupted);
        assert_eq!(actual()?.dispatch, RequestCheckDispatchStatus::Attempted);
        Ok(())
    }

    fn native_config() -> Config {
        let mut config = Config::default();
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
                        timeout_ms: 100,
                        max_input_bytes: DEFAULT_CHECKER_MAX_INPUT_BYTES,
                    }],
                },
            },
        );
        config.checkers.insert(
            "safety".to_owned(),
            CheckerConfig::Native {
                native: bitrouter_sdk::config::checker::NativeCheckerConfig {
                    revision: "rules-v1".to_owned(),
                },
            },
        );
        config
    }

    #[tokio::test]
    async fn native_timeout_and_cancellation_hold_admission_until_cpu_work_finishes()
    -> anyhow::Result<()> {
        use bitrouter_sdk::extension::request_check::Decision as CheckDecision;
        use std::sync::atomic::AtomicUsize;
        for cancel in [false, true] {
            let config = native_config();
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let wait = Mutex::new(wait);
            let started = Arc::new(tokio::sync::Notify::new());
            let signal = started.clone();
            let calls = Arc::new(AtomicUsize::new(0));
            let captured = calls.clone();
            let callback: Arc<CheckCallback> = Arc::new(move |_| {
                captured.fetch_add(1, Ordering::SeqCst);
                signal.notify_one();
                if let Ok(receiver) = wait.lock() {
                    // Bound the test even if an assertion fails before release.
                    let _ = receiver.recv_timeout(Duration::from_secs(5));
                }
                CheckDecision::Allow
            });
            let runtime = Arc::new(RequestCheckRuntime::activate_with_registrations(
                &config,
                HashMap::from([("safety".to_owned(), Registration::new("rules-v1", callback))]),
            )?);
            let semaphore = runtime
                .checkers
                .get("safety")
                .ok_or_else(|| anyhow::anyhow!("no checker"))?
                .semaphore
                .clone();
            // Reserve every slot except one; only that last slot may start work.
            let held = semaphore
                .clone()
                .acquire_many_owned((MAX_CONCURRENT_INVOCATIONS_PER_CHECKER - 1) as u32)
                .await?;
            let (binding, input, reporter, _receipt) =
                started_invocation(&runtime, &config, "native-in-flight")?;
            let runner = runtime.clone();
            let task = tokio::spawn(async move { runner.check(binding, input, reporter).await });
            tokio::time::timeout(Duration::from_secs(2), started.notified()).await?;
            if cancel {
                task.abort();
                assert!(task.await.is_err());
            } else {
                let result = task.await?;
                assert!(matches!(
                    result,
                    Err(CheckerFailure {
                        kind: CheckerFailureKind::Timeout,
                        ..
                    })
                ));
            }
            assert_eq!(semaphore.available_permits(), 0);
            let (binding, input, reporter, _receipt) =
                started_invocation(&runtime, &config, "native-queued")?;
            let result = runtime.check(binding, input, reporter).await;
            assert!(matches!(
                result,
                Err(CheckerFailure {
                    kind: CheckerFailureKind::Timeout,
                    ..
                })
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            release.send(())?;
            let restored =
                tokio::time::timeout(Duration::from_secs(2), semaphore.clone().acquire_owned())
                    .await??;
            drop(restored);
            drop(held);
            assert_eq!(
                semaphore.available_permits(),
                MAX_CONCURRENT_INVOCATIONS_PER_CHECKER
            );
        }
        Ok(())
    }

    #[test]
    fn native_activation_rejects_missing_mismatched_and_invalid_registrations() -> anyhow::Result<()>
    {
        use bitrouter_sdk::extension::request_check::Decision as CheckDecision;
        let config = native_config();
        assert!(RequestCheckRuntime::activate(&config).is_err());
        let register =
            |revision: &str| Registration::new(revision, Arc::new(|_| CheckDecision::Allow));
        let invalid_revision = RequestCheckRuntime::activate_with_registrations(
            &config,
            HashMap::from([("safety".to_owned(), register("rules:v1"))]),
        )
        .err()
        .ok_or_else(|| anyhow::anyhow!("invalid native revision activated"))?;
        assert!(invalid_revision.to_string().contains("revision"));
        assert!(
            RequestCheckRuntime::activate_with_registrations(
                &config,
                HashMap::from([("safety".to_owned(), register("wrong")),])
            )
            .is_err()
        );
        assert!(
            RequestCheckRuntime::activate_with_registrations(
                &config,
                HashMap::from([
                    ("safety".to_owned(), register("rules-v1")),
                    ("undeclared".to_owned(), register("invalid:revision")),
                ])
            )
            .is_err()
        );
        let runtime = RequestCheckRuntime::activate_with_registrations(
            &config,
            HashMap::from([("safety".to_owned(), register("rules-v1"))]),
        )?;
        assert!(runtime.configured()[0].registered);
        Ok(())
    }

    #[test]
    fn undeclared_registrations_allocate_no_runtime_entries() -> anyhow::Result<()> {
        let config = native_config();
        let runtime = RequestCheckRuntime::activate_with_registrations(
            &config,
            HashMap::from([
                (
                    "safety".to_owned(),
                    Registration::new("rules-v1", Arc::new(|_| Decision::Allow)),
                ),
                (
                    "undeclared".to_owned(),
                    Registration::new("rules-v1", Arc::new(|_| Decision::Allow)),
                ),
            ]),
        )?;
        assert_eq!(runtime.checkers.len(), 1);
        assert!(!runtime.checkers.contains_key("undeclared"));
        assert!(runtime.receipts().list(10).receipts.is_empty());
        Ok(())
    }
}
