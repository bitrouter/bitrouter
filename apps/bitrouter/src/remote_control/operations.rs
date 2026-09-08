//! Bounded, boot-local reload ownership and recovery after HTTP disconnects.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::daemon::DaemonReloader;
use crate::reload::{ReloadAdmissionError, ReloadOutcome, ReloadReport};

pub const MAX_OPERATIONS: usize = 1_024;
pub const RETENTION_SECONDS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReloadInput {
    pub request_id: String,
    pub expected_server_instance_id: String,
    pub expected_generation: u64,
}

impl ReloadInput {
    fn validate(&mut self) -> Result<(), OperationError> {
        if self.request_id.len() > 256 || self.expected_server_instance_id.len() > 256 {
            return Err(OperationError::InvalidRequest);
        }
        self.request_id = uuid::Uuid::parse_str(&self.request_id)
            .map_err(|_| OperationError::InvalidRequest)?
            .to_string();
        self.expected_server_instance_id = uuid::Uuid::parse_str(&self.expected_server_instance_id)
            .map_err(|_| OperationError::InvalidRequest)?
            .to_string();
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Running,
    Succeeded,
    Failed,
    PartiallyApplied,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OperationReport {
    pub request_id: String,
    pub server_instance_id: String,
    pub generation_before: u64,
    pub status: OperationStatus,
    pub accepted_at_unix_ms: i64,
    pub completed_at_unix_ms: Option<i64>,
    pub retain_until_unix_ms: Option<i64>,
    pub lookup_url: String,
    pub result: Option<ReloadReport>,
}

impl OperationReport {
    pub fn is_running(&self) -> bool {
        self.status == OperationStatus::Running
    }
    pub fn succeeded(&self) -> bool {
        self.status == OperationStatus::Succeeded
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationError {
    InvalidRequest,
    IdempotencyConflict,
    ServerInstanceChanged,
    StaleGeneration,
    ReloadInProgress,
    Unsupported,
    Capacity,
    GenerationExhausted,
    NotFound,
    Expired,
}

impl OperationError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::ServerInstanceChanged => "server_instance_changed",
            Self::StaleGeneration => "stale_generation",
            Self::ReloadInProgress => "reload_in_progress",
            Self::Unsupported => "unsupported_action",
            Self::GenerationExhausted => "generation_exhausted",
            Self::Capacity => "operation_capacity",
            Self::NotFound => "operation_not_found",
            Self::Expired => "operation_expired",
        }
    }
}

impl std::fmt::Display for OperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for OperationError {}

impl From<ReloadAdmissionError> for OperationError {
    fn from(error: ReloadAdmissionError) -> Self {
        match error {
            ReloadAdmissionError::ServerInstanceChanged => Self::ServerInstanceChanged,
            ReloadAdmissionError::StaleGeneration => Self::StaleGeneration,
            ReloadAdmissionError::ReloadInProgress => Self::ReloadInProgress,
            ReloadAdmissionError::GenerationExhausted => Self::GenerationExhausted,
            ReloadAdmissionError::Unsupported => Self::Unsupported,
        }
    }
}

struct StoredOperation {
    input: ReloadInput,
    report: OperationReport,
    completed: Option<Instant>,
}

impl StoredOperation {
    fn expired(&self) -> bool {
        self.completed
            .is_some_and(|completed| completed.elapsed() >= Duration::from_secs(RETENTION_SECONDS))
    }
}

/// Registry locks precede the coordinator's short admission lock. Execution
/// never holds the registry lock and never depends on a client connection.
pub struct OperationService {
    reloader: Arc<dyn DaemonReloader>,
    registry: Mutex<BTreeMap<(String, String), StoredOperation>>,
}

impl OperationService {
    pub fn new(reloader: Arc<dyn DaemonReloader>) -> Self {
        Self {
            reloader,
            registry: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn state(&self) -> Result<crate::reload::ReloadState, OperationError> {
        self.reloader
            .reload_state()
            .ok_or(OperationError::Unsupported)
    }

    pub async fn submit(
        self: &Arc<Self>,
        credential_id: &str,
        mut input: ReloadInput,
    ) -> Result<OperationReport, OperationError> {
        input.validate()?;
        let key = (credential_id.to_owned(), input.request_id.clone());
        let mut registry = self.registry.lock().await;
        if let Some(existing) = registry.get(&key) {
            if existing.expired() {
                return Err(OperationError::Expired);
            }
            return if existing.input == input {
                Ok(existing.report.clone())
            } else {
                Err(OperationError::IdempotencyConflict)
            };
        }
        registry.retain(|_, operation| !operation.expired());
        if registry.len() >= MAX_OPERATIONS {
            return Err(OperationError::Capacity);
        }

        // Admission atomically validates generation and claims exclusive reload
        // ownership before the HTTP handler can publish a running operation.
        let reservation = self.reloader.reserve_remote(
            &input.expected_server_instance_id,
            input.expected_generation,
        )?;
        let report = OperationReport {
            request_id: input.request_id.clone(),
            server_instance_id: input.expected_server_instance_id.clone(),
            generation_before: input.expected_generation,
            status: OperationStatus::Running,
            accepted_at_unix_ms: chrono::Utc::now().timestamp_millis(),
            completed_at_unix_ms: None,
            retain_until_unix_ms: None,
            lookup_url: format!(
                "operations/{}?instance={}",
                input.request_id, input.expected_server_instance_id
            ),
            result: None,
        };
        registry.insert(
            key.clone(),
            StoredOperation {
                input,
                report: report.clone(),
                completed: None,
            },
        );
        drop(registry);
        tracing::info!(action = "reload", credential_id, request_id = %report.request_id,
            instance = %report.server_instance_id, generation = report.generation_before, "control operation admitted");
        let service = self.clone();
        tokio::spawn(async move {
            let instance = reservation.server_instance_id().to_owned();
            let generation = reservation.generation();
            let result =
                std::panic::AssertUnwindSafe(service.reloader.reload_reserved(reservation))
                    .catch_unwind()
                    .await
                    .ok()
                    .or_else(|| {
                        service
                            .reloader
                            .reload_state()
                            .and_then(|state| state.last_outcome)
                            .filter(|report| {
                                report.server_instance_id == instance
                                    && report.generation == generation
                            })
                    });
            let status = match result.as_ref().map(|report| report.outcome) {
                Some(ReloadOutcome::Succeeded) => OperationStatus::Succeeded,
                Some(ReloadOutcome::Failed) => OperationStatus::Failed,
                Some(ReloadOutcome::PartiallyApplied) => OperationStatus::PartiallyApplied,
                Some(ReloadOutcome::Unknown) | None => OperationStatus::Unknown,
            };
            let now = chrono::Utc::now().timestamp_millis();
            let mut registry = service.registry.lock().await;
            if let Some(operation) = registry.get_mut(&key) {
                operation.report.status = status;
                operation.report.completed_at_unix_ms = Some(now);
                operation.report.retain_until_unix_ms =
                    Some(now + (RETENTION_SECONDS as i64 * 1_000));
                operation.report.result = result;
                operation.completed = Some(Instant::now());
                tracing::info!(action = "reload", credential_id = %key.0, request_id = %key.1,
                    instance = %operation.report.server_instance_id, generation,
                    participants = ?operation.report.result.as_ref().map(|report| &report.participants),
                    restart_required_fields = ?operation.report.result.as_ref().map(|report| &report.restart_required_fields),
                    outcome = ?status, duration_ms = now.saturating_sub(operation.report.accepted_at_unix_ms),
                    "control operation completed");
            }
        });
        Ok(report)
    }

    pub async fn lookup(
        &self,
        credential_id: &str,
        request_id: &str,
        instance: &str,
    ) -> Result<OperationReport, OperationError> {
        if request_id.len() > 256 || instance.len() > 256 {
            return Err(OperationError::InvalidRequest);
        }
        let instance = uuid::Uuid::parse_str(instance)
            .map_err(|_| OperationError::InvalidRequest)?
            .to_string();
        let request_id = uuid::Uuid::parse_str(request_id)
            .map_err(|_| OperationError::InvalidRequest)?
            .to_string();
        if self.state()?.server_instance_id != instance {
            return Err(OperationError::ServerInstanceChanged);
        }
        let registry = self.registry.lock().await;
        let operation = registry
            .get(&(credential_id.to_owned(), request_id))
            .ok_or(OperationError::NotFound)?;
        if operation.expired() {
            return Err(OperationError::Expired);
        }
        Ok(operation.report.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reload::{ReloadCoordinator, ReloadReservation, ReloadState};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestReloader {
        coordinator: Arc<ReloadCoordinator>,
        executions: AtomicUsize,
        release: tokio::sync::Semaphore,
    }

    #[async_trait::async_trait]
    impl DaemonReloader for TestReloader {
        async fn reload(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn reload_state(&self) -> Option<ReloadState> {
            Some(self.coordinator.state())
        }
        fn reserve_remote(
            &self,
            instance: &str,
            generation: u64,
        ) -> Result<ReloadReservation, ReloadAdmissionError> {
            self.coordinator.reserve_remote(instance, generation)
        }
        async fn reload_reserved(&self, reservation: ReloadReservation) -> ReloadReport {
            self.executions.fetch_add(1, Ordering::SeqCst);
            if let Ok(permit) = self.release.acquire().await {
                permit.forget();
            }
            let report = ReloadReport::succeeded(
                reservation.server_instance_id().into(),
                reservation.generation(),
            );
            self.coordinator.complete(&reservation, report.clone());
            report
        }
    }

    fn fixture() -> (Arc<TestReloader>, Arc<OperationService>, ReloadInput) {
        let reloader = Arc::new(TestReloader {
            coordinator: ReloadCoordinator::new(),
            executions: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
        });
        let input = ReloadInput {
            request_id: uuid::Uuid::new_v4().to_string(),
            expected_server_instance_id: reloader.coordinator.state().server_instance_id,
            expected_generation: 0,
        };
        let service = Arc::new(OperationService::new(reloader.clone()));
        (reloader, service, input)
    }

    async fn completed(
        service: &OperationService,
        input: &ReloadInput,
    ) -> anyhow::Result<OperationReport> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let report = service
                    .lookup(
                        "operator",
                        &input.request_id,
                        &input.expected_server_instance_id,
                    )
                    .await?;
                if !report.is_running() {
                    return Ok(report);
                }
                tokio::task::yield_now().await;
            }
        })
        .await?
    }

    #[tokio::test]
    async fn duplicate_owner_and_disconnect_preserve_one_execution() -> anyhow::Result<()> {
        let (reloader, service, input) = fixture();
        let first = service.submit("operator", input.clone()).await?;
        assert!(first.is_running());
        let repeated = service.submit("operator", input.clone()).await?;
        assert_eq!(first.request_id, repeated.request_id);
        let mut changed = input.clone();
        changed.expected_generation = 1;
        assert!(matches!(
            service.submit("operator", changed).await,
            Err(OperationError::IdempotencyConflict)
        ));
        assert!(matches!(
            service
                .lookup(
                    "reader",
                    &input.request_id,
                    &input.expected_server_instance_id
                )
                .await,
            Err(OperationError::NotFound)
        ));
        assert!(matches!(
            service.submit("other", input.clone()).await,
            Err(OperationError::ReloadInProgress)
        ));
        // No HTTP request or response owns this work after submit returns.
        drop(first);
        reloader.release.add_permits(1);
        let result = completed(&service, &input).await?;
        assert!(result.succeeded());
        assert_eq!(reloader.executions.load(Ordering::SeqCst), 1);
        assert_eq!(service.state()?.generation, 1);
        assert!(result.retain_until_unix_ms > result.completed_at_unix_ms);
        assert!(service.submit("operator", input.clone()).await?.succeeded());
        let mut fresh = input.clone();
        fresh.request_id = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            service.submit("operator", fresh).await,
            Err(OperationError::StaleGeneration)
        ));
        assert!(matches!(
            service
                .lookup(
                    "operator",
                    &input.request_id,
                    &uuid::Uuid::new_v4().to_string()
                )
                .await,
            Err(OperationError::ServerInstanceChanged)
        ));
        assert!(matches!(
            service
                .lookup("operator", &input.request_id, "not-a-uuid")
                .await,
            Err(OperationError::InvalidRequest)
        ));
        assert!(matches!(
            service
                .lookup("operator", &input.request_id, &"x".repeat(257))
                .await,
            Err(OperationError::InvalidRequest)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn capacity_never_evicts_unexpired_and_expiry_never_reexecutes() -> anyhow::Result<()> {
        let (reloader, service, input) = fixture();
        service.submit("operator", input.clone()).await?;
        reloader.release.add_permits(1);
        let report = completed(&service, &input).await?;
        {
            let mut registry = service.registry.lock().await;
            for index in 1..MAX_OPERATIONS {
                registry.insert(
                    ("operator".into(), format!("retained-{index}")),
                    StoredOperation {
                        input: input.clone(),
                        report: report.clone(),
                        completed: Some(Instant::now()),
                    },
                );
            }
        }
        let mut fresh = input.clone();
        fresh.request_id = uuid::Uuid::new_v4().to_string();
        fresh.expected_generation = 1;
        assert!(matches!(
            service.submit("operator", fresh.clone()).await,
            Err(OperationError::Capacity)
        ));
        assert_eq!(reloader.executions.load(Ordering::SeqCst), 1);
        {
            let mut registry = service.registry.lock().await;
            let stored = registry
                .get_mut(&("operator".into(), input.request_id.clone()))
                .ok_or_else(|| anyhow::anyhow!("missing retained operation"))?;
            stored.completed = Some(Instant::now() - Duration::from_secs(RETENTION_SECONDS + 1));
        }
        assert!(matches!(
            service
                .lookup(
                    "operator",
                    &input.request_id,
                    &input.expected_server_instance_id
                )
                .await,
            Err(OperationError::Expired)
        ));
        assert!(matches!(
            service.submit("operator", input.clone()).await,
            Err(OperationError::Expired)
        ));
        service.submit("operator", fresh.clone()).await?;
        reloader.release.add_permits(1);
        assert!(completed(&service, &fresh).await?.succeeded());
        Ok(())
    }
}
