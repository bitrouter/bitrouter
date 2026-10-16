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

pub(super) mod activity;
mod quote_wait;

pub(super) struct ModelControl {
    goals: Arc<GoalService>,
    goal: GoalSnapshot,
    coordinator_id: String,
    root: PathBuf,
    explicit_input: bool,
    generation: Option<bitrouter_orchestrator::turn::GenerationOrigin>,
    worker: Option<bitrouter_orchestrator::thread::GoalWork>,
    failed_ack: AtomicBool,
    failure: tokio::sync::Mutex<Option<String>>,
    plans: tokio::sync::Mutex<BTreeMap<String, NativePlan>>,
    activity: tokio::sync::Mutex<BTreeMap<String, activity::StreamActivity>>,
}

impl ModelControl {
    pub(super) fn new(
        coordinator: &Coordinator,
        goal: &GoalSnapshot,
        explicit_input: bool,
    ) -> Arc<Self> {
        Arc::new(Self::base(coordinator, goal, explicit_input))
    }

    pub(super) fn for_generation(
        coordinator: &Coordinator,
        goal: &GoalSnapshot,
        origin: bitrouter_orchestrator::turn::GenerationOrigin,
    ) -> Arc<Self> {
        Arc::new(Self {
            generation: Some(origin.clone()),
            ..Self::base(coordinator, goal, origin.purpose.is_input())
        })
    }

    pub(super) fn for_worker(
        coordinator: &Coordinator,
        goal: &GoalSnapshot,
        source: bitrouter_orchestrator::thread::GoalWork,
    ) -> Arc<Self> {
        Arc::new(Self {
            worker: Some(source),
            ..Self::base(coordinator, goal, false)
        })
    }

    fn base(coordinator: &Coordinator, goal: &GoalSnapshot, explicit_input: bool) -> Self {
        Self {
            goals: coordinator.goals.clone(),
            goal: goal.clone(),
            coordinator_id: coordinator.id.clone(),
            root: crate::goal_artifacts::root(&coordinator.workspaces, &goal.id),
            explicit_input,
            generation: None,
            worker: None,
            failed_ack: AtomicBool::new(false),
            failure: tokio::sync::Mutex::new(None),
            plans: tokio::sync::Mutex::new(BTreeMap::new()),
            activity: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    pub(super) fn acknowledged(&self) -> bool {
        !self.failed_ack.load(Ordering::Acquire)
    }

    pub(super) async fn failure_detail(&self) -> Option<String> {
        self.failure.lock().await.clone()
    }

    async fn fail_ack(&self, detail: &str) {
        let mut failure = self.failure.lock().await;
        if failure.is_none() {
            *failure = Some(detail.chars().take(2048).collect());
        }
        self.failed_ack.store(true, Ordering::Release);
    }

    pub(super) async fn begin_typed(
        &self,
        attempt_id: &str,
        request: &bitrouter_sdk::decision_model::types::DecisionRequest,
        pricing: Option<&bitrouter_sdk::decision_model::policy::DecisionPricing>,
    ) -> bitrouter_sdk::Result<()> {
        let quote = pricing.and_then(|pricing| typed_quote(&request.model, pricing));
        self.begin_with_reservation(
            WorkKind::TypedDecision,
            &format!("typed/{attempt_id}"),
            None,
            serde_json::to_value(request).map_err(|_| unavailable())?,
            quote,
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
        let work_id = key_id(&format!("typed/{attempt_id}"));
        let no_dispatch =
            matches!(outcome, Err(error) if !error.may_have_run && error.usage.is_none());
        let work = match self.intent(&work_id).await? {
            Some(work) => work,
            None if no_dispatch => {
                // The host rejected before saving an intent and Core confirms
                // it did not call the backend. There is no charge to settle.
                return Ok(());
            }
            None => return Err(unavailable()),
        };
        if work.id != work_id || work.kind != WorkKind::TypedDecision {
            return Err(unavailable());
        }
        // The immutable intent keeps its frozen quote even after the active
        // reservation and bounded recent-work projection have been retired.
        let quote = work.token_reservation.as_ref();
        let usage = match outcome {
            Ok(response) => Some(response.usage),
            Err(error) => error.usage,
        };

        let verified = no_dispatch
            || quote.is_some_and(|quote| {
                let Some(usage) = usage else { return false };
                let Ok(pricing) = serde_json::from_value::<
                    bitrouter_sdk::decision_model::policy::DecisionPricing,
                >(quote.evidence["pricing"].clone()) else {
                    return false;
                };
                typed_quote(&quote.model, &pricing).as_ref() == Some(quote)
                    && usage.input_tokens <= quote.input_tokens_bound
                    && usage.output_tokens <= quote.output_tokens_bound
                    && decision_estimate(&pricing, usage) == estimate
            });
        self.observe_with_reservation(
            &format!("typed/{attempt_id}"),
            serde_json::to_value(outcome).map_err(|_| unavailable())?,
            if no_dispatch { Some(0) } else { estimate },
            verified,
        )
        .await
    }

    pub(super) async fn gate(&self) -> bitrouter_sdk::Result<()> {
        if !self.acknowledged() {
            return Err(diagnostic(
                self.failure_detail()
                    .await
                    .as_deref()
                    .unwrap_or("previous outcome acknowledgement failed"),
            ));
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
            .map_err(|error| diagnostic(&format!("source snapshot unavailable: {error}")))?;
        let GoalReply::Snapshot { goal } = response else {
            return Err(unavailable());
        };
        let time = now().map_err(|_| unavailable())?;
        if self.generation.as_ref().is_some_and(|origin| {
            !bitrouter_orchestrator::goal::workflow::generation_requested(&goal, origin)
        }) {
            return Err(diagnostic(
                "the originating generation action is no longer pending",
            ));
        }
        if let Some(source) = &self.worker
            && !bitrouter_orchestrator::goal::workflow::worker_requested(&goal, source)
        {
            return Err(diagnostic(
                "the originating worker attempt is no longer current",
            ));
        }
        if self.worker.is_none() && goal.requirements_revision != self.goal.requirements_revision {
            return Err(diagnostic(&format!(
                "requirements changed from {} to {}",
                self.goal.requirements_revision, goal.requirements_revision
            )));
        }
        if let Err(error) = goal.budget.admission(goal.created_at_ms, time) {
            return Err(diagnostic(&format!("budget admission rejected: {error}")));
        }
        if !self.explicit_input && goal.run_intent != RunIntent::Run {
            return Err(diagnostic("background work was paused or stopped"));
        }
        if !self.explicit_input
            && goal
                .workflow
                .completion
                .completed_for(goal.requirements_revision)
        {
            return Err(diagnostic("this requirements version is already completed"));
        }
        match &goal.workflow.coordinator {
            None => return Err(diagnostic("coordinator lease is missing")),
            Some(lease) if lease.id != self.coordinator_id => {
                return Err(diagnostic("coordinator lease belongs to a different owner"));
            }
            Some(lease) if lease.expires_at_ms <= time => {
                return Err(diagnostic("coordinator lease expired"));
            }
            Some(_) => {}
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

    async fn intent(&self, id: &str) -> bitrouter_sdk::Result<Option<Work>> {
        let reply = self
            .goals
            .execute(
                "local",
                GoalCommand::Operation {
                    operation_id: format!("{id}-begin"),
                },
                now().map_err(|_| unavailable())?,
            )
            .await;
        let operation = match reply {
            Ok(GoalReply::Accepted { operation }) => operation,
            Err(bitrouter_orchestrator::goal::GoalError::NotFound) => return Ok(None),
            _ => return Err(unavailable()),
        };
        if operation.goal_id != self.goal.id {
            return Err(unavailable());
        }
        let bitrouter_orchestrator::goal::OperationKind::Workflow { change } = operation.kind
        else {
            return Err(unavailable());
        };
        let Change::Accounting {
            event: Event::Begin { work },
        } = *change
        else {
            return Err(unavailable());
        };
        if work.id != id {
            return Err(unavailable());
        }
        Ok(Some(work))
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
        let requirements_revision = self
            .intent(&id)
            .await?
            .map_or(self.goal.requirements_revision, |work| {
                work.requirements_revision
            });
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
                    source: self.worker.clone().map(Box::new),
                    id,
                    kind,
                    requirements_revision,
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
        // Recreated controls may observe a newer, independent Goal edit. The
        // original work revision still owns this action's immutable receipts.
        let requirements_revision = match &event {
            Event::Begin { work } => work.requirements_revision,
            Event::Observe { id, .. } => {
                self.intent(id)
                    .await?
                    .ok_or_else(unavailable)?
                    .requirements_revision
            }
        };
        let provider_attempt =
            matches!(&event, Event::Begin { work } if work.kind == WorkKind::ProviderAttempt);
        let change = Change::Accounting { event };
        let result = self
            .commit_fact(operation, requirements_revision, change.clone())
            .await;
        let result = self
            .wait_for_quote(operation, requirements_revision, &change, result)
            .await?;
        if matches!(
            &result,
            Err(bitrouter_orchestrator::goal::GoalError::BudgetExceeded { .. })
        ) && provider_attempt
            && self.worker.is_none()
        {
            let _ = self.goals.workflow_change(WorkflowActor { owner: "local", coordinator_id: &self.coordinator_id },
                &self.goal.id, &format!("{operation}-budget"), self.goal.requirements_revision,
                Change::Budget { event: bitrouter_orchestrator::goal::budget::Event::Halt { reason: "A provider attempt no longer fits the shared Goal budget; no dispatch occurred".into() } },
                now().map_err(|_| unavailable())?).await;
        }
        if let Err(error) = &result
            && !(self.worker.is_some()
                && matches!(
                    error,
                    bitrouter_orchestrator::goal::GoalError::BudgetExceeded { .. }
                ))
        {
            self.fail_ack(&format!(
                "accounting operation {operation} was not acknowledged: {error}"
            ))
            .await;
        }
        result.map(|_| ()).map_err(|error| {
            diagnostic(&format!(
                "accounting operation {operation} was not acknowledged: {error}"
            ))
        })?;
        if matches!(
            change,
            Change::Accounting {
                event: Event::Begin { .. }
            }
        ) {
            // A recovered receipt confirms a past intent, not current authority
            // to dispatch after a pause, source change or ownership transfer.
            self.gate().await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl bitrouter_orchestrator::control::WorkControl for ModelControl {
    async fn authorize(&self) -> bitrouter_sdk::Result<()> {
        self.gate().await
    }
    fn outcome_acknowledged(&self) -> bool {
        self.acknowledged()
    }
}

#[async_trait::async_trait]
impl NativeExecutionControl for ModelControl {
    async fn on_stream_part(&self, request_id: &str, part: &bitrouter_ai::types::StreamPart) {
        self.record_content(request_id, part).await;
    }

    async fn before_typed_decision(
        &self,
        attempt_id: &str,
        request: &bitrouter_sdk::decision_model::types::DecisionRequest,
        pricing: Option<&bitrouter_sdk::decision_model::policy::DecisionPricing>,
    ) -> bitrouter_sdk::Result<()> {
        self.begin_typed(attempt_id, request, pricing).await
    }

    async fn after_typed_decision(
        &self,
        attempt_id: &str,
        outcome: &std::result::Result<
            bitrouter_sdk::decision_model::types::DecisionResponse,
            bitrouter_sdk::decision_model::types::DecisionError,
        >,
        pricing: Option<&bitrouter_sdk::decision_model::policy::DecisionPricing>,
    ) -> bitrouter_sdk::Result<()> {
        let usage = match outcome {
            Ok(response) => Some(response.usage),
            Err(error) => error.usage,
        };
        self.observe_typed(
            attempt_id,
            outcome,
            pricing.and_then(|pricing| usage.and_then(|usage| decision_estimate(pricing, usage))),
        )
        .await
    }

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
            &count_key(plan, route_index)?,
            Some(&plan.request_id),
            serde_json::json!({"plan":plan,"route_index":route_index}),
        )
        .await?;
        self.plans
            .lock()
            .await
            .insert(plan.request_id.clone(), plan.clone());
        Ok(())
    }
    async fn after_input_count(&self, report: NativeInputCountReport) -> bitrouter_sdk::Result<()> {
        let key = {
            let plans = self.plans.lock().await;
            count_key(
                plans.get(&report.request_id).ok_or_else(unavailable)?,
                report.route_index,
            )?
        };
        self.observe(
            &key,
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
        let key = format!("{}/plan/{}", plan.request_id, plan_digest(&plan)?);
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
                let quote = reservations.get(*index).and_then(Option::as_ref);
                if self.worker.is_some() {
                    bitrouter_orchestrator::goal::budget::waiting_on_workers(&goal, quote).is_ok()
                } else {
                    goal.budget
                        .can_reserve(goal.accounting.estimated_model_tokens_micro_usd, quote)
                        .is_ok()
                }
            })
            .map(u32::try_from)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| unavailable())?;
        // Replayed admission retains its original routes and quotes even if
        // another worker has since settled or consumed budget.
        let indices = match self.plan_outcome(&key).await? {
            Some(saved) => {
                if saved["token_reservations"]
                    != serde_json::to_value(&reservations).map_err(|_| unavailable())?
                {
                    return Err(diagnostic("frozen plan reservation changed on replay"));
                }
                serde_json::from_value::<Vec<u32>>(saved["accepted_routes"].clone())
                    .map_err(|_| unavailable())?
            }
            None => {
                self.observe(&key, serde_json::json!({"accepted_routes":indices,"token_reservations":reservations}), None).await?;
                indices
            }
        };
        if indices.is_empty() {
            if self.worker.is_some() {
                return Err(diagnostic(
                    "worker reservation does not currently fit the shared Goal budget; no provider dispatch occurred",
                ));
            }
            self.goals.workflow_change(WorkflowActor { owner: "local", coordinator_id: &self.coordinator_id },
                &self.goal.id, &format!("{}-budget", key_id(&key)), goal.requirements_revision,
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
        let plan = self
            .plans
            .lock()
            .await
            .get(request_id)
            .cloned()
            .ok_or_else(unavailable)?;
        let route = plan
            .routes
            .get(attempt_index as usize)
            .ok_or_else(unavailable)?;
        let saved = self
            .plan_outcome(&format!("{request_id}/plan/{}", plan_digest(&plan)?))
            .await?
            .ok_or_else(unavailable)?;
        let frozen_quote = saved["token_reservations"]
            .get(attempt_index as usize)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if frozen_quote != serde_json::to_value(reservation).map_err(|_| unavailable())? {
            return Err(diagnostic("attempt quote differs from its frozen plan"));
        }
        self.begin_with_reservation(WorkKind::ProviderAttempt, &format!("{request_id}/attempt/{attempt_index}"), Some(request_id), serde_json::json!({"request_id":request_id,"attempt_index":attempt_index,"route":route}), reservation.cloned()).await
    }
    async fn admission_aborted(&self, request_id: &str, attempt_index: u32) {
        if let Err(error) = self.abort_admission(request_id, attempt_index).await {
            self.fail_ack(&format!(
                "undispatched admission could not be reconciled: {error}"
            ))
            .await;
        }
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
            self.fail_ack("provider integration outcome could not be serialized")
                .await;
            return;
        };
        if self
            .observe(&provider_key(&report.work), value, None)
            .await
            .is_err()
        {
            self.fail_ack("provider integration outcome was not acknowledged")
                .await;
        }
    }
    async fn after_attempt(&self, report: NativeAttemptReport) {
        match self.replay_attempt(&report).await {
            Ok(true) => return,
            Ok(false) => {}
            Err(_) => {
                self.fail_ack("original provider attempt outcome could not be confirmed")
                    .await;
                return;
            }
        }
        let estimate = report.token_cost.estimated_micro_usd();
        let work_id = key_id(&format!(
            "{}/attempt/{}",
            report.request_id, report.attempt_index
        ));
        let Ok(time) = now() else {
            self.fail_ack("provider outcome clock unavailable").await;
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
        let Ok(mut value) = serde_json::to_value(&report) else {
            self.fail_ack("provider attempt outcome could not be serialized")
                .await;
            return;
        };
        if let Some(activity) = self.activity.lock().await.remove(&report.request_id) {
            let Ok(observed) = serde_json::to_value(activity) else {
                self.fail_ack("provider stream observations could not be serialized")
                    .await;
                return;
            };
            value["bro_stream_activity"] = observed;
        }
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
            self.fail_ack("provider attempt outcome was not acknowledged")
                .await;
        }
    }
}

fn unavailable() -> bitrouter_sdk::error::BitrouterError {
    bitrouter_sdk::error::BitrouterError::bad_request(
        "Goal model source/owner or durable acknowledgement is unavailable",
    )
}
fn diagnostic(detail: &str) -> bitrouter_sdk::error::BitrouterError {
    bitrouter_sdk::error::BitrouterError::bad_request(format!(
        "Goal model {}",
        detail.chars().take(2048).collect::<String>()
    ))
}
fn key_id(key: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(key.as_bytes()))
}
fn plan_digest(plan: &NativePlan) -> bitrouter_sdk::Result<String> {
    serde_json::to_string(plan)
        .map(|value| key_id(&value))
        .map_err(|_| unavailable())
}
fn count_key(plan: &NativePlan, route: u32) -> bitrouter_sdk::Result<String> {
    Ok(format!(
        "{}/count/{route}/{}",
        plan.request_id,
        plan_digest(plan)?
    ))
}
fn decision_estimate(
    pricing: &bitrouter_sdk::decision_model::policy::DecisionPricing,
    usage: bitrouter_sdk::decision_model::types::DecisionUsage,
) -> Option<u64> {
    let amount = pricing.estimate(usage) * 1_000_000.0;
    if !amount.is_finite() || amount < 0.0 {
        return None;
    }
    format!("{:.0}", amount.ceil()).parse().ok()
}
fn typed_quote(
    model: &str,
    pricing: &bitrouter_sdk::decision_model::policy::DecisionPricing,
) -> Option<NativeTokenReservation> {
    pricing.validate().ok()?;
    let bound = |tokens: Option<u64>, rate: f64| {
        if rate == 0.0 {
            Some(tokens.unwrap_or(u64::MAX))
        } else {
            tokens
        }
    };
    let input = bound(pricing.max_input_tokens, pricing.input_usd_per_million)?;
    let output = bound(pricing.max_output_tokens, pricing.output_usd_per_million)?;
    let evidence = serde_json::json!({"pricing":pricing,"model":model,"basis":"operator_declared_aggregate_token_estimate"});
    let micro_usd = decision_estimate(
        pricing,
        bitrouter_sdk::decision_model::types::DecisionUsage {
            input_tokens: input,
            output_tokens: output,
        },
    )?;
    Some(NativeTokenReservation {
        micro_usd,
        pricing_version: key_id(&serde_json::to_string(&evidence).ok()?),
        provider: "decision_model".into(),
        model: model.into(),
        input_tokens_bound: input,
        output_tokens_bound: output,
        evidence,
    })
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

#[cfg(test)]
mod tests;
