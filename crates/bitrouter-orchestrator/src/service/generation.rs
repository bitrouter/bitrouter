//! Tool-free Goal generation uses the same admission, owner and Turn journal.

use super::{ErrorCode, ServiceError, ThreadService};
use crate::control::WorkControl;
use crate::store::{ExecutionRecord, SettlementOutcome};
use crate::thread::ThreadTarget;
use crate::turn::{
    GenerationOrigin, TurnEventPayload, TurnReceipt, TurnRequest, TurnStatus, VerificationStatus,
};
use bitrouter_ai::types::{Content, Message, Prompt, Role, StreamPart};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::native::{
    NativeAttemptReport, NativeContextValidationReport, NativeExecutionControl,
    NativeInputCountReport, NativeModelSelection, NativePlan, NativePlanAdmission, NativeRoute,
};
use bitrouter_sdk::language_model::native_accounting::NativeTokenReservation;
use bitrouter_sdk::language_model::native_preparation::{
    NativePreparationWork, NativePreparationWorkReport,
};
use bitrouter_sdk::language_model::native_work::{NativeProviderWork, NativeProviderWorkReport};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct GenerationRequest {
    pub origin: GenerationOrigin,
    pub prompt: Prompt,
}

/// One action, independent of the original user operation or another model stage.
pub fn admission_key(origin: &GenerationOrigin) -> Result<String, ServiceError> {
    super::admission::fingerprint(origin).map(|hash| format!("goal-model-{hash}"))
}

#[derive(Clone)]
pub(super) struct Job {
    pub(super) request: GenerationRequest,
    pub(super) control: Arc<dyn WorkControl>,
}

impl GenerationRequest {
    pub(super) fn input(&self) -> Result<TurnRequest, ServiceError> {
        Ok(TurnRequest {
            prompt: format!(
                "Goal {} / {:?} / source {} / requirements {}",
                self.origin.goal_id,
                self.origin.purpose,
                self.origin.source_id,
                self.origin.requirements_revision
            ),
            idempotency_key: admission_key(&self.origin)?,
        })
    }
}

impl ThreadService {
    /// Trusted Goal assembly supplies both the causal source and its existing
    /// live budget/permission control. Native transports do not expose this RPC.
    pub async fn start_goal_generation(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        request: GenerationRequest,
        control: Arc<dyn WorkControl>,
    ) -> Result<TurnReceipt, ServiceError> {
        crate::goal::identifier(&request.origin.goal_id).map_err(|error| error.to_string())?;
        crate::goal::identifier(&request.origin.source_id).map_err(|error| error.to_string())?;
        if request.origin.requirements_revision == 0
            || !request.prompt.tools.is_empty()
            || request.prompt.tool_choice.is_some()
            || request.prompt.stream
            || request.prompt.messages.is_empty()
        {
            return Err(
                "managed generation needs a versioned, tool-free, non-streaming input".into(),
            );
        }
        self.initialize_execution().await?;
        let view = self.load_thread(target, caller).await?;
        if let Some(recovery) = &view.recovery {
            self.recover_thread(
                target,
                caller,
                crate::thread::ThreadRecoveryRequest {
                    source_server_instance_id: recovery.source_server_instance_id.clone(),
                    source_cursor: recovery.source_cursor,
                    idempotency_key: format!("{}-restore", admission_key(&request.origin)?),
                },
            )
            .await?;
        }
        let owner = if let Some(parent) = &view.thread.input_for_thread_id {
            self.read_stored_thread_view(
                &ThreadTarget {
                    thread_id: parent.clone(),
                    server_instance_id: target.server_instance_id.clone(),
                },
                caller,
            )
            .await?
            .thread
        } else {
            view.thread.clone()
        };
        if owner.owned_goal_id.as_deref() != Some(request.origin.goal_id.as_str())
            || request.origin.purpose.is_input() != view.thread.input_for_thread_id.is_some()
            || request.prompt.model != view.thread.model
        {
            return Err(ServiceError::new(
                ErrorCode::Unauthorized,
                "generation source does not match this Goal Thread",
            ));
        }
        let input = request.input()?;
        self.admit_turn(
            target,
            caller,
            input,
            true,
            true,
            Some(super::worker::Managed::Generation(Box::new(Job {
                request,
                control,
            }))),
        )
        .await
    }

    /// Resolve an interrupted association without supplying a changed prompt,
    /// starting a worker, or manufacturing a new action key.
    pub async fn find_generation_admission(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        origin: &GenerationOrigin,
    ) -> Result<Option<TurnReceipt>, ServiceError> {
        self.read_stored_thread_view(target, caller).await?;
        let key = admission_key(origin)?;
        let scope =
            super::admission::key_scope(caller, Some(&target.thread_id), "generation", &key)?;
        let Some(entry) = self
            .inner
            .store
            .find_key(&scope, &key)
            .await
            .map_err(ServiceError::storage)?
        else {
            return Ok(None);
        };
        self.existing_turn_receipt(&entry).await.map(Some)
    }

    pub(super) async fn generation_gate(&self, turn_id: &str) -> bitrouter_sdk::Result<()> {
        {
            let state = self.lock_state();
            let Some(turn) = state.turns.get(turn_id) else {
                return Err(closed());
            };
            if state.closing
                || turn.cancel.is_cancelled()
                || turn.storage_error.is_some()
                || turn.snapshot.generation.is_none()
                || turn.snapshot.status.terminal()
            {
                return Err(closed());
            }
        }
        // The database owner/version fence is checked at actual SDK dispatch,
        // including counters, callbacks and fallback attempts.
        self.commit_records(turn_id, &[ExecutionRecord::CoreDispatch])
            .await
            .map_err(|_| closed())
    }

    pub(super) async fn run_generation_inner(
        &self,
        turn_id: String,
        job: Job,
        cancel: CancellationToken,
    ) -> Result<(), ServiceError> {
        self.append(&turn_id, TurnEventPayload::Started).await?;
        let started = Instant::now();
        let (version, caller, duration, selection) = {
            let state = self.lock_state();
            let turn = state.turns.get(&turn_id).ok_or_else(super::unknown_turn)?;
            let thread = state
                .threads
                .get(&turn.thread_id)
                .ok_or_else(super::threads::unknown_thread)?;
            (
                thread.snapshot.context_version,
                thread.caller.clone(),
                thread.config.max_duration(),
                if thread.config.native()?.model_mode == crate::core::protocol::ModelMode::Fixed {
                    NativeModelSelection::Fixed
                } else {
                    NativeModelSelection::Policy
                },
            )
        };
        let step_id = format!("{turn_id}-model");
        let item_id = format!("{turn_id}-answer");
        self.commit_records(
            &turn_id,
            &[ExecutionRecord::ModelRequest {
                step_id: step_id.clone(),
                item_id: item_id.clone(),
                context_version: version,
                prompt: Box::new(job.request.prompt.clone()),
            }],
        )
        .await?;
        let control = Arc::new(Control {
            service: self.clone(),
            turn_id: turn_id.clone(),
            parent: job.control,
            cancel: cancel.clone(),
            failed: AtomicBool::new(false),
            selection,
        });
        let deadline = control.cancel.clone();
        let deadline_task = tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            deadline.cancel();
        });
        let requested_model = job.request.prompt.model.clone();
        let response = self
            .inner
            .app
            .execute_native_controlled(job.request.prompt, caller, control.clone())
            .await;
        deadline_task.abort();
        let _ = deadline_task.await;
        let outcome = match response {
            Ok(response)
                if !control.failed.load(Ordering::Acquire)
                    && !cancel.is_cancelled()
                    && control.parent.outcome_acknowledged() =>
            {
                let answer = response
                    .result
                    .generation()
                    .ok_or_else(|| "generation driver received a non-generation result".to_string())
                    .and_then(|generation| {
                        let text = generation
                            .content
                            .iter()
                            .filter_map(|part| match part {
                                Content::Text { text, .. } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<String>();
                        if text.trim().is_empty()
                            || generation
                                .content
                                .iter()
                                .any(|part| matches!(part, Content::ToolCall { .. }))
                        {
                            return Err(
                                "tool-free generation did not return an admitted text answer"
                                    .into(),
                            );
                        }
                        Ok((text, generation.usage.clone()))
                    });
                match answer {
                    Ok((text, usage)) => {
                        self.commit_records(
                            &turn_id,
                            &[ExecutionRecord::ModelResponse {
                                step_id: step_id.clone(),
                                item_id: item_id.clone(),
                                request_id: response.request_id,
                                requested_model,
                                usage,
                                estimated_spend_microusd: 0,
                                message: Message::text(Role::Assistant, text.clone()),
                                calls: Vec::new(),
                            }],
                        )
                        .await?;
                        Ok(text)
                    }
                    Err(detail) => Err(detail),
                }
            }
            Ok(_) if cancel.is_cancelled() => Err("managed generation was cancelled".into()),
            Ok(_) => Err("generation execution evidence was not durably acknowledged".into()),
            Err(error) => Err(error.to_string()),
        };
        let (status, answer, detail, messages) = match outcome {
            Ok(text) => (
                TurnStatus::Completed,
                Some(text.clone()),
                "Tool-free Goal generation completed; adoption and Goal validation remain separate"
                    .to_string(),
                vec![Message::text(Role::Assistant, text)],
            ),
            Err(detail) => {
                self.commit_records(
                    &turn_id,
                    &[ExecutionRecord::ModelInterrupted {
                        step_id,
                        item_id,
                        request_id: None,
                        usage: None,
                        partial: Message::text(Role::Assistant, ""),
                        detail: detail.clone(),
                    }],
                )
                .await?;
                (
                    if cancel.is_cancelled() {
                        TurnStatus::Cancelled
                    } else {
                        TurnStatus::Failed
                    },
                    None,
                    detail.chars().take(2048).collect(),
                    Vec::new(),
                )
            }
        };
        self.commit_records(
            &turn_id,
            &[ExecutionRecord::Settled {
                outcome: Some(SettlementOutcome {
                    status: if status == TurnStatus::Completed {
                        crate::agent::RunStatus::Completed
                    } else if status == TurnStatus::Cancelled {
                        crate::agent::RunStatus::Cancelled
                    } else {
                        crate::agent::RunStatus::Failed
                    },
                    final_answer: answer.clone(),
                    detail: detail.clone(),
                }),
                context_version: version.saturating_add(1),
                messages,
                model_steps: 1,
                tool_calls: 0,
                estimated_spend_microusd: 0,
                active_duration_ms: u64::try_from(started.elapsed().as_millis())
                    .unwrap_or(u64::MAX),
            }],
        )
        .await?;
        self.append(
            &turn_id,
            TurnEventPayload::Finished {
                status,
                detail,
                final_answer: answer,
                verification: VerificationStatus::NotRequested,
                verification_evidence: None,
                unknown_effect: false,
            },
        )
        .await?;
        Ok(())
    }
}

fn closed() -> bitrouter_sdk::error::BitrouterError {
    bitrouter_sdk::error::BitrouterError::bad_request("managed generation source/owner is closed")
}

struct Control {
    service: ThreadService,
    turn_id: String,
    parent: Arc<dyn WorkControl>,
    cancel: CancellationToken,
    failed: AtomicBool,
    selection: NativeModelSelection,
}

impl Control {
    async fn record_plan(
        &self,
        plan: NativePlan,
        admission: &NativePlanAdmission,
    ) -> bitrouter_sdk::Result<()> {
        // Shared prompt preparation may rewrite a request. It must not turn
        // this explicit text-generation driver into an agent/tool loop.
        if !plan.prompt.tools.is_empty() || plan.prompt.tool_choice.is_some() || plan.prompt.stream
        {
            return Err(bitrouter_sdk::error::BitrouterError::bad_request(
                "managed generation preparation introduced tools or streaming",
            ));
        }
        self.service
            .commit_records(
                &self.turn_id,
                &[ExecutionRecord::GenerationPlan {
                    plan: Box::new(plan),
                    route_indices: admission.route_indices.clone(),
                }],
            )
            .await
            .map_err(|_| closed())?;
        self.gate().await
    }
    async fn gate(&self) -> bitrouter_sdk::Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(closed());
        }
        self.parent.authorize().await?;
        self.service.generation_gate(&self.turn_id).await
    }
}

#[async_trait::async_trait]
impl NativeExecutionControl for Control {
    async fn before_typed_decision(
        &self,
        attempt_id: &str,
        request: &bitrouter_sdk::decision_model::types::DecisionRequest,
        pricing: Option<&bitrouter_sdk::decision_model::policy::DecisionPricing>,
    ) -> bitrouter_sdk::Result<()> {
        self.gate().await?;
        self.service
            .commit_records(
                &self.turn_id,
                &[ExecutionRecord::GenerationDecisionIntent {
                    attempt_id: attempt_id.into(),
                    request: Box::new(request.clone()),
                    pricing: pricing.cloned(),
                }],
            )
            .await
            .map_err(|_| closed())?;
        self.parent
            .before_typed_decision(attempt_id, request, pricing)
            .await?;
        self.gate().await
    }

    async fn after_typed_decision(
        &self,
        attempt_id: &str,
        outcome: &Result<
            bitrouter_sdk::decision_model::types::DecisionResponse,
            bitrouter_sdk::decision_model::types::DecisionError,
        >,
        pricing: Option<&bitrouter_sdk::decision_model::policy::DecisionPricing>,
    ) -> bitrouter_sdk::Result<()> {
        let recorded = self
            .service
            .commit_records(
                &self.turn_id,
                &[ExecutionRecord::GenerationDecisionOutcome {
                    attempt_id: attempt_id.into(),
                    outcome: Box::new(outcome.clone()),
                }],
            )
            .await;
        if recorded.is_err() {
            self.failed.store(true, Ordering::Release);
        }
        // A Thread ACK failure must not suppress the root Goal's usage record.
        let host = self
            .parent
            .after_typed_decision(attempt_id, outcome, pricing)
            .await;
        recorded.map_err(|_| closed()).and(host)?;
        if !self.parent.outcome_acknowledged() {
            self.failed.store(true, Ordering::Release);
            return Err(closed());
        }
        Ok(())
    }

    async fn provider_cancelled(&self) {
        tokio::select! {
            () = self.cancel.cancelled() => {},
            () = self.parent.provider_cancelled() => {},
        }
    }
    fn model_selection(&self) -> NativeModelSelection {
        self.selection
    }
    async fn after_attempt(&self, report: NativeAttemptReport) {
        if self
            .service
            .commit_records(
                &self.turn_id,
                &[ExecutionRecord::GenerationAttempt {
                    report: Box::new(report.clone()),
                }],
            )
            .await
            .is_err()
        {
            self.failed.store(true, Ordering::Release);
        }
        // Settlement/usage reporting must still run after our own ACK failure.
        self.parent.after_attempt(report).await;
    }
    async fn prepare_routing(
        &self,
        prompt: &Prompt,
    ) -> bitrouter_sdk::Result<Option<bitrouter_sdk::routing::preparation::Prepared>> {
        self.gate().await?;
        let prepared = self.parent.prepare_routing(prompt).await?;
        self.gate().await?;
        Ok(prepared)
    }
    async fn commit_routing(
        &self,
        plan: &bitrouter_sdk::routing::plan::Plan,
    ) -> bitrouter_sdk::Result<()> {
        self.gate().await?;
        self.parent.commit_routing(plan).await?;
        self.gate().await
    }
    fn observe_stream(&self) -> bool {
        self.parent.observe_stream()
    }
    async fn on_stream_part(&self, request_id: &str, part: &StreamPart) {
        self.parent.on_stream_part(request_id, part).await
    }
    fn provider_response_byte_limit(&self) -> Option<u64> {
        Some(
            self.parent
                .provider_response_byte_limit()
                .unwrap_or(2 * 1024 * 1024)
                .min(2 * 1024 * 1024),
        )
    }
    fn canonical_output_byte_limit(&self) -> Option<u64> {
        Some(
            self.parent
                .canonical_output_byte_limit()
                .unwrap_or(1024 * 1024)
                .min(1024 * 1024),
        )
    }
    fn attempt_report_byte_limit(
        &self,
        request_id: &str,
        route: &NativeRoute,
    ) -> bitrouter_sdk::Result<Option<u64>> {
        Ok(Some(
            self.parent
                .attempt_report_byte_limit(request_id, route)?
                .unwrap_or(2 * 1024 * 1024)
                .min(2 * 1024 * 1024),
        ))
    }
    fn auxiliary_report_byte_limit(&self, request_id: &str) -> bitrouter_sdk::Result<Option<u64>> {
        self.parent.auxiliary_report_byte_limit(request_id)
    }
    async fn before_preparation_work(
        &self,
        work: &NativePreparationWork,
    ) -> bitrouter_sdk::Result<()> {
        self.gate().await?;
        self.parent.before_preparation_work(work).await?;
        self.gate().await
    }
    async fn after_preparation_work(
        &self,
        report: NativePreparationWorkReport,
    ) -> bitrouter_sdk::Result<()> {
        self.parent.after_preparation_work(report).await
    }
    async fn before_input_count(
        &self,
        plan: &NativePlan,
        route_index: u32,
    ) -> bitrouter_sdk::Result<()> {
        self.gate().await?;
        self.parent.before_input_count(plan, route_index).await?;
        self.gate().await
    }
    async fn after_input_count(&self, report: NativeInputCountReport) -> bitrouter_sdk::Result<()> {
        self.parent.after_input_count(report).await
    }
    async fn before_context_validation(&self, request_id: &str) -> bitrouter_sdk::Result<()> {
        self.gate().await?;
        self.parent.before_context_validation(request_id).await?;
        self.gate().await
    }
    async fn check_context_validation(&self, request_id: &str) -> bitrouter_sdk::Result<()> {
        self.gate().await?;
        self.parent.check_context_validation(request_id).await?;
        self.gate().await
    }
    fn context_validation_gate_duration(&self) -> Option<std::time::Duration> {
        self.parent.context_validation_gate_duration()
    }
    async fn validate_context_rebuild(
        &self,
        original: &Prompt,
        rebuilt: &Prompt,
        request_id: &str,
    ) -> bitrouter_sdk::Result<()> {
        self.gate().await?;
        self.parent
            .validate_context_rebuild(original, rebuilt, request_id)
            .await?;
        self.gate().await
    }
    async fn after_context_validation(
        &self,
        report: NativeContextValidationReport,
    ) -> bitrouter_sdk::Result<()> {
        self.parent.after_context_validation(report).await
    }
    async fn plan(&self, plan: NativePlan) -> bitrouter_sdk::Result<NativePlanAdmission> {
        self.gate().await?;
        let admission = self.parent.plan(plan.clone()).await?;
        self.record_plan(plan, &admission).await?;
        Ok(admission)
    }
    async fn plan_with_token_reservations(
        &self,
        plan: NativePlan,
        reservations: Vec<Option<NativeTokenReservation>>,
    ) -> bitrouter_sdk::Result<NativePlanAdmission> {
        self.gate().await?;
        let admission = self
            .parent
            .plan_with_token_reservations(plan.clone(), reservations)
            .await?;
        self.record_plan(plan, &admission).await?;
        Ok(admission)
    }
    async fn rebuild_context(
        &self,
        plan: &NativePlan,
    ) -> bitrouter_sdk::Result<Option<Vec<Message>>> {
        self.gate().await?;
        let messages = self.parent.rebuild_context(plan).await?;
        self.gate().await?;
        Ok(messages)
    }
    async fn before_attempt(
        &self,
        request_id: &str,
        attempt_index: u32,
    ) -> bitrouter_sdk::Result<()> {
        self.gate().await?;
        self.parent
            .before_attempt(request_id, attempt_index)
            .await?;
        self.gate().await
    }
    async fn before_attempt_with_token_reservation(
        &self,
        request_id: &str,
        attempt_index: u32,
        reservation: Option<&NativeTokenReservation>,
    ) -> bitrouter_sdk::Result<()> {
        self.gate().await?;
        self.parent
            .before_attempt_with_token_reservation(request_id, attempt_index, reservation)
            .await?;
        self.gate().await
    }
    async fn before_provider_work(&self, work: &NativeProviderWork) -> bitrouter_sdk::Result<()> {
        self.gate().await?;
        self.parent.before_provider_work(work).await?;
        self.gate().await
    }
    async fn after_provider_work(&self, report: NativeProviderWorkReport) {
        self.parent.after_provider_work(report).await
    }
}
