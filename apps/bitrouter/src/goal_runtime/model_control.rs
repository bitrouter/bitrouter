use super::*;
use bitrouter_orchestrator::goal::accounting::{Event, Work, WorkKind};
use bitrouter_sdk::language_model::native::{
    NativeAttemptReport, NativeContextValidationReport, NativeExecutionControl,
    NativeInputCountReport, NativePlan, NativePlanAdmission,
};
use bitrouter_sdk::language_model::native_accounting::{NativeTokenCost, NativeTokenReservation};
use bitrouter_sdk::language_model::native_preparation::{
    NativePreparationWork, NativePreparationWorkReport,
};
use bitrouter_sdk::language_model::native_work::{NativeProviderWork, NativeProviderWorkReport};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct ModelControl {
    goals: Arc<GoalService>,
    goal: GoalSnapshot,
    coordinator_id: String,
    root: PathBuf,
    explicit_input: bool,
    failed_ack: AtomicBool,
    plans: tokio::sync::Mutex<BTreeMap<String, NativePlan>>,
}

impl ModelControl {
    pub(super) fn new(
        coordinator: &Coordinator,
        goal: &GoalSnapshot,
        explicit_input: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            goals: coordinator.goals.clone(),
            goal: goal.clone(),
            coordinator_id: coordinator.id.clone(),
            root: crate::goal_artifacts::root(&coordinator.workspaces, &goal.id),
            explicit_input,
            failed_ack: AtomicBool::new(false),
            plans: tokio::sync::Mutex::new(BTreeMap::new()),
        })
    }

    pub(super) fn acknowledged(&self) -> bool {
        !self.failed_ack.load(Ordering::Acquire)
    }

    pub(super) async fn begin_typed(
        &self,
        attempt_id: &str,
        request: &bitrouter_sdk::decision_model::types::DecisionRequest,
    ) -> bitrouter_sdk::Result<()> {
        self.begin(
            WorkKind::TypedDecision,
            &format!("typed/{attempt_id}"),
            None,
            serde_json::to_value(request).map_err(|_| unavailable())?,
        )
        .await
    }

    pub(super) async fn observe_typed(
        &self,
        attempt_id: &str,
        outcome: &std::result::Result<
            bitrouter_sdk::decision_model::types::DecisionResponse,
            bitrouter_sdk::decision_model::types::DecisionError,
        >,
        estimate: Option<u64>,
    ) -> bitrouter_sdk::Result<()> {
        self.observe(
            &format!("typed/{attempt_id}"),
            serde_json::to_value(outcome).map_err(|_| unavailable())?,
            estimate,
        )
        .await
    }

    async fn gate(&self) -> bitrouter_sdk::Result<()> {
        if !self.acknowledged() {
            return Err(unavailable());
        }
        let response = self
            .goals
            .execute(
                "local",
                GoalCommand::Get {
                    goal_id: self.goal.id.clone(),
                },
                now().map_err(|_| unavailable())?,
            )
            .await
            .map_err(|_| unavailable())?;
        let GoalReply::Snapshot { goal } = response else {
            return Err(unavailable());
        };
        let time = now().map_err(|_| unavailable())?;
        if goal.requirements_revision != self.goal.requirements_revision
            || goal.budget.admission(goal.created_at_ms, time).is_err()
            || !self.explicit_input && goal.run_intent != RunIntent::Run
            || !self.explicit_input
                && goal
                    .workflow
                    .completion
                    .completed_for(goal.requirements_revision)
            || !goal
                .workflow
                .coordinator
                .as_ref()
                .is_some_and(|lease| lease.id == self.coordinator_id && lease.expires_at_ms > time)
        {
            return Err(unavailable());
        }
        Ok(())
    }

    async fn begin(
        &self,
        kind: WorkKind,
        key: &str,
        request_id: Option<&str>,
        value: serde_json::Value,
    ) -> bitrouter_sdk::Result<()> {
        self.begin_with_reservation(kind, key, request_id, value, None)
            .await
    }

    async fn begin_with_reservation(
        &self,
        kind: WorkKind,
        key: &str,
        request_id: Option<&str>,
        value: serde_json::Value,
        token_reservation: Option<NativeTokenReservation>,
    ) -> bitrouter_sdk::Result<()> {
        self.gate().await?;
        let id = key_id(key);
        let intent = crate::goal_artifacts::put(
            &self.root,
            &serde_json::to_vec(&value).map_err(|_| unavailable())?,
        )
        .await
        .map_err(|_| unavailable())?;
        self.commit(
            &format!("{id}-begin"),
            Event::Begin {
                work: Work {
                    id,
                    kind,
                    requirements_revision: self.goal.requirements_revision,
                    request_id: request_id.map(str::to_string),
                    intent,
                    token_reservation,
                },
            },
        )
        .await
    }

    async fn observe(
        &self,
        key: &str,
        value: serde_json::Value,
        estimate: Option<u64>,
    ) -> bitrouter_sdk::Result<()> {
        self.observe_with_reservation(key, value, estimate, false)
            .await
    }

    async fn observe_with_reservation(
        &self,
        key: &str,
        value: serde_json::Value,
        estimate: Option<u64>,
        reservation_verified: bool,
    ) -> bitrouter_sdk::Result<()> {
        let id = key_id(key);
        let report = crate::goal_artifacts::put(
            &self.root,
            &serde_json::to_vec(&value).map_err(|_| unavailable())?,
        )
        .await
        .map_err(|_| unavailable())?;
        self.commit(
            &format!("{id}-outcome"),
            Event::Observe {
                id,
                report,
                estimated_model_tokens_micro_usd: estimate,
                reservation_verified,
            },
        )
        .await
    }

    async fn commit(&self, operation: &str, event: Event) -> bitrouter_sdk::Result<()> {
        let provider_attempt =
            matches!(&event, Event::Begin { work } if work.kind == WorkKind::ProviderAttempt);
        let result = self
            .goals
            .workflow_change(
                WorkflowActor {
                    owner: "local",
                    coordinator_id: &self.coordinator_id,
                },
                &self.goal.id,
                operation,
                self.goal.requirements_revision,
                Change::Accounting { event },
                now().map_err(|_| unavailable())?,
            )
            .await;
        if matches!(
            &result,
            Err(bitrouter_orchestrator::goal::GoalError::BudgetExceeded { .. })
        ) && provider_attempt
        {
            let _ = self.goals.workflow_change(WorkflowActor { owner: "local", coordinator_id: &self.coordinator_id },
                &self.goal.id, &format!("{operation}-budget"), self.goal.requirements_revision,
                Change::Budget { event: bitrouter_orchestrator::goal::budget::Event::Halt { reason: "A provider attempt no longer fits the shared Goal budget; no dispatch occurred".into() } },
                now().map_err(|_| unavailable())?).await;
        }
        if result.is_err() {
            self.failed_ack.store(true, Ordering::Release);
        }
        result.map(|_| ()).map_err(|_| unavailable())
    }
}

#[async_trait::async_trait]
impl NativeExecutionControl for ModelControl {
    fn provider_response_byte_limit(&self) -> Option<u64> {
        Some(2 * 1024 * 1024)
    }
    fn canonical_output_byte_limit(&self) -> Option<u64> {
        Some(1024 * 1024)
    }

    async fn provider_cancelled(&self) {
        loop {
            if self.gate().await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn before_preparation_work(
        &self,
        work: &NativePreparationWork,
    ) -> bitrouter_sdk::Result<()> {
        self.begin(
            WorkKind::Preparation,
            &prep_key(work)?,
            Some(&work.request_id),
            serde_json::to_value(work).map_err(|_| unavailable())?,
        )
        .await
    }
    async fn after_preparation_work(
        &self,
        report: NativePreparationWorkReport,
    ) -> bitrouter_sdk::Result<()> {
        self.observe(
            &prep_key(&report.work)?,
            serde_json::to_value(&report).map_err(|_| unavailable())?,
            None,
        )
        .await
    }
    async fn before_input_count(
        &self,
        plan: &NativePlan,
        route_index: u32,
    ) -> bitrouter_sdk::Result<()> {
        self.begin(
            WorkKind::InputCount,
            &format!("{}/count/{route_index}", plan.request_id),
            Some(&plan.request_id),
            serde_json::json!({"plan":plan,"route_index":route_index}),
        )
        .await
    }
    async fn after_input_count(&self, report: NativeInputCountReport) -> bitrouter_sdk::Result<()> {
        self.observe(
            &format!("{}/count/{}", report.request_id, report.route_index),
            serde_json::to_value(&report).map_err(|_| unavailable())?,
            None,
        )
        .await
    }
    async fn before_context_validation(&self, request_id: &str) -> bitrouter_sdk::Result<()> {
        self.begin(
            WorkKind::ContextValidation,
            &format!("{request_id}/validation"),
            Some(request_id),
            serde_json::json!({"request_id":request_id}),
        )
        .await
    }
    async fn check_context_validation(&self, _: &str) -> bitrouter_sdk::Result<()> {
        self.gate().await
    }
    async fn after_context_validation(
        &self,
        report: NativeContextValidationReport,
    ) -> bitrouter_sdk::Result<()> {
        self.observe(
            &format!("{}/validation", report.request_id),
            serde_json::to_value(&report).map_err(|_| unavailable())?,
            None,
        )
        .await
    }
    async fn plan(&self, plan: NativePlan) -> bitrouter_sdk::Result<NativePlanAdmission> {
        self.plan_with_token_reservations(plan, Vec::new()).await
    }

    async fn plan_with_token_reservations(
        &self,
        plan: NativePlan,
        reservations: Vec<Option<NativeTokenReservation>>,
    ) -> bitrouter_sdk::Result<NativePlanAdmission> {
        let key = format!("{}/plan", plan.request_id);
        self.begin(
            WorkKind::Preparation,
            &key,
            Some(&plan.request_id),
            serde_json::to_value(&plan).map_err(|_| unavailable())?,
        )
        .await?;
        let response = self
            .goals
            .execute(
                "local",
                GoalCommand::Get {
                    goal_id: self.goal.id.clone(),
                },
                now().map_err(|_| unavailable())?,
            )
            .await
            .map_err(|_| unavailable())?;
        let GoalReply::Snapshot { goal } = response else {
            return Err(unavailable());
        };
        let indices = (0..plan.routes.len())
            .filter(|index| {
                goal.budget
                    .can_reserve(
                        goal.accounting.estimated_model_tokens_micro_usd,
                        reservations.get(*index).and_then(Option::as_ref),
                    )
                    .is_ok()
            })
            .map(u32::try_from)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| unavailable())?;
        self.observe(
            &key,
            serde_json::json!({"accepted_routes":indices,"token_reservations":reservations}),
            None,
        )
        .await?;
        if indices.is_empty() {
            self.goals.workflow_change(WorkflowActor { owner: "local", coordinator_id: &self.coordinator_id },
                &self.goal.id, &format!("{}-budget", key_id(&key)), self.goal.requirements_revision,
                Change::Budget { event: bitrouter_orchestrator::goal::budget::Event::Halt { reason: "No frozen model route has a bounded token-price reservation fitting the Goal cap".into() } }, now().map_err(|_| unavailable())?).await.map_err(|_| unavailable())?;
            return Err(unavailable());
        }
        self.plans
            .lock()
            .await
            .insert(plan.request_id.clone(), plan);
        Ok(NativePlanAdmission {
            route_indices: indices,
        })
    }
    async fn before_attempt(
        &self,
        request_id: &str,
        attempt_index: u32,
    ) -> bitrouter_sdk::Result<()> {
        self.before_attempt_with_token_reservation(request_id, attempt_index, None)
            .await
    }

    async fn before_attempt_with_token_reservation(
        &self,
        request_id: &str,
        attempt_index: u32,
        reservation: Option<&NativeTokenReservation>,
    ) -> bitrouter_sdk::Result<()> {
        let plans = self.plans.lock().await;
        let plan = plans.get(request_id).ok_or_else(unavailable)?;
        let route = plan
            .routes
            .get(attempt_index as usize)
            .ok_or_else(unavailable)?;
        self.begin_with_reservation(WorkKind::ProviderAttempt, &format!("{request_id}/attempt/{attempt_index}"), Some(request_id), serde_json::json!({"request_id":request_id,"attempt_index":attempt_index,"route":route}), reservation.cloned()).await
    }
    async fn before_provider_work(&self, work: &NativeProviderWork) -> bitrouter_sdk::Result<()> {
        self.begin(
            WorkKind::ProviderIntegration,
            &provider_key(work),
            Some(&work.request_id),
            serde_json::to_value(work).map_err(|_| unavailable())?,
        )
        .await
    }
    async fn after_provider_work(&self, report: NativeProviderWorkReport) {
        let Ok(value) = serde_json::to_value(&report) else {
            self.failed_ack.store(true, Ordering::Release);
            return;
        };
        if self
            .observe(&provider_key(&report.work), value, None)
            .await
            .is_err()
        {
            self.failed_ack.store(true, Ordering::Release);
        }
    }
    async fn after_attempt(&self, report: NativeAttemptReport) {
        let estimate = report.token_cost.estimated_micro_usd();
        let work_id = key_id(&format!(
            "{}/attempt/{}",
            report.request_id, report.attempt_index
        ));
        let Ok(time) = now() else {
            self.failed_ack.store(true, Ordering::Release);
            return;
        };
        let current = self
            .goals
            .execute(
                "local",
                GoalCommand::Get {
                    goal_id: self.goal.id.clone(),
                },
                time,
            )
            .await;
        let verified = match (&report.token_cost, current) {
            (
                NativeTokenCost::ConfiguredEstimate {
                    pricing_version,
                    pricing_provider,
                    pricing_model,
                    normalized_usage,
                    ..
                },
                Ok(GoalReply::Snapshot { goal }),
            ) => goal
                .budget
                .held
                .iter()
                .find(|held| held.work_id == work_id)
                .is_some_and(|held| {
                    let quote = &held.quote;
                    quote.pricing_version == *pricing_version
                        && quote.provider == *pricing_provider
                        && quote.model == *pricing_model
                        && normalized_usage
                            .uncached_input_tokens
                            .checked_add(normalized_usage.cache_read_tokens)
                            .and_then(|total| {
                                total.checked_add(normalized_usage.cache_write_tokens)
                            })
                            .is_some_and(|total| total <= quote.input_tokens_bound)
                        && normalized_usage
                            .output_tokens
                            .checked_add(normalized_usage.reasoning_tokens)
                            .is_some_and(|total| total <= quote.output_tokens_bound)
                }),
            _ => false,
        };
        let Ok(value) = serde_json::to_value(&report) else {
            self.failed_ack.store(true, Ordering::Release);
            return;
        };
        if self
            .observe_with_reservation(
                &format!("{}/attempt/{}", report.request_id, report.attempt_index),
                value,
                estimate,
                verified,
            )
            .await
            .is_err()
        {
            self.failed_ack.store(true, Ordering::Release);
        }
    }
}

fn unavailable() -> bitrouter_sdk::error::BitrouterError {
    bitrouter_sdk::error::BitrouterError::bad_request(
        "Goal model source/owner or durable acknowledgement is unavailable",
    )
}
fn key_id(key: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(key.as_bytes()))
}
fn prep_key(work: &NativePreparationWork) -> bitrouter_sdk::Result<String> {
    Ok(format!(
        "{}/prep/{}",
        work.request_id,
        serde_json::to_string(work).map_err(|_| unavailable())?
    ))
}
fn provider_key(work: &NativeProviderWork) -> String {
    format!(
        "{}/attempt/{}/integration/{}",
        work.request_id, work.attempt_index, work.work_index
    )
}
