//! Managed model work has execution identity, bounded control and no file tools.
#[path = "generation/routing.rs"]
mod routing;
#[path = "generation/waiting.rs"]
mod waiting;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bitrouter_ai::types::{GenerationParams, Message, Prompt, Role};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::executor::{MockExecutor, MockResponse};
use bitrouter_sdk::language_model::native::{
    NativeAttemptReport, NativeExecutionControl, NativePlan, NativePlanAdmission,
};

use super::support::{app_with_executor, final_turn, input, target, tool_call, turn};
use crate::control::WorkControl;
use crate::service::generation::GenerationRequest;
use crate::service::{ErrorCode, ThreadService};
use crate::store::{ExecutionRecord, ExecutionStore, MemoryExecutionStore};
use crate::thread::{PermissionProfile, ThreadSnapshot};
use crate::turn::{GenerationOrigin, GenerationPurpose, TurnStatus};

struct Control {
    acknowledged: bool,
    attempts: AtomicUsize,
}
#[async_trait::async_trait]
impl NativeExecutionControl for Control {
    async fn plan(&self, plan: NativePlan) -> bitrouter_sdk::Result<NativePlanAdmission> {
        Ok(NativePlanAdmission {
            route_indices: (0..plan.routes.len())
                .map(u32::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    bitrouter_sdk::error::BitrouterError::internal(error.to_string())
                })?,
        })
    }
    async fn before_attempt(&self, _: &str, _: u32) -> bitrouter_sdk::Result<()> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn after_attempt(&self, _: NativeAttemptReport) {}
}
impl WorkControl for Control {
    fn outcome_acknowledged(&self) -> bool {
        self.acknowledged
    }
}

struct HeldAdmission {
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Semaphore,
}
#[async_trait::async_trait]
impl NativeExecutionControl for HeldAdmission {
    async fn plan(&self, plan: NativePlan) -> bitrouter_sdk::Result<NativePlanAdmission> {
        Control {
            acknowledged: true,
            attempts: AtomicUsize::new(0),
        }
        .plan(plan)
        .await
    }
    async fn before_attempt(&self, _: &str, _: u32) -> bitrouter_sdk::Result<()> {
        self.entered
            .send(())
            .map_err(|error| bitrouter_sdk::error::BitrouterError::internal(error.to_string()))?;
        self.release
            .acquire()
            .await
            .map_err(|error| bitrouter_sdk::error::BitrouterError::internal(error.to_string()))?
            .forget();
        Ok(())
    }
    async fn after_attempt(&self, _: NativeAttemptReport) {}
}
impl WorkControl for HeldAdmission {
    fn outcome_acknowledged(&self) -> bool {
        true
    }
}

struct HeldExecutor {
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Semaphore,
    inner: MockExecutor,
}
#[async_trait::async_trait]
impl bitrouter_sdk::language_model::Executor for HeldExecutor {
    async fn execute(
        &self,
        target: &bitrouter_sdk::language_model::RoutingTarget,
        prompt: &Prompt,
        context: &bitrouter_sdk::language_model::PipelineContext,
    ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::ExecutionResult> {
        self.entered
            .send(())
            .map_err(|error| bitrouter_sdk::error::BitrouterError::internal(error.to_string()))?;
        self.release
            .acquire()
            .await
            .map_err(|error| bitrouter_sdk::error::BitrouterError::internal(error.to_string()))?
            .forget();
        bitrouter_sdk::language_model::Executor::execute(&self.inner, target, prompt, context).await
    }
    async fn execute_stream(
        &self,
        _: &bitrouter_sdk::language_model::RoutingTarget,
        _: &Prompt,
        _: &bitrouter_sdk::language_model::PipelineContext,
    ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::StreamPartStream> {
        Err(bitrouter_sdk::error::BitrouterError::internal(
            "generation must not open a streaming tool loop",
        ))
    }
}

fn request(purpose: GenerationPurpose, source: &str) -> GenerationRequest {
    GenerationRequest {
        origin: GenerationOrigin {
            goal_id: "goal".into(),
            source_id: source.into(),
            requirements_revision: 1,
            purpose,
        },
        prompt: Prompt {
            model: "fixture-model".into(),
            system: Some("Return a factual JSON result".into()),
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "{}")],
            tools: Vec::new(),
            params: GenerationParams {
                max_tokens: Some(128),
                ..Default::default()
            },
            response_format: None,
            tool_choice: None,
            stream: false,
        },
    }
}

async fn owner(
    service: &ThreadService,
    path: &std::path::Path,
) -> Result<ThreadSnapshot, Box<dyn std::error::Error>> {
    service
        .create_goal_owner_thread(
            &service.capabilities().server_instance_id,
            "goal",
            crate::thread::ThreadRequest {
                caller: CallerContext::local(),
                workspace: path.into(),
                config: crate::agent::AgentConfig::fixed("fixture-model", None).read_only(),
                permission_profile: PermissionProfile::ReadOnly,
                verification_command: None,
                idempotency_key: crate::goal::workflow::owner_thread_key("goal"),
            },
        )
        .await
        .map_err(Into::into)
}

async fn finished(
    service: &ThreadService,
    id: &str,
) -> Result<crate::turn::TurnSnapshot, Box<dyn std::error::Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = service.read(id)?;
            if snapshot.status.terminal() || snapshot.status == TurnStatus::RecoveryRequired {
                return Ok(snapshot);
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

#[tokio::test]
async fn generation_replay_cold_reads_and_direct_native_rpcs_preserve_goal_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("removed-project");
    let executor = Arc::new(MockExecutor::new(vec![
        MockResponse::Generate(final_turn()),
    ]));
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(app_with_executor(executor)?, &[], store.clone())?;
    let owner = owner(&service, &workspace).await?;
    assert!(!workspace.exists());
    let target = target(&owner);
    let caller = CallerContext::local();
    assert_eq!(
        service
            .start_turn(
                &target,
                &caller,
                input("bypass Goal controls", "native-bypass")
            )
            .await
            .err()
            .ok_or("ordinary RPC started Goal work")?
            .code,
        ErrorCode::Unauthorized
    );
    let control = Arc::new(Control {
        acknowledged: true,
        attempts: AtomicUsize::new(0),
    });
    let original = request(GenerationPurpose::Planning, "plan-request");
    let receipt = service
        .start_goal_generation(&target, &caller, original, control.clone())
        .await?;
    let snapshot = finished(&service, &receipt.turn_id).await?;
    assert_eq!(
        snapshot.status,
        TurnStatus::Completed,
        "{:?}",
        snapshot.detail
    );
    assert!(
        snapshot
            .generation
            .as_ref()
            .and_then(|progress| progress.request_id.as_ref())
            .is_some()
    );
    let same = service
        .start_goal_generation(
            &target,
            &caller,
            request(GenerationPurpose::Planning, "plan-request"),
            control.clone(),
        )
        .await?;
    assert_eq!(same.turn_id, receipt.turn_id);
    assert_eq!(control.attempts.load(Ordering::SeqCst), 1);
    let mut changed = request(GenerationPurpose::Planning, "plan-request");
    changed.prompt.messages = vec![Message::text(Role::User, "different input")];
    assert_eq!(
        service
            .start_goal_generation(&target, &caller, changed, control)
            .await
            .err()
            .ok_or("different prompt reused an action key")?
            .code,
        ErrorCode::Conflict
    );
    let records = store
        .load(&owner.thread_id)
        .await?
        .ok_or("missing execution root")?;
    assert!(records.records.iter().any(
        |record| matches!(record, ExecutionRecord::TurnRecord { fact, .. }
        if matches!(fact.as_ref(), ExecutionRecord::GenerationPlan { .. }))
    ));
    assert!(records.records.iter().any(
        |record| matches!(record, ExecutionRecord::TurnRecord { fact, .. }
        if matches!(fact.as_ref(), ExecutionRecord::GenerationAttempt { .. }))
    ));
    assert!(service.lock_state().active_workspaces.is_empty());
    tokio::time::timeout(Duration::from_secs(3), async {
        while service
            .lock_state()
            .running_turns
            .contains_key(&receipt.turn_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    service.unload_thread(&target, &caller).await?;
    let cold = service
        .read_stored_turn(&target, &caller, &receipt.turn_id)
        .await?;
    assert_eq!(cold.final_answer, snapshot.final_answer);
    assert_eq!(
        cold.generation.and_then(|progress| progress.request_id),
        snapshot.generation.and_then(|progress| progress.request_id)
    );
    assert!(!service.lock_state().threads.contains_key(&owner.thread_id));
    assert!(
        service
            .find_generation_admission(
                &target,
                &caller,
                &request(GenerationPurpose::Planning, "plan-request").origin
            )
            .await?
            .is_some()
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unacknowledged_generation_outcome_never_becomes_a_completed_turn()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let service = ThreadService::new(
        app_with_executor(Arc::new(MockExecutor::new(vec![MockResponse::Generate(
            final_turn(),
        )])))?,
        &[],
    )?;
    let owner = owner(&service, directory.path()).await?;
    let receipt = service
        .start_goal_generation(
            &target(&owner),
            &CallerContext::local(),
            request(GenerationPurpose::Planning, "unacknowledged"),
            Arc::new(Control {
                acknowledged: false,
                attempts: AtomicUsize::new(0),
            }),
        )
        .await?;
    let result = finished(&service, &receipt.turn_id).await?;
    assert_eq!(result.status, TurnStatus::Failed);
    assert!(result.final_answer.is_none());
    assert!(
        result
            .detail
            .is_some_and(|detail| detail.contains("acknowledged"))
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn input_generation_overlaps_owner_work_without_bypassing_thread_or_global_capacity()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let (entered, mut arrivals) = tokio::sync::mpsc::unbounded_channel();
    let executor = Arc::new(HeldExecutor {
        entered,
        release: tokio::sync::Semaphore::new(0),
        inner: MockExecutor::new(vec![
            MockResponse::Generate(final_turn()),
            MockResponse::Generate(final_turn()),
        ]),
    });
    let service = ThreadService::with_limits(
        app_with_executor(executor.clone())?,
        &[],
        crate::service::RuntimeLimits {
            active_turns: 2,
            ..Default::default()
        },
    )?;
    let owner = owner(&service, directory.path()).await?;
    let owner_target = target(&owner);
    let view = service
        .read_stored_thread_view(&owner_target, &CallerContext::local())
        .await?;
    let input_thread = service
        .create_goal_input_thread(
            &owner_target,
            crate::thread::ThreadRequest {
                caller: CallerContext::local(),
                workspace: directory.path().into(),
                config: view.config,
                permission_profile: PermissionProfile::ReadOnly,
                verification_command: None,
                idempotency_key: crate::goal::workflow::input_thread_key("goal"),
            },
        )
        .await?;
    let control = Arc::new(Control {
        acknowledged: true,
        attempts: AtomicUsize::new(0),
    });
    let root = service
        .start_goal_generation(
            &owner_target,
            &CallerContext::local(),
            request(GenerationPurpose::Planning, "busy-owner"),
            control.clone(),
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(3), arrivals.recv())
        .await?
        .ok_or("owner request did not reach execution")?;
    assert_eq!(
        service
            .steer(
                &owner_target,
                &CallerContext::local(),
                crate::turn::SteeringRequest {
                    expected_turn_id: root.turn_id.clone(),
                    text: "change the Goal without its input operation".into(),
                    idempotency_key: "native-steering-bypass".into(),
                }
            )
            .await
            .err()
            .ok_or("native steering bypassed the Goal source")?
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(
        service
            .start_goal_generation(
                &owner_target,
                &CallerContext::local(),
                request(GenerationPurpose::Review, "another-review"),
                control.clone()
            )
            .await
            .err()
            .ok_or("one Thread admitted two active Turns")?
            .code,
        ErrorCode::Conflict
    );
    let input_target = target(&input_thread);
    let input = service
        .start_goal_generation(
            &input_target,
            &CallerContext::local(),
            request(GenerationPurpose::Input, "new-input"),
            control.clone(),
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(3), arrivals.recv())
        .await?
        .ok_or("input was hidden behind owner work")?;
    assert_eq!(service.read(&root.turn_id)?.status, TurnStatus::Running);
    assert_eq!(service.read(&input.turn_id)?.status, TurnStatus::Running);
    assert!(service.lock_state().active_workspaces.is_empty());
    assert!(
        service
            .check_active_turn_limit(&service.lock_state())
            .is_err()
    );
    executor.release.add_permits(2);
    assert_eq!(
        finished(&service, &root.turn_id).await?.status,
        TurnStatus::Completed
    );
    assert_eq!(
        finished(&service, &input.turn_id).await?.status,
        TurnStatus::Completed
    );
    assert_eq!(control.attempts.load(Ordering::SeqCst), 2);
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn owner_changed_during_goal_callback_blocks_provider_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let (entered, mut arrivals) = tokio::sync::mpsc::unbounded_channel();
    let executor = Arc::new(HeldExecutor {
        entered,
        release: tokio::sync::Semaphore::new(0),
        inner: MockExecutor::always_text("must not run"),
    });
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(app_with_executor(executor)?, &[], store.clone())?;
    let owner = owner(&service, directory.path()).await?;
    let (entered, mut callbacks) = tokio::sync::mpsc::unbounded_channel();
    let control = Arc::new(HeldAdmission {
        entered,
        release: tokio::sync::Semaphore::new(0),
    });
    let receipt = service
        .start_goal_generation(
            &target(&owner),
            &CallerContext::local(),
            request(GenerationPurpose::Planning, "held-admission"),
            control.clone(),
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(3), callbacks.recv())
        .await?
        .ok_or("callback did not start")?;
    let previous = service.initialize_execution().await?;
    store.stop_owner(&previous).await?;
    store.claim_owner("replacement-owner").await?;
    control.release.add_permits(1);
    assert_eq!(
        finished(&service, &receipt.turn_id).await?.status,
        TurnStatus::RecoveryRequired
    );
    assert!(
        arrivals.try_recv().is_err(),
        "old owner dispatched after its awaited callback"
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cancelled_provider_work_settles_before_shutdown_releases_ownership()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let (entered, mut arrivals) = tokio::sync::mpsc::unbounded_channel();
    let executor = Arc::new(HeldExecutor {
        entered,
        release: tokio::sync::Semaphore::new(0),
        inner: MockExecutor::always_text("late answer"),
    });
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(app_with_executor(executor)?, &[], store.clone())?;
    let owner = owner(&service, directory.path()).await?;
    let receipt = service
        .start_goal_generation(
            &target(&owner),
            &CallerContext::local(),
            request(GenerationPurpose::Planning, "cancel-provider"),
            Arc::new(Control {
                acknowledged: true,
                attempts: AtomicUsize::new(0),
            }),
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(3), arrivals.recv())
        .await?
        .ok_or("provider did not start")?;
    service.cancel(&receipt.turn_id).await?;
    let result = finished(&service, &receipt.turn_id).await?;
    assert_eq!(result.status, TurnStatus::Cancelled);
    assert!(result.final_answer.is_none());
    let stored = store.load(&owner.thread_id).await?.ok_or("root missing")?;
    assert!(stored.records.iter().any(|record| matches!(record, ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::GenerationAttempt { .. }))));
    service.shutdown().await;
    assert!(service.lock_state().running_turns.is_empty());
    assert!(
        store
            .read_owner(&service.inner.instance_id)
            .await?
            .ok_or("owner missing")?
            .stopped_at_ms
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn stopped_generation_reopens_without_workspace_or_replaying_the_original_request()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("missing-project");
    let store = Arc::new(MemoryExecutionStore::default());
    let first = ThreadService::with_store(
        app_with_executor(Arc::new(MockExecutor::always_text("original answer")))?,
        &[],
        store.clone(),
    )?;
    let owner = owner(&first, &workspace).await?;
    let control = Arc::new(Control {
        acknowledged: true,
        attempts: AtomicUsize::new(0),
    });
    let original = request(GenerationPurpose::Planning, "original");
    let receipt = first
        .start_goal_generation(
            &target(&owner),
            &CallerContext::local(),
            original,
            control.clone(),
        )
        .await?;
    assert_eq!(
        finished(&first, &receipt.turn_id).await?.status,
        TurnStatus::Completed
    );
    first.shutdown().await;
    let second = ThreadService::with_store(
        app_with_executor(Arc::new(MockExecutor::always_text("next answer")))?,
        &[],
        store,
    )?;
    let target = crate::thread::ThreadTarget {
        thread_id: owner.thread_id,
        server_instance_id: second.inner.instance_id.clone(),
    };
    let cold = second
        .read_stored_turn(&target, &CallerContext::local(), &receipt.turn_id)
        .await?;
    assert_eq!(cold.final_answer.as_deref(), Some("original answer"));
    assert!(second.lock_state().threads.is_empty());
    let replay = second
        .find_generation_admission(
            &target,
            &CallerContext::local(),
            &request(GenerationPurpose::Planning, "original").origin,
        )
        .await?
        .ok_or("original admission missing")?;
    assert_eq!(replay.turn_id, receipt.turn_id);
    assert_eq!(control.attempts.load(Ordering::SeqCst), 1);
    let next = second
        .start_goal_generation(
            &target,
            &CallerContext::local(),
            request(GenerationPurpose::Planning, "next"),
            control.clone(),
        )
        .await?;
    let result = finished(&second, &next.turn_id).await?;
    assert_eq!(result.status, TurnStatus::Completed, "{:?}", result.detail);
    assert_eq!(result.final_answer.as_deref(), Some("next answer"));
    assert_eq!(control.attempts.load(Ordering::SeqCst), 2);
    assert!(!workspace.exists());
    second.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn tool_output_is_retained_as_attempt_evidence_but_never_executed_or_completed()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let app = app_with_executor(Arc::new(MockExecutor::new(vec![MockResponse::Generate(
        turn(vec![tool_call(
            "unadmitted-tool",
            "write",
            serde_json::json!({"path":"unadmitted.txt","content":"must not execute"}),
        )]),
    )])))?;
    let service = ThreadService::with_store(app, &[], store.clone())?;
    let owner = owner(&service, directory.path()).await?;
    let receipt = service
        .start_goal_generation(
            &target(&owner),
            &CallerContext::local(),
            request(GenerationPurpose::Planning, "tool-output"),
            Arc::new(Control {
                acknowledged: true,
                attempts: AtomicUsize::new(0),
            }),
        )
        .await?;
    let result = finished(&service, &receipt.turn_id).await?;
    assert_eq!(result.status, TurnStatus::Failed);
    assert!(result.final_answer.is_none());
    assert!(!directory.path().join("unadmitted.txt").exists());
    let saved = store.load(&owner.thread_id).await?.ok_or("root missing")?;
    assert!(saved.records.iter().any(|record| matches!(record, ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::GenerationAttempt { report } if report.result.is_some()))));
    assert!(!saved.records.iter().any(|record| matches!(record, ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::ToolIntent { .. }))));
    service.shutdown().await;
    Ok(())
}
