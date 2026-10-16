use super::*;
use crate::goal_store::DatabaseGoalStore;
use bitrouter_orchestrator::goal::{
    GoalError, GoalOperation, GoalPage, GoalStore, HistoryPage, OperationKind,
};

#[derive(Clone, Copy)]
enum Fault {
    Conflict,
    LostAcknowledgement,
    Storage,
    LostAcknowledgementAfterPause,
}

struct FaultStore {
    inner: Arc<DatabaseGoalStore>,
    fault: std::sync::Mutex<Option<(Fault, usize)>>,
}

impl FaultStore {
    fn inject(&self, fault: Fault, count: usize) -> Result<()> {
        *self
            .fault
            .lock()
            .map_err(|_| anyhow::anyhow!("fault lock poisoned"))? = Some((fault, count));
        Ok(())
    }
}

#[async_trait::async_trait]
impl GoalStore for FaultStore {
    async fn get(&self, owner: &str, id: &str) -> std::result::Result<GoalSnapshot, GoalError> {
        self.inner.get(owner, id).await
    }
    async fn operation(
        &self,
        owner: &str,
        id: &str,
    ) -> std::result::Result<Option<GoalOperation>, GoalError> {
        self.inner.operation(owner, id).await
    }
    async fn list(
        &self,
        owner: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
    ) -> std::result::Result<GoalPage, GoalError> {
        self.inner.list(owner, after, cutoff, limit).await
    }
    async fn history(
        &self,
        owner: &str,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
    ) -> std::result::Result<HistoryPage, GoalError> {
        self.inner.history(owner, id, after, cutoff, limit).await
    }
    async fn commit(
        &self,
        revision: Option<u64>,
        goal: GoalSnapshot,
        operation: GoalOperation,
    ) -> std::result::Result<GoalOperation, GoalError> {
        let fault = if matches!(&operation.kind, OperationKind::Workflow { change } if matches!(change.as_ref(), Change::Accounting { .. }))
        {
            let mut fault = self.fault.lock().map_err(|_| GoalError::Storage {
                message: "fault lock poisoned".into(),
            })?;
            match fault.as_mut() {
                Some((kind, count)) if *count > 0 => {
                    *count -= 1;
                    Some(*kind)
                }
                _ => None,
            }
        } else {
            None
        };
        match fault {
            Some(Fault::Conflict) => Err(GoalError::Conflict),
            Some(Fault::Storage) => Err(GoalError::Storage {
                message: "fixture storage unavailable".into(),
            }),
            Some(kind @ (Fault::LostAcknowledgement | Fault::LostAcknowledgementAfterPause)) => {
                let id = goal.id.clone();
                self.inner.commit(revision, goal, operation).await?;
                if matches!(kind, Fault::LostAcknowledgementAfterPause) {
                    let service = GoalService::new(self.inner.clone());
                    let current = self.inner.get("local", &id).await?;
                    service
                        .execute(
                            "local",
                            GoalCommand::Control {
                                operation_id: "pause-after-persist".into(),
                                goal_id: id,
                                expected_revision: current.revision,
                                intent: RunIntent::Pause,
                            },
                            now().map_err(|error| GoalError::Storage {
                                message: error.to_string(),
                            })?,
                        )
                        .await?;
                }
                Err(GoalError::Storage {
                    message: "fixture commit reply lost".into(),
                })
            }
            None => self.inner.commit(revision, goal, operation).await,
        }
    }
}

async fn snapshot(goals: &GoalService, id: &str) -> Result<GoalSnapshot> {
    let GoalReply::Snapshot { goal } = goals
        .execute("local", GoalCommand::Get { goal_id: id.into() }, now()?)
        .await?
    else {
        anyhow::bail!("missing snapshot");
    };
    Ok(*goal)
}

async fn worker_change(goals: &GoalService, id: &str, key: &str, change: Change) -> Result<()> {
    let goal = snapshot(goals, id).await?;
    goals
        .workflow_change(
            WorkflowActor {
                owner: "local",
                coordinator_id: "model-test",
            },
            id,
            key,
            goal.requirements_revision,
            change,
            now()?,
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn worker_accounting_survives_an_independent_edit_but_rejects_its_own_stale_scope()
-> Result<()> {
    use bitrouter_orchestrator::goal::input::{
        Event as InputEvent, Interpretation, Stage, TaskUpdate,
    };
    let directory = tempfile::tempdir()?;
    let (mut control, _, goals) = control(directory.path()).await?;
    let id = control.goal.id.clone();
    // Domain transitions only: no Git work, provider request or execution
    // adoption is claimed by this source/accounting concurrency regression.
    worker_change(
        &goals,
        &id,
        "worker-owner",
        Change::BindOwnerThread {
            thread_id: "owner-thread".into(),
        },
    )
    .await?;
    worker_change(
        &goals,
        &id,
        "worker-plan-request",
        Change::RequestPlan {
            attempt_id: "worker-plan".into(),
        },
    )
    .await?;
    let plan: Plan = serde_json::from_value(serde_json::json!({
        "tasks":[
            {"id":"left","title":"left","instruction":"Implement left","harness":"fixture","dependencies":[]},
            {"id":"right","title":"right","instruction":"Implement right","harness":"fixture","dependencies":[]}
        ],
        "completion":{"conditions":[{"id":"result","description":"Deliver both verified results"}],"checks":[],"human_acceptance_required":false}
    }))?;
    worker_change(
        &goals,
        &id,
        "worker-plan-install",
        Change::InstallPlan {
            attempt_id: "worker-plan".into(),
            plan,
            available_harnesses: BTreeSet::from(["fixture".into()]),
        },
    )
    .await?;
    for task in ["left", "right"] {
        let execution_id = format!("{task}-attempt");
        worker_change(
            &goals,
            &id,
            &format!("{task}-request"),
            Change::RequestExecution {
                task_id: task.into(),
                execution_id: execution_id.clone(),
                workspace: directory.path().join(task),
            },
        )
        .await?;
        worker_change(
            &goals,
            &id,
            &format!("{task}-workspace"),
            Change::RegisterWorkspace {
                execution_id: execution_id.clone(),
                base_commit: "0".repeat(40),
            },
        )
        .await?;
        worker_change(
            &goals,
            &id,
            &format!("{task}-dispatch"),
            Change::RequestDispatch { execution_id },
        )
        .await?;
    }
    let control_source_revision = control.goal.control_source_revision;
    let source = |task: &str| bitrouter_orchestrator::thread::GoalWork {
        goal_id: id.clone(),
        owner_thread_id: "owner-thread".into(),
        task_id: task.into(),
        execution_id: format!("{task}-attempt"),
        task_revision: 1,
        control_source_revision,
    };
    Arc::get_mut(&mut control)
        .context("fixture control unexpectedly shared")?
        .worker = Some(source("right"));
    let before = snapshot(&goals, &id).await?;
    let GoalReply::Accepted { operation } = goals
        .execute(
            "local",
            GoalCommand::Input {
                operation_id: "edit-independent-left".into(),
                goal_id: id.clone(),
                expected_revision: before.revision,
                text: "Only left: add compatibility coverage".into(),
                task_reference: Some("left".into()),
            },
            now()?,
        )
        .await?
    else {
        anyhow::bail!("input receipt missing")
    };
    worker_change(
        &goals,
        &id,
        "begin-independent-edit",
        Change::Input {
            event: InputEvent::Begin {
                operation: Box::new(operation),
                attempt_id: "independent-edit".into(),
                stage: Stage::Reasoning,
            },
        },
    )
    .await?;
    worker_change(
        &goals,
        &id,
        "finish-independent-edit",
        Change::Input {
            event: InputEvent::Finish {
                attempt_id: "independent-edit".into(),
                reasoning_request_id: Some("fixture-interpretation".into()),
                interpretation: Interpretation {
                    task_updates: vec![TaskUpdate {
                        task_id: "left".into(),
                        instruction: "Implement left and add compatibility coverage".into(),
                        worker: None,
                        resources: None,
                        additional_checks: vec![],
                    }],
                    ..Default::default()
                },
            },
        },
    )
    .await?;
    let edited = snapshot(&goals, &id).await?;
    assert!(edited.requirements_revision > control.goal.requirements_revision);
    control
        .begin(
            WorkKind::Preparation,
            "right-after-left-edit",
            None,
            serde_json::json!({"fixture":"source admission"}),
        )
        .await?;
    let active = snapshot(&goals, &id).await?;
    assert_eq!(
        active
            .accounting
            .in_flight
            .last()
            .and_then(|work| work.source.as_deref()),
        Some(&source("right"))
    );
    // A restored control sees the current Goal, while this action keeps the
    // revision and payload of its original accounting receipt.
    let work_started = active.accounting.work_started;
    Arc::get_mut(&mut control)
        .context("fixture control unexpectedly shared")?
        .goal = edited;
    control
        .begin(
            WorkKind::Preparation,
            "right-after-left-edit",
            None,
            serde_json::json!({"fixture":"source admission"}),
        )
        .await?;
    assert_eq!(
        snapshot(&goals, &id).await?.accounting.work_started,
        work_started
    );
    Arc::get_mut(&mut control)
        .context("fixture control unexpectedly shared")?
        .worker = Some(source("left"));
    assert!(
        control
            .begin(
                WorkKind::Preparation,
                "stale-left",
                None,
                serde_json::json!({})
            )
            .await
            .is_err()
    );
    assert_eq!(
        snapshot(&goals, &id).await?.accounting.work_started,
        active.accounting.work_started
    );
    Arc::get_mut(&mut control)
        .context("fixture control unexpectedly shared")?
        .worker = Some(source("right"));
    let quote = NativeTokenReservation {
        micro_usd: 10,
        pricing_version: "fixture-tariff".into(),
        provider: "fixture".into(),
        model: "fixture-model".into(),
        input_tokens_bound: 10,
        output_tokens_bound: 10,
        evidence: serde_json::json!({"fixture":"explicit reservation; no provider is invoked"}),
    };
    let current = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::SetBudget {
                operation_id: "worker-budget-limit".into(),
                goal_id: id.clone(),
                expected_revision: current.revision,
                limits: bitrouter_orchestrator::goal::budget::Limits {
                    elapsed_ms: None,
                    managed_token_estimates_micro_usd: Some(10),
                },
            },
            now()?,
        )
        .await?;
    control
        .begin_with_reservation(
            WorkKind::ProviderAttempt,
            "held-worker-credit",
            Some("held-request"),
            serde_json::json!({"fixture":"admission only"}),
            Some(quote.clone()),
        )
        .await?;
    assert!(
        control
            .begin_with_reservation(
                WorkKind::ProviderAttempt,
                "waiting-worker-credit",
                Some("waiting-request"),
                serde_json::json!({"fixture":"admission only"}),
                Some(quote.clone())
            )
            .await
            .is_err()
    );
    let waiting = snapshot(&goals, &id).await?;
    assert!(waiting.budget.halt.is_none());
    assert!(
        control.acknowledged(),
        "a known budget rejection is not a lost write acknowledgement"
    );
    // Both calls are domain-only admissions. No provider was dispatched, so
    // settling zero releases the exact held quote before retrying the same key.
    control
        .observe_with_reservation(
            "held-worker-credit",
            serde_json::json!({"not_dispatched":true}),
            Some(0),
            true,
        )
        .await?;
    control
        .begin_with_reservation(
            WorkKind::ProviderAttempt,
            "waiting-worker-credit",
            Some("waiting-request"),
            serde_json::json!({"fixture":"admission only"}),
            Some(quote),
        )
        .await?;
    control
        .observe_with_reservation(
            "waiting-worker-credit",
            serde_json::json!({"not_dispatched":true}),
            Some(0),
            true,
        )
        .await?;
    let current = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "pause-worker-goal".into(),
                goal_id: id.clone(),
                expected_revision: current.revision,
                intent: RunIntent::Pause,
            },
            now()?,
        )
        .await?;
    assert!(control.gate().await.is_err());
    // Revocation stops admission, while the original in-flight outcome remains
    // recordable and cannot be converted into a second work attempt.
    control
        .observe(
            "right-after-left-edit",
            serde_json::json!({"settled":true}),
            None,
        )
        .await?;
    assert!(snapshot(&goals, &id).await?.accounting.in_flight.is_empty());
    let operation_id = format!("{}-outcome", key_id("right-after-left-edit"));
    let GoalReply::Accepted { operation } = goals
        .execute(
            "local",
            GoalCommand::Operation {
                operation_id: operation_id.clone(),
            },
            now()?,
        )
        .await?
    else {
        anyhow::bail!("outcome receipt missing")
    };
    let OperationKind::Workflow { change } = operation.kind.clone() else {
        anyhow::bail!("accounting receipt has wrong kind")
    };
    let replay = goals
        .workflow_change(
            WorkflowActor {
                owner: "local",
                coordinator_id: "model-test",
            },
            &id,
            &operation_id,
            1,
            *change,
            now()?,
        )
        .await?;
    assert_eq!(
        replay, operation,
        "settlement lost its original source revision"
    );
    Ok(())
}

async fn control(root: &Path) -> Result<(Arc<ModelControl>, Arc<FaultStore>, Arc<GoalService>)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = Arc::new(FaultStore {
        inner: Arc::new(DatabaseGoalStore::new(db)),
        fault: std::sync::Mutex::new(None),
    });
    let goals = Arc::new(GoalService::new(store.clone()));
    let GoalReply::Accepted { operation } = goals
        .execute(
            "local",
            GoalCommand::Create {
                operation_id: "accounting-goal".into(),
                title: "Model accounting".into(),
                workspace: root.into(),
                objective: "Record model work before dispatch".into(),
                budget_limits: Default::default(),
            },
            now()?,
        )
        .await?
    else {
        anyhow::bail!("missing creation");
    };
    let time = now()?;
    goals
        .workflow_change(
            WorkflowActor {
                owner: "local",
                coordinator_id: "model-test",
            },
            &operation.goal_id,
            "claim-model-owner",
            1,
            Change::ClaimCoordinator {
                id: "model-test".into(),
                expires_at_ms: time + OWNER_LEASE_MS,
            },
            time,
        )
        .await?;
    let goal = snapshot(&goals, &operation.goal_id).await?;
    let control = Arc::new(ModelControl {
        goals: goals.clone(),
        goal,
        coordinator_id: "model-test".into(),
        root: root.join("artifacts"),
        explicit_input: false,
        generation: None,
        worker: None,
        failed_ack: AtomicBool::new(false),
        failure: tokio::sync::Mutex::new(None),
        plans: tokio::sync::Mutex::new(BTreeMap::new()),
    });
    Ok((control, store, goals))
}

#[tokio::test]
async fn native_typed_callback_settles_original_receipt_after_pause_and_lost_reply() -> Result<()> {
    use bitrouter_orchestrator::control::WorkControl;
    use bitrouter_sdk::decision_model::policy::DecisionPricing;
    use bitrouter_sdk::decision_model::types::{
        DecisionError, DecisionFailure, DecisionRequest, DecisionUsage, Question,
    };
    let root = tempfile::tempdir()?;
    let (control, store, goals) = control(root.path()).await?;
    let pricing = DecisionPricing {
        input_usd_per_million: 1.0,
        output_usd_per_million: 0.0,
        max_input_tokens: Some(100),
        max_output_tokens: None,
    };
    let request = DecisionRequest {
        model: "native-context-fixture".into(),
        state: serde_json::json!({"source":"native context callback fixture"}),
        questions: BTreeMap::from([(
            "q".into(),
            Question::Noul {
                instructions: Some(serde_json::json!("Evaluate this fixture")),
                criteria: None,
            },
        )]),
    };
    store.inject(Fault::LostAcknowledgement, 1)?;
    control
        .before_typed_decision("original-native-decision", &request, Some(&pricing))
        .await?;
    let admitted = snapshot(&goals, &control.goal.id).await?;
    assert_eq!(admitted.accounting.in_flight.len(), 1);
    assert_eq!(admitted.budget.reserved_micro_usd()?, 100);
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "pause-native-context".into(),
                goal_id: admitted.id.clone(),
                expected_revision: admitted.revision,
                intent: RunIntent::Pause,
            },
            now()?,
        )
        .await?;
    let outcome = Err(DecisionError {
        kind: DecisionFailure::Cancelled,
        message: "native fixture cancelled with observed usage".into(),
        usage: Some(DecisionUsage {
            input_tokens: 40,
            output_tokens: 2,
        }),
        may_have_run: true,
    });
    store.inject(Fault::LostAcknowledgement, 1)?;
    control
        .after_typed_decision("original-native-decision", &outcome, Some(&pricing))
        .await?;
    let settled = snapshot(&goals, &admitted.id).await?;
    assert_eq!(settled.run_intent, RunIntent::Pause);
    assert!(settled.accounting.in_flight.is_empty());
    assert_eq!(settled.budget.reserved_micro_usd()?, 0);
    assert_eq!(settled.accounting.estimated_model_tokens_micro_usd, 40);
    control
        .after_typed_decision("original-native-decision", &outcome, Some(&pricing))
        .await?;
    assert_eq!(snapshot(&goals, &admitted.id).await?, settled);
    assert!(control.acknowledged());
    assert!(control.authorize().await.is_err());

    // A pause can race the durable intent ACK itself. The callback rejects
    // dispatch, but the already accepted quote still needs a zero-dispatch
    // settlement with its original identity.
    let interrupted_root = tempfile::tempdir()?;
    let (interrupted, store, goals) = self::control(interrupted_root.path()).await?;
    store.inject(Fault::LostAcknowledgementAfterPause, 1)?;
    assert!(
        interrupted
            .before_typed_decision("paused-intent", &request, Some(&pricing))
            .await
            .is_err()
    );
    assert_eq!(
        snapshot(&goals, &interrupted.goal.id)
            .await?
            .budget
            .reserved_micro_usd()?,
        100
    );
    let not_dispatched = Err(DecisionError::invalid_request(
        "source closed before dispatch",
    ));
    interrupted
        .after_typed_decision("paused-intent", &not_dispatched, Some(&pricing))
        .await?;
    let saved = snapshot(&goals, &interrupted.goal.id).await?;
    assert_eq!(saved.run_intent, RunIntent::Pause);
    assert!(saved.accounting.in_flight.is_empty());
    assert_eq!(saved.budget.reserved_micro_usd()?, 0);
    assert_eq!(saved.accounting.estimated_model_tokens_micro_usd, 0);
    interrupted
        .after_typed_decision("never-admitted", &not_dispatched, Some(&pricing))
        .await?;
    assert_eq!(snapshot(&goals, &interrupted.goal.id).await?, saved);

    // A backend's no-dispatch flag cannot erase usage it also reports.
    let usage_root = tempfile::tempdir()?;
    let (usage_control, _, goals) = self::control(usage_root.path()).await?;
    usage_control
        .before_typed_decision("contradictory-flag", &request, Some(&pricing))
        .await?;
    usage_control
        .after_typed_decision(
            "contradictory-flag",
            &Err(DecisionError {
                kind: DecisionFailure::InvalidResponse,
                message: "fixture reported usage despite its no-dispatch flag".into(),
                usage: Some(DecisionUsage {
                    input_tokens: 13,
                    output_tokens: 0,
                }),
                may_have_run: false,
            }),
            Some(&pricing),
        )
        .await?;
    let charged = snapshot(&goals, &usage_control.goal.id).await?;
    assert_eq!(charged.accounting.estimated_model_tokens_micro_usd, 13);
    assert_eq!(charged.budget.reserved_micro_usd()?, 0);
    Ok(())
}

#[tokio::test]
async fn typed_quotes_share_goal_budget_and_preserve_unknown_or_excess_usage() -> Result<()> {
    use bitrouter_sdk::decision_model::{
        policy::DecisionPricing,
        types::{
            Answer, DecisionError, DecisionFailure, DecisionRequest, DecisionResponse,
            DecisionUsage, Question,
        },
    };
    let pricing = DecisionPricing {
        input_usd_per_million: 1.0,
        output_usd_per_million: 0.0,
        max_input_tokens: Some(100),
        max_output_tokens: None,
    };
    let request = DecisionRequest {
        model: "fixture-typed".into(),
        state: serde_json::json!({"fixture":true}),
        questions: BTreeMap::from([(
            "q".into(),
            Question::Noul {
                instructions: Some(serde_json::json!("Judge this fixture")),
                criteria: None,
            },
        )]),
    };
    for (case, tokens, estimate, may_have_run, held, halt) in [
        ("known", Some(50), Some(50), true, 0, false),
        ("too_many", Some(101), Some(101), true, 0, true),
        ("wrong_tariff", Some(50), Some(80), true, 0, true),
        ("unknown", None, None, true, 1, false),
        ("not_dispatched", None, None, false, 0, false),
    ] {
        let root = tempfile::tempdir()?;
        let (control, _, goals) = control(root.path()).await?;
        let current = snapshot(&goals, &control.goal.id).await?;
        goals
            .execute(
                "local",
                GoalCommand::SetBudget {
                    operation_id: format!("cap-{case}"),
                    goal_id: current.id.clone(),
                    expected_revision: current.revision,
                    limits: bitrouter_orchestrator::goal::budget::Limits {
                        elapsed_ms: None,
                        managed_token_estimates_micro_usd: Some(200),
                    },
                },
                now()?,
            )
            .await?;
        control.begin_typed(case, &request, Some(&pricing)).await?;
        let before = snapshot(&goals, &current.id).await?;
        assert_eq!(before.budget.reserved_micro_usd()?, 100);
        assert_eq!(before.accounting.in_flight[0].kind, WorkKind::TypedDecision);
        let outcome = match tokens {
            Some(input_tokens) => Ok(DecisionResponse {
                model: "fixture-typed-revision".into(),
                answers: BTreeMap::from([("q".into(), Answer::Noul { noul: 0.6 })]),
                usage: DecisionUsage {
                    input_tokens,
                    output_tokens: 5,
                },
            }),
            None => Err(DecisionError {
                kind: if may_have_run {
                    DecisionFailure::Transport
                } else {
                    DecisionFailure::InvalidRequest
                },
                message: "fixture failure".into(),
                usage: None,
                may_have_run,
            }),
        };
        control.observe_typed(case, &outcome, estimate).await?;
        let after = snapshot(&goals, &current.id).await?;
        assert_eq!(after.budget.held.len(), held, "{case}");
        assert_eq!(after.budget.halt.is_some(), halt, "{case}");
        assert_eq!(
            after.accounting.estimated_model_tokens_micro_usd,
            estimate.unwrap_or(0),
            "{case}"
        );
        assert!(after.accounting.in_flight.is_empty());
        if held > 0 {
            let competing = DecisionPricing {
                input_usd_per_million: 2.0,
                ..pricing.clone()
            };
            assert!(
                control
                    .begin_typed("competing", &request, Some(&competing))
                    .await
                    .is_err()
            );
            assert_eq!(
                snapshot(&goals, &current.id).await?,
                after,
                "failed reservation changed persistent state"
            );
        }
    }
    Ok(())
}

#[test]
fn typed_quotes_require_bounds_for_every_nonzero_price() {
    use bitrouter_sdk::decision_model::policy::DecisionPricing;
    let mut pricing = DecisionPricing {
        input_usd_per_million: 1.0,
        output_usd_per_million: 2.0,
        max_input_tokens: None,
        max_output_tokens: None,
    };
    assert!(typed_quote("fixture", &pricing).is_none());
    pricing.max_input_tokens = Some(10);
    assert!(typed_quote("fixture", &pricing).is_none());
    pricing.max_output_tokens = Some(5);
    assert_eq!(
        typed_quote("fixture", &pricing).map(|quote| quote.micro_usd),
        Some(20)
    );
    pricing.max_input_tokens = Some(0);
    assert!(typed_quote("fixture", &pricing).is_none());
    pricing.max_input_tokens = Some(u64::MAX);
    pricing.input_usd_per_million = f64::MAX;
    assert!(typed_quote("fixture", &pricing).is_none());
}

#[tokio::test]
async fn bounded_accounting_conflicts_do_not_poison_model_admission() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (control, store, goals) = control(root.path()).await?;
    store.inject(Fault::Conflict, 5)?;
    control.before_context_validation("same-work").await?;
    assert!(control.acknowledged());
    let before = snapshot(&goals, &control.goal.id).await?;
    assert_eq!(before.accounting.work_started, 1);
    assert_eq!(before.accounting.provider_attempts, 0);
    assert_eq!(before.accounting.in_flight.len(), 1);
    control
        .observe(
            "same-work/validation",
            serde_json::json!({"completed":true}),
            None,
        )
        .await?;
    let after = snapshot(&goals, &control.goal.id).await?;
    assert_eq!(after.accounting.work_observed, 1);
    assert!(after.accounting.in_flight.is_empty());
    Ok(())
}

#[tokio::test]
async fn lost_accounting_reply_confirms_the_same_receipt_without_duplicate_work() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (control, store, goals) = control(root.path()).await?;
    store.inject(Fault::LostAcknowledgement, 1)?;
    control.before_context_validation("lost-reply").await?;
    assert!(control.acknowledged());
    let head = snapshot(&goals, &control.goal.id).await?;
    assert_eq!(head.accounting.work_started, 1);
    assert_eq!(head.accounting.in_flight.len(), 1);
    assert_eq!(head.accounting.provider_attempts, 0);
    store.inject(Fault::LostAcknowledgement, 1)?;
    control
        .observe(
            "lost-reply/validation",
            serde_json::json!({"completed":true}),
            None,
        )
        .await?;
    let head = snapshot(&goals, &control.goal.id).await?;
    assert_eq!(head.accounting.work_observed, 1);
    assert_eq!(head.accounting.unpriced_outcomes, 1);
    assert!(head.accounting.in_flight.is_empty());
    Ok(())
}

#[tokio::test]
async fn persistent_accounting_failure_retains_its_cause_and_fences_new_work() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (control, store, goals) = control(root.path()).await?;
    store.inject(Fault::Storage, 100)?;
    let error = control
        .before_context_validation("unavailable-work")
        .await
        .err()
        .context("expected unavailable storage")?;
    assert!(error.to_string().contains("fixture storage unavailable"));
    assert!(!control.acknowledged());
    assert!(
        control
            .failure_detail()
            .await
            .is_some_and(|detail| detail.contains("accounting operation")
                && detail.contains("fixture storage unavailable"))
    );
    assert!(
        control
            .before_context_validation("another-work")
            .await
            .is_err()
    );
    let head = snapshot(&goals, &control.goal.id).await?;
    assert_eq!(head.accounting.work_started, 0);
    assert_eq!(head.accounting.provider_attempts, 0);
    assert!(head.accounting.in_flight.is_empty());
    Ok(())
}

#[tokio::test]
async fn recovered_intent_receipt_does_not_authorize_work_after_pause() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (control, store, goals) = control(root.path()).await?;
    store.inject(Fault::LostAcknowledgementAfterPause, 1)?;
    let error = control
        .before_context_validation("paused-reply")
        .await
        .err()
        .context("expected paused admission")?;
    assert!(error.to_string().contains("paused or stopped"));
    let head = snapshot(&goals, &control.goal.id).await?;
    assert_eq!(head.run_intent, RunIntent::Pause);
    assert_eq!(head.accounting.work_started, 1);
    assert_eq!(head.accounting.provider_attempts, 0);
    assert!(
        control
            .before_context_validation("forbidden-new-work")
            .await
            .is_err()
    );
    assert_eq!(
        snapshot(&goals, &control.goal.id)
            .await?
            .accounting
            .work_started,
        1
    );
    Ok(())
}

#[tokio::test]
async fn model_gate_reports_owner_transfer_instead_of_an_opaque_unavailability() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (control, _, goals) = control(root.path()).await?;
    let time = now()?;
    goals
        .workflow_change(
            WorkflowActor {
                owner: "local",
                coordinator_id: "model-test",
            },
            &control.goal.id,
            "shorten-owner",
            1,
            Change::ClaimCoordinator {
                id: "model-test".into(),
                expires_at_ms: time + 1,
            },
            time,
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(3)).await;
    let time = now()?;
    goals
        .workflow_change(
            WorkflowActor {
                owner: "local",
                coordinator_id: "different-owner",
            },
            &control.goal.id,
            "transfer-owner",
            1,
            Change::ClaimCoordinator {
                id: "different-owner".into(),
                expires_at_ms: time + OWNER_LEASE_MS,
            },
            time,
        )
        .await?;
    let error = control
        .before_context_validation("wrong-owner-work")
        .await
        .err()
        .context("expected ownership fence")?;
    assert!(error.to_string().contains("different owner"));
    assert_eq!(
        snapshot(&goals, &control.goal.id)
            .await?
            .accounting
            .work_started,
        0
    );
    Ok(())
}
