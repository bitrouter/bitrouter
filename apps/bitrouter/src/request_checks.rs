//! Fail-closed runtime for compiled request-check extensions.

use async_trait::async_trait;
use bitrouter_sdk::config::Config;
use bitrouter_sdk::config::checker::CheckerConfig;
use bitrouter_sdk::config::router::MAX_CHECKER_TIMEOUT_MS;
use bitrouter_sdk::extension::request_check::{
    Decision, Input, Registration, RequestCheckCoverageStatus, validate_revision,
};
use bitrouter_sdk::language_model::request_checks::{
    CheckerFailure, CheckerFailureKind, CheckerResult, RequestCheckBinding, RequestCheckerRunner,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Maximum concurrent invocations admitted for one registered capability.
pub const MAX_CONCURRENT_INVOCATIONS_PER_CHECKER: usize = 32;
struct ActiveChecker {
    registration: Registration,
    semaphore: Arc<Semaphore>,
    bindings: Vec<ActiveBinding>,
}
struct ActiveBinding {
    binding: RequestCheckBinding,
}
/// Activated compiled request-check capabilities.
pub struct RequestCheckRuntime {
    checkers: HashMap<String, ActiveChecker>,
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
                    binding: binding.resolve(router_id, configured)?,
                });
            }
        }
        Ok(Self { checkers })
    }

    async fn invoke(
        &self,
        binding: RequestCheckBinding,
        input: Input,
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
            let decision = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                callback(&input)
            })
            .await
            .map_err(|_| failure(CheckerFailureKind::Internal, "native_execution_failed"))?;
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
    ) -> Result<CheckerResult, CheckerFailure> {
        let checker_id = binding.checker_id.clone();
        let started = Instant::now();
        let result = self.invoke(binding, input).await.and_then(|result| {
            result
                .decision
                .validate()
                .map_err(|_| failure(CheckerFailureKind::InvalidResponse, "invalid_decision"))?;
            Ok(result)
        });
        let elapsed_ms = started.elapsed().as_millis();
        match &result {
            Ok(CheckerResult {
                decision: Decision::Allow,
                revision,
            }) => tracing::debug!(
                checker_id,
                revision,
                elapsed_ms,
                outcome = "allow",
                "native request check completed"
            ),
            Ok(CheckerResult {
                decision: Decision::Deny { .. },
                revision,
            }) => tracing::info!(
                checker_id,
                revision,
                elapsed_ms,
                outcome = "deny",
                "native request check completed"
            ),
            Err(failure) => tracing::warn!(
                checker_id,
                failure_kind = ?failure.kind,
                elapsed_ms,
                outcome = "failed",
                "native request check failed closed"
            ),
        }
        result
    }
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
            let active_binding = binding(&config)?;
            let check_input = input();
            let runner = runtime.clone();
            let task = tokio::spawn(async move { runner.check(active_binding, check_input).await });
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
            let result = runtime.check(binding(&config)?, input()).await;
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

    #[tokio::test]
    async fn invalid_native_decision_is_classified_before_return() -> anyhow::Result<()> {
        let config = native_config();
        let runtime = RequestCheckRuntime::activate_with_registrations(
            &config,
            HashMap::from([(
                "safety".to_owned(),
                Registration::new(
                    "rules-v1",
                    Arc::new(|_| Decision::Deny {
                        reason_code: "invalid reason".to_owned(),
                    }),
                ),
            )]),
        )?;
        let error = runtime
            .check(binding(&config)?, input())
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("invalid native decision was accepted"))?;
        assert_eq!(error.kind, CheckerFailureKind::InvalidResponse);
        assert_eq!(error.detail.as_deref(), Some("invalid_decision"));
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
        assert!(runtime.checkers.contains_key("safety"));
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
        Ok(())
    }
}
