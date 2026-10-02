//! Fine-grained durable admission within an already admitted provider attempt.

use super::*;
use bitrouter_sdk::language_model::native_work::{
    NativeProviderWork, NativeProviderWorkKind, NativeProviderWorkReport,
};

impl StepControl {
    pub(super) async fn admit_provider_work(
        &self,
        work: &NativeProviderWork,
    ) -> bitrouter_sdk::Result<()> {
        let step_id = self.step_id.lock().await.clone();
        let activity_id = format!("{}/{}", work.request_id, work.attempt_index);
        let active_ms = self
            .session
            .shared
            .live
            .lock()
            .await
            .activity
            .finish(&activity_id);
        self.session
            .transition_for(Some(&self.agent_id), "provider.work.intent", |state, _| {
                validate_step_source(state, &self.agent_id, &step_id)?;
                let run = active_run(state)?;
                run.active_ms = run.active_ms.max(active_ms);
                if !matches!(run.status, RunStatus::Running | RunStatus::Waiting)
                    || run.active_ms >= run.limits.active_seconds.saturating_mul(1000)
                {
                    return Err(reject(
                        ErrorCode::LimitExceeded,
                        "provider integration work is no longer admitted",
                    ));
                }
                if agent_turn(state, &self.agent_id)?.status != AgentStatus::ModelRunning {
                    return Err(reject(ErrorCode::Busy, "provider integration turn changed"));
                }
                let step = current_step(state, &self.agent_id, &step_id)?;
                if step
                    .plan
                    .as_ref()
                    .is_none_or(|plan| plan.request_id != work.request_id)
                {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "provider work has no committed plan",
                    ));
                }
                let attempt = step
                    .attempts
                    .last_mut()
                    .filter(|attempt| {
                        attempt.index == work.attempt_index && attempt.receipt.is_none()
                    })
                    .ok_or_else(|| {
                        reject(
                            ErrorCode::OperationConflict,
                            "provider work has no pending attempt",
                        )
                    })?;
                if attempt.provider_work.len() != work.work_index as usize
                    || attempt.provider_work.len() >= 32
                    || attempt
                        .provider_work
                        .last()
                        .is_some_and(|prior| prior.report.is_none())
                {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "provider work is not the next bounded phase",
                    ));
                }
                let retry = work.kind == NativeProviderWorkKind::HttpDispatch
                    && attempt
                        .provider_work
                        .iter()
                        .any(|prior| prior.work.kind == NativeProviderWorkKind::HttpDispatch);
                attempt.provider_work.push(ProviderWorkRecord {
                    work: work.clone(),
                    report: None,
                });
                if retry {
                    let run = active_run(state)?;
                    if run.model_attempts >= run.limits.model_attempts {
                        return Err(reject(
                            ErrorCode::LimitExceeded,
                            "internal HTTP retry exceeds model attempt budget",
                        ));
                    }
                    run.model_attempts += 1;
                }
                encode(work)
            })
            .await
            .map_err(sdk_error)?;
        self.session
            .ensure_dispatch(&self.agent_id, &step_id, activity_id)
            .await
            .map_err(sdk_error)
    }

    pub(super) async fn record_provider_work(&self, report: NativeProviderWorkReport) {
        let step_id = self.step_id.lock().await.clone();
        let activity_id = format!("{}/{}", report.work.request_id, report.work.attempt_index);
        let active_ms = self
            .session
            .shared
            .live
            .lock()
            .await
            .activity
            .finish(&activity_id);
        let recorded = self
            .session
            .transition_for(Some(&self.agent_id), "provider.work.outcome", |state, _| {
                let work = &report.work;
                let step = current_step(state, &self.agent_id, &step_id)?;
                if step
                    .plan
                    .as_ref()
                    .is_none_or(|plan| plan.request_id != work.request_id)
                {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "provider work outcome has no plan",
                    ));
                }
                let attempt = step
                    .attempts
                    .last_mut()
                    .filter(|attempt| {
                        attempt.index == work.attempt_index && attempt.receipt.is_none()
                    })
                    .ok_or_else(|| {
                        reject(
                            ErrorCode::OperationConflict,
                            "provider work outcome has no pending attempt",
                        )
                    })?;
                let retry = work.kind == NativeProviderWorkKind::HttpDispatch
                    && attempt
                        .provider_work
                        .iter()
                        .take(work.work_index as usize)
                        .any(|prior| prior.work.kind == NativeProviderWorkKind::HttpDispatch);
                let record = attempt
                    .provider_work
                    .last_mut()
                    .filter(|record| record.work == *work && record.report.is_none())
                    .ok_or_else(|| {
                        reject(
                            ErrorCode::OperationConflict,
                            "provider work outcome has no pending intent",
                        )
                    })?;
                if (work.kind != NativeProviderWorkKind::HttpDispatch
                    && report.http_status.is_some())
                    || (report.http_status.is_some() && report.error_code.is_some())
                    || (work.kind == NativeProviderWorkKind::HttpDispatch
                        && report.http_status.is_none()
                        && report.error_code.is_none())
                {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "provider work outcome is inconsistent",
                    ));
                }
                record.report = Some(report.clone());
                let run = active_run(state)?;
                run.active_ms = run.active_ms.max(active_ms);
                if retry && let Some(accounting) = &mut run.token_accounting {
                    // The outer attempt's usage can describe only its terminal result.
                    // No usage evidence establishes the earlier HTTP request as free.
                    accounting.record(
                        &bitrouter_sdk::language_model::native_accounting::NativeTokenCost::unknown(
                            "internal_http_retry_usage_unavailable",
                        ),
                    );
                }
                encode(&report)
            })
            .await;
        if recorded.is_err() {
            self.session.disconnect().await;
        } else {
            // Even after cancellation, consuming an accepted response and settling
            // its usage is ongoing work. New dispatch requires its own live gate.
            let mut live = self.session.shared.live.lock().await;
            live.activity.start(activity_id);
            budget::watch(&self.session, &mut live);
        }
    }
}
