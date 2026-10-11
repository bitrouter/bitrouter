//! Auxiliary callback admission before the model plan is frozen.

use super::*;
use bitrouter_sdk::language_model::native_preparation::{
    NativePreparationWork, NativePreparationWorkKind, NativePreparationWorkReport,
};

pub(super) fn validate_preparation(step: &ModelStep, request_id: &str) -> Result<(), CoreError> {
    if step.preparation_work.iter().any(|record| {
        record.work.request_id != request_id
            || record
                .report
                .as_ref()
                .is_none_or(|report| report.error_code.is_some())
    }) {
        return Err(reject(
            ErrorCode::OperationConflict,
            "plan does not match acknowledged preparation",
        ));
    }
    Ok(())
}

impl StepControl {
    pub(super) async fn admit_preparation_work(
        &self,
        work: &NativePreparationWork,
    ) -> bitrouter_sdk::Result<()> {
        let step_id = self.step_id.lock().await.clone();
        self.session
            .transition_for(
                Some(&self.agent_id),
                "preparation.work.intent",
                |state, _| {
                    validate_step_source(state, &self.agent_id, &step_id)?;
                    let run = active_run(state)?;
                    if !matches!(run.status, RunStatus::Running | RunStatus::Waiting)
                        || run.active_ms >= run.limits.active_seconds.saturating_mul(1000)
                        || run.model_attempts >= run.limits.model_attempts
                    {
                        return Err(reject(
                            ErrorCode::LimitExceeded,
                            "preparation is no longer admitted",
                        ));
                    }
                    if agent_turn(state, &self.agent_id)?.status != AgentStatus::ModelRunning {
                        return Err(reject(ErrorCode::Busy, "preparation turn changed"));
                    }
                    let step = current_step(state, &self.agent_id, &step_id)?;
                    if step.preparation_work.len() >= 256 {
                        return Err(reject(
                            ErrorCode::LimitExceeded,
                            "preparation callback limit reached",
                        ));
                    }
                    let app = work.kind == NativePreparationWorkKind::PromptTransform;
                    let expected = step
                        .preparation_work
                        .iter()
                        .filter(|record| {
                            (record.work.kind == NativePreparationWorkKind::PromptTransform) == app
                        })
                        .count();
                    if step.reconstructed_from.is_some()
                        || step.plan.is_some()
                        || step.count_plan.is_some()
                        || step.settled
                        || !step.attempts.is_empty()
                        || work.request_id.is_empty()
                        || work.work_index as usize != expected
                        || step.preparation_work.iter().any(|record| {
                            record.work.request_id != work.request_id
                                || record.report.is_none()
                                || (app
                                    && record.work.kind
                                        != NativePreparationWorkKind::PromptTransform)
                        })
                    {
                        return Err(reject(
                            ErrorCode::OperationConflict,
                            "preparation work is not the next bounded callback",
                        ));
                    }
                    step.preparation_work.push(PreparationWorkRecord {
                        work: work.clone(),
                        report: None,
                    });
                    encode(work)
                },
            )
            .await
            .map_err(sdk_error)?;
        self.session
            .ensure_dispatch_with_budget(
                &self.agent_id,
                &step_id,
                format!("{}/preparation", work.request_id),
                true,
            )
            .await
            .map_err(sdk_error)
    }

    pub(super) async fn record_preparation_work(
        &self,
        report: NativePreparationWorkReport,
    ) -> bitrouter_sdk::Result<()> {
        let step_id = self.step_id.lock().await.clone();
        let active_ms = self
            .session
            .shared
            .live
            .lock()
            .await
            .activity
            .finish(&format!("{}/preparation", report.work.request_id));
        let recorded = self
            .session
            .transition_for(
                Some(&self.agent_id),
                "preparation.work.outcome",
                |state, _| {
                    let step = current_step(state, &self.agent_id, &step_id)?;
                    let record = step
                        .preparation_work
                        .last_mut()
                        .filter(|record| record.work == report.work && record.report.is_none())
                        .ok_or_else(|| {
                            reject(
                                ErrorCode::OperationConflict,
                                "preparation outcome has no pending intent",
                            )
                        })?;
                    record.report = Some(report.clone());
                    let run = active_run(state)?;
                    run.active_ms = run.active_ms.max(active_ms);
                    encode(&report)
                },
            )
            .await;
        if recorded.is_err() {
            self.session.disconnect().await;
        }
        recorded.map_err(sdk_error)
    }
}
