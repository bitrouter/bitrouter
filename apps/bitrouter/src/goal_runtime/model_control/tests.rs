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
        failed_ack: AtomicBool::new(false),
        failure: tokio::sync::Mutex::new(None),
        plans: tokio::sync::Mutex::new(BTreeMap::new()),
    });
    Ok((control, store, goals))
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
