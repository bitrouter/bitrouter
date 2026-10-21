//! Live owners of the existing native managed harness. Durable authority stays
//! in DatabaseNativeStore/Core; only pre-admission failures have a boot cache.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, OnceLock};

use bitrouter_orchestrator::agent::ToolMode;
use bitrouter_orchestrator::core::checkpoint::sha256;
use bitrouter_orchestrator::core::protocol::{
    CoreError, ErrorCode, OwnershipGrant, TaskInput, Verification,
};
use bitrouter_orchestrator::core::session::CoreSession;
use bitrouter_orchestrator::core::skills::SkillConsentAnswer;
use bitrouter_orchestrator::harness::HarnessConfig;
use bitrouter_orchestrator::harness::managed::session::{
    NativeApproval, NativeSession, StoredSession,
};
use bitrouter_orchestrator::harness::managed::store::NativeStore;
use bitrouter_sdk::{App, caller::CallerContext, config::Config};
use sea_orm::DatabaseConnection;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::protocol::{Report, Submit};

pub(super) const MAX_ACTIVE: usize = 4;
const MAX_FAILURES: usize = 128;

fn error(code: ErrorCode, message: impl Into<String>) -> CoreError {
    CoreError::rejected(code, message)
}

struct HeadlessApproval;

#[async_trait::async_trait]
impl NativeApproval for HeadlessApproval {
    async fn approve(
        &self,
        _: &bitrouter_orchestrator::core::protocol::ToolExecute,
    ) -> Result<bool, CoreError> {
        // Preserve this explicit headless command's original permission policy.
        // NativeResources still enforce workspace, mode and declared tools.
        Ok(true)
    }

    async fn approve_skill(
        &self,
        _: &bitrouter_orchestrator::core::skills::SkillConsentRequest,
    ) -> Result<bool, CoreError> {
        // The original Core request remains pending until an exact operator
        // answer or cancellation. Headless command policy grants no consent.
        std::future::pending().await
    }
}

struct Job {
    request: Submit,
    input: TaskInput,
    core: OnceLock<(CoreSession, OwnershipGrant)>,
    cancel: CancellationToken,
}

struct Registry {
    accepting: bool,
    live: BTreeMap<String, Arc<Job>>,
    failures: VecDeque<(Submit, CoreError)>,
    drivers: JoinSet<()>,
}

struct Shared {
    app: Arc<App>,
    db: DatabaseConnection,
    config: Config,
    resources: HarnessConfig,
    registry: Mutex<Registry>,
}

#[derive(Clone)]
pub(crate) struct Owner {
    shared: Arc<Shared>,
}

impl Owner {
    pub fn new(
        app: Arc<App>,
        db: DatabaseConnection,
        config: Config,
        resources: HarnessConfig,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                app,
                db,
                config,
                resources,
                registry: Mutex::new(Registry {
                    accepting: true,
                    live: BTreeMap::new(),
                    failures: VecDeque::new(),
                    drivers: JoinSet::new(),
                }),
            }),
        }
    }

    fn store(&self, name: &str) -> Result<Arc<dyn NativeStore>, CoreError> {
        crate::managed_store::DatabaseNativeStore::new(self.shared.db.clone(), name.into())
            .map(|store| Arc::new(store) as Arc<dyn NativeStore>)
            .map_err(|e| error(ErrorCode::UnsupportedCapability, e.to_string()))
    }

    fn input(&self, request: &Submit) -> Result<TaskInput, CoreError> {
        if let Some(skills) = &request.skills {
            skills.validate()?;
        }
        crate::control_output::validate_id(&request.operation_id)
            .map_err(|e| error(ErrorCode::UnsupportedCapability, e.to_string()))?;
        if request.prompt.trim().is_empty()
            || request.max_output_tokens == Some(0)
            || (request.read_only && request.check.is_some())
        {
            return Err(error(
                ErrorCode::UnsupportedCapability,
                "invalid prompt/output reservation or read-only verification",
            ));
        }
        let model = request
            .model
            .clone()
            .or_else(|| {
                crate::policy_lock::native_default_model(&self.shared.config).map(str::to_owned)
            })
            .ok_or_else(|| {
                error(
                    ErrorCode::UnsupportedCapability,
                    "no native model configured; pass --model or enable defaults",
                )
            })?;
        let model_mode = crate::policy_lock::native_model_mode(&self.shared.config, &model)
            .map_err(|e| error(ErrorCode::UnsupportedCapability, e.to_string()))?;
        Ok(TaskInput {
            text: request.prompt.clone(),
            model,
            effort: request.effort.map(|e| e.to_string()),
            max_output_tokens: request.max_output_tokens,
            context_limit_bytes: None,
            routing: bitrouter_orchestrator::core::protocol::RoutingSettings {
                model: model_mode,
                skills: request.skills.clone(),
                ..Default::default()
            },
            max_concurrent_subagents: None,
            discardable_history: None,
            acceptance_criteria: vec![],
            required_materials: vec![],
            limits: None,
            verification: request.check.as_ref().map(|command| Verification {
                tool: "shell".into(),
                arguments: serde_json::json!({"command":command}),
            }),
        })
    }

    pub async fn submit(&self, mut request: Submit) -> Result<Report, CoreError> {
        let store = self.store(&request.name)?;
        let workspace = request
            .workspace
            .canonicalize()
            .map_err(|e| error(ErrorCode::UnsupportedCapability, format!("workspace: {e}")))?;
        if !request.workspace.is_absolute() || workspace != request.workspace || !workspace.is_dir()
        {
            return Err(error(
                ErrorCode::UnsupportedCapability,
                "workspace must be an exact absolute canonical directory",
            ));
        }
        request.workspace = workspace;
        let input = self.input(&request)?;
        let mut registry = self.shared.registry.lock().await;
        while registry.drivers.try_join_next().is_some() {}
        if !registry.accepting {
            return Err(error(
                ErrorCode::RecoveryRequired,
                "managed admission is shutting down",
            ));
        }
        if let Some(job) = registry.live.get(&request.name).cloned() {
            if job.request != request {
                return Err(error(
                    if job.request.operation_id == request.operation_id {
                        ErrorCode::OperationConflict
                    } else {
                        ErrorCode::Busy
                    },
                    "managed session already owns a different request; inspect its original operation",
                ));
            }
            drop(registry);
            return self.live_report(&job, None).await;
        }
        // Serialize same-name admission with the live registry. This read never
        // opens resources or restores a core, including after daemon restart.
        let stored = NativeSession::inspect(store.as_ref()).await?;
        if let Some(stored) = stored.as_ref() {
            let mode = if request.read_only {
                ToolMode::ReadOnly
            } else {
                ToolMode::Coding
            };
            if stored.workspace != request.workspace || stored.mode != mode {
                return Err(error(
                    ErrorCode::RecoveryRequired,
                    "managed session workspace/mode differs from its original journal",
                ));
            }
            if let Some(receipt) = stored
                .snapshot
                .as_ref()
                .and_then(|s| s.operations.get(&request.operation_id))
            {
                let revision = receipt.state_revision.checked_sub(1).ok_or_else(|| {
                    error(
                        ErrorCode::CheckpointUnavailable,
                        "invalid retained input receipt",
                    )
                })?;
                let digest = sha256(
                    &serde_json::to_vec(&serde_json::json!({
                        "type":"start", "expected_state_revision":revision, "input":input,
                    }))
                    .map_err(|e| error(ErrorCode::UnsupportedCapability, e.to_string()))?,
                );
                if digest != receipt.request_sha256 {
                    return Err(error(
                        ErrorCode::OperationConflict,
                        "operation ID already accepted different input",
                    ));
                }
                return Ok(stored_report(
                    &request.name,
                    Some(&request.operation_id),
                    stored,
                ));
            }
            if !stored.released {
                return Err(error(
                    ErrorCode::RecoveryRequired,
                    "managed session has no confirmed release; lost/unknown work cannot be taken over",
                ));
            }
        }
        if let Some((previous, failure)) = registry
            .failures
            .iter()
            .find(|(r, _)| r.name == request.name && r.operation_id == request.operation_id)
        {
            if previous != &request {
                return Err(error(
                    ErrorCode::OperationConflict,
                    "failed request identity cannot change input",
                ));
            }
            return Err(failure.clone());
        }
        if registry.live.len() >= MAX_ACTIVE {
            return Err(error(
                ErrorCode::Busy,
                "managed native session capacity reached",
            ));
        }
        let job = Arc::new(Job {
            request,
            input,
            core: OnceLock::new(),
            cancel: CancellationToken::new(),
        });
        registry.live.insert(job.request.name.clone(), job.clone());
        let owner = self.clone();
        let worker = job.clone();
        registry.drivers.spawn(async move {
            let outcome = owner.execute(&worker, store).await;
            let mut registry = owner.shared.registry.lock().await;
            if let Err(failure) = outcome {
                if registry.failures.len() == MAX_FAILURES {
                    registry.failures.pop_front();
                }
                registry
                    .failures
                    .push_back((worker.request.clone(), failure));
            }
            registry.live.remove(&worker.request.name);
        });
        drop(registry);
        self.live_report(&job, None).await
    }

    async fn execute(&self, job: &Job, store: Arc<dyn NativeStore>) -> Result<(), CoreError> {
        let mode = if job.request.read_only {
            ToolMode::ReadOnly
        } else {
            ToolMode::Coding
        };
        let mut session = NativeSession::open_cancellable(
            self.shared.app.clone(),
            CallerContext::local(),
            store.clone(),
            &job.request.workspace,
            mode,
            &self.shared.resources,
            &job.cancel,
        )
        .await?;
        let outcome = async {
            let stored = NativeSession::inspect(store.as_ref())
                .await?
                .ok_or_else(|| {
                    error(
                        ErrorCode::CheckpointUnavailable,
                        "opened native session has no journal",
                    )
                })?;
            job.core
                .set((session.core().clone(), stored.grant))
                .map_err(|_| {
                    error(
                        ErrorCode::OperationConflict,
                        "managed owner already initialized",
                    )
                })?;
            session
                .start(&job.request.operation_id, job.input.clone())
                .await?;
            session
                .drive(Arc::new(HeadlessApproval), job.cancel.clone())
                .await
                .map(|_| ())
        }
        .await;
        // close performs the original release/ACK and workspace-fence cleanup.
        // A failure does not turn an unreleased journal into a reusable owner.
        let released = session.close().await;
        match (outcome, released) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Err(execution), Err(mut release)) => {
                release.message = format!(
                    "execution: {}; release: {}",
                    execution.message, release.message
                );
                Err(release)
            }
            (Ok(()), Err(error)) => Err(error),
        }
    }

    pub async fn status(
        &self,
        name: &str,
        operation_id: Option<&str>,
    ) -> Result<Report, CoreError> {
        let store = self.store(name)?;
        let registry = self.shared.registry.lock().await;
        if let Some(job) = registry.live.get(name).cloned() {
            drop(registry);
            return self.live_report(&job, operation_id).await;
        }
        let failure = registry
            .failures
            .iter()
            .rev()
            .find(|(r, _)| r.name == name && operation_id.is_some_and(|id| id == r.operation_id))
            .cloned();
        // Prevent a new native owner from opening this row between the passive
        // metadata/payload reads. Live owners are projected directly from Core.
        let stored = NativeSession::inspect(store.as_ref()).await?;
        drop(registry);
        let mut report = match stored {
            Some(stored) => stored_report(name, operation_id, &stored),
            None => Report::empty(name.into(), operation_id.map(str::to_owned), "not_found"),
        };
        if let Some((request, error)) = failure {
            report.operation_id = Some(request.operation_id);
            report.error = Some(error);
            report.state = "failed".into();
        }
        Ok(report)
    }

    pub async fn cancel(&self, name: &str, operation_id: &str) -> Result<Report, CoreError> {
        crate::control_output::validate_id(operation_id)
            .map_err(|e| error(ErrorCode::UnsupportedCapability, e.to_string()))?;
        let registry = self.shared.registry.lock().await;
        if let Some(job) = registry.live.get(name) {
            if job.request.operation_id != operation_id {
                return Err(error(
                    ErrorCode::OperationConflict,
                    "cancel does not name the active managed operation",
                ));
            }
            job.cancel.cancel();
        }
        drop(registry);
        let report = self.status(name, Some(operation_id)).await?;
        if !report.resident && report.receipt.is_none() {
            return Err(error(
                ErrorCode::OperationConflict,
                "cancel names no active or accepted managed input",
            ));
        }
        Ok(report)
    }

    pub async fn answer_skill(
        &self,
        name: &str,
        operation_id: &str,
        answer_operation_id: &str,
        answer: SkillConsentAnswer,
    ) -> Result<Report, CoreError> {
        for id in [operation_id, answer_operation_id] {
            crate::control_output::validate_id(id)
                .map_err(|e| error(ErrorCode::OperationConflict, e.to_string()))?;
        }
        let store = self.store(name)?;
        let registry = self.shared.registry.lock().await;
        let receipt = if let Some(job) = registry.live.get(name) {
            let (core, _) = job
                .core
                .get()
                .ok_or_else(|| error(ErrorCode::Busy, "managed owner is preparing"))?;
            let snapshot = core.snapshot().await;
            ensure_skill_input(&snapshot, operation_id, &answer)?;
            if job.request.operation_id == operation_id {
                core.answer_skill_consent(answer_operation_id, answer)
                    .await?
            } else {
                retained_skill_answer(&snapshot, answer_operation_id, &answer)?
            }
        } else {
            // Inspecting a lost answer ACK must never restore an owner, read
            // MCP content, or accept a fresh answer on an unreleased journal.
            let stored = NativeSession::inspect(store.as_ref())
                .await?
                .ok_or_else(|| error(ErrorCode::OperationConflict, "managed session is absent"))?;
            let snapshot = stored.snapshot.as_ref().ok_or_else(|| {
                error(ErrorCode::RecoveryRequired, "managed checkpoint is absent")
            })?;
            ensure_skill_input(snapshot, operation_id, &answer)?;
            retained_skill_answer(snapshot, answer_operation_id, &answer)?
        };
        drop(registry);
        let mut report = self.status(name, Some(operation_id)).await?;
        report.answer_receipt = Some(receipt);
        Ok(report)
    }

    async fn live_report(&self, job: &Job, selected: Option<&str>) -> Result<Report, CoreError> {
        let operation_id = selected.unwrap_or(&job.request.operation_id);
        let current = operation_id == job.request.operation_id;
        let Some((core, grant)) = job.core.get() else {
            let mut report = Report::empty(
                job.request.name.clone(),
                Some(operation_id.into()),
                if current { "preparing" } else { "not_found" },
            );
            report.workspace = Some(job.request.workspace.clone());
            report.resident = current;
            return Ok(report);
        };
        // Core publishes these facts only through its original checkpoint/ACK
        // barrier. Do not race database row replacement on every observer poll.
        let snapshot = core.snapshot().await;
        let receipt = snapshot.operations.get(operation_id).cloned();
        let skill_consents = consent_projection(&snapshot, receipt.as_ref());
        let run = snapshot.run.filter(|run| {
            receipt.as_ref().and_then(|r| r.assigned_ids.get("run_id")) == Some(&run.run_id)
        });
        Ok(Report {
            name: job.request.name.clone(),
            operation_id: Some(operation_id.into()),
            state: if current {
                if receipt.is_some() {
                    "running"
                } else {
                    "preparing"
                }
            } else if receipt.is_some() {
                "accepted_historical"
            } else {
                "not_found"
            }
            .into(),
            workspace: Some(job.request.workspace.clone()),
            mode: Some(if job.request.read_only {
                ToolMode::ReadOnly
            } else {
                ToolMode::Coding
            }),
            grant: Some(grant.clone()),
            head: Some(core.head().await),
            receipt,
            run,
            agents: snapshot.agents.len(),
            resident: current,
            released: false,
            error: None,
            skill_consents,
            answer_receipt: None,
        })
    }

    pub async fn active_sessions(&self) -> Result<Vec<Report>, CoreError> {
        let jobs = self
            .shared
            .registry
            .lock()
            .await
            .live
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut reports = Vec::new();
        for job in jobs {
            if job.core.get().is_some() {
                reports.push(self.live_report(&job, None).await?);
            }
        }
        Ok(reports)
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let mut registry = self.shared.registry.lock().await;
        registry.accepting = false;
        for job in registry.live.values() {
            job.cancel.cancel();
        }
        let mut drivers = std::mem::take(&mut registry.drivers);
        drop(registry);
        while let Some(result) = drivers.join_next().await {
            result?;
        }
        Ok(())
    }
}

fn stored_report(name: &str, operation_id: Option<&str>, stored: &StoredSession) -> Report {
    let receipt = operation_id
        .and_then(|id| stored.snapshot.as_ref()?.operations.get(id))
        .cloned();
    let run = stored
        .snapshot
        .as_ref()
        .and_then(|s| s.run.as_ref())
        .filter(|run| {
            operation_id.is_none()
                || receipt.as_ref().and_then(|r| r.assigned_ids.get("run_id")) == Some(&run.run_id)
        })
        .cloned();
    let state = if operation_id.is_some() && receipt.is_none() {
        "not_found"
    } else if !stored.released {
        "recovery_required"
    } else if operation_id.is_some() && run.is_none() {
        "accepted_historical"
    } else {
        "released"
    };
    Report {
        name: name.into(),
        operation_id: operation_id.map(str::to_owned),
        state: state.into(),
        workspace: Some(stored.workspace.clone()),
        mode: Some(stored.mode),
        grant: Some(stored.grant.clone()),
        head: Some(stored.head.clone()),
        skill_consents: stored.snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
            if operation_id.is_none() {
                snapshot
                    .run
                    .as_ref()
                    .map_or_else(Vec::new, |run| snapshot.skill_consent_requests(&run.run_id))
            } else {
                consent_projection(snapshot, receipt.as_ref())
            }
        }),
        answer_receipt: None,
        receipt,
        run,
        agents: stored.snapshot.as_ref().map_or(0, |s| s.agents.len()),
        resident: false,
        released: stored.released,
        error: None,
    }
}

fn consent_projection(
    snapshot: &bitrouter_orchestrator::core::session::SessionSnapshot,
    receipt: Option<&bitrouter_orchestrator::core::protocol::OperationReceipt>,
) -> Vec<bitrouter_orchestrator::core::skills::SkillConsentRequest> {
    receipt
        .and_then(|receipt| receipt.assigned_ids.get("run_id"))
        .map_or_else(Vec::new, |run_id| snapshot.skill_consent_requests(run_id))
}

fn ensure_skill_input(
    snapshot: &bitrouter_orchestrator::core::session::SessionSnapshot,
    operation_id: &str,
    answer: &SkillConsentAnswer,
) -> Result<(), CoreError> {
    if snapshot.session_id != answer.session_id
        || snapshot
            .operations
            .get(operation_id)
            .and_then(|receipt| receipt.assigned_ids.get("run_id"))
            != Some(&answer.run_id)
    {
        return Err(error(
            ErrorCode::OperationConflict,
            "skill answer names another accepted managed input",
        ));
    }
    Ok(())
}

fn retained_skill_answer(
    snapshot: &bitrouter_orchestrator::core::session::SessionSnapshot,
    operation_id: &str,
    answer: &SkillConsentAnswer,
) -> Result<bitrouter_orchestrator::core::protocol::OperationReceipt, CoreError> {
    let receipt = snapshot.operations.get(operation_id).ok_or_else(|| error(ErrorCode::RecoveryRequired, "no live owner can accept a new skill answer; only an original committed answer may be inspected"))?;
    if receipt.request_sha256 != answer.fingerprint()? {
        return Err(error(
            ErrorCode::OperationConflict,
            "skill answer operation already names different input",
        ));
    }
    Ok(receipt.clone())
}
