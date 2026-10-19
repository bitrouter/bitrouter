use super::*;
use crate::store::ExecutionRecord;
use crate::turn::{TurnEventPayload, TurnStatus, VerificationStatus};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct Progress {
    owner: Owner,
    binding: Option<Binding>,
    cursor: u64,
    answer: String,
    history_complete: bool,
    prompt_dispatched: bool,
    start_dispatched: bool,
    outcome: Option<Outcome>,
}

impl ThreadService {
    pub(in crate::service) async fn run_acp_turn(
        &self,
        turn_id: String,
        job: Job,
        prompt: String,
        cancel: CancellationToken,
    ) {
        let Some(thread_id) = self
            .lock_state()
            .turns
            .get(&turn_id)
            .map(|turn| turn.thread_id.clone())
        else {
            return;
        };
        let duration = match self.lock_state().threads.get(&thread_id) {
            Some(thread) => thread.config.max_duration(),
            None => return,
        };
        let remaining = job.continuation.as_ref().map_or(duration, |saved| {
            duration.saturating_sub(Duration::from_millis(
                crate::service::now_ms().saturating_sub(saved.accepted_at_ms),
            ))
        });
        let deadline = tokio::time::Instant::now() + remaining;
        let mut progress = Progress {
            owner: Owner {
                server_instance_id: self.inner.instance_id.clone(),
                thread_id,
                turn_id: turn_id.clone(),
            },
            binding: None,
            cursor: 0,
            answer: String::new(),
            history_complete: true,
            prompt_dispatched: false,
            outcome: None,
            start_dispatched: false,
        };
        let result =
            Box::pin(self.drive_acp(&job, &prompt, &cancel, deadline, &mut progress)).await;
        let stop_key = Action::Stop.key(&turn_id);
        // A failed owner/store ACK must never prevent cleanup of the original
        // owned process. It cannot authorize another prompt or replacement.
        let intent = self
            .commit_records(
                &turn_id,
                &[ExecutionRecord::AcpDispatch {
                    owner: progress.owner.clone(),
                    action: Action::Stop,
                    action_id: stop_key.clone(),
                }],
            )
            .await;
        let stopped = if !progress.start_dispatched {
            Stop {
                run_id: None,
                confirmed: true,
                session_never_started: true,
                detail: "No ACP startup was dispatched by this Turn".into(),
            }
        } else {
            match job
                .driver
                .stop(&progress.owner, progress.binding.as_ref(), &stop_key)
                .await
            {
                Ok(observation) => observation,
                Err(error) => Stop {
                    run_id: progress
                        .binding
                        .as_ref()
                        .map(|binding| binding.run_id.clone()),
                    confirmed: false,
                    session_never_started: false,
                    detail: error,
                },
            }
        };
        if !stopped.confirmed {
            self.inner
                .cleanup_unconfirmed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let stop_saved = self
            .commit_records(
                &turn_id,
                &[ExecutionRecord::AcpProcessStop {
                    observation: stopped.clone(),
                }],
            )
            .await;
        if stopped.confirmed && progress.binding.is_some() {
            // Retain results that actually arrived during cancellation/stop.
            // Reading the original journal is not another prompt or stop RPC.
            for _ in 0..128 {
                let before = progress.cursor;
                if self.observe_acp(&job, &mut progress).await.is_err() || progress.cursor == before
                {
                    break;
                }
            }
        }
        let completed = matches!(progress.outcome, Some(Outcome::Completed))
            && progress.history_complete
            && !progress.answer.trim().is_empty();
        let unknown = !stopped.confirmed
            || intent.is_err()
            || stop_saved.is_err()
            || progress.prompt_dispatched && !completed
            || progress.binding.is_none() && !stopped.session_never_started;
        let cancelled = cancel.is_cancelled()
            || result.as_ref().is_err_and(|error| {
                matches!(
                    error.code,
                    ErrorCode::Unauthorized | ErrorCode::ShuttingDown
                )
            });
        let status = if unknown {
            TurnStatus::RecoveryRequired
        } else if cancelled {
            TurnStatus::Cancelled
        } else if completed {
            TurnStatus::Completed
        } else {
            TurnStatus::Failed
        };
        let detail = match result {
            Err(error) => format!("{error}; {}", stopped.detail),
            Ok(()) => match &progress.outcome {
                Some(Outcome::Unknown(error)) => format!("{error}; {}", stopped.detail),
                _ => stopped.detail,
            },
        };
        let _ = self
            .append(
                &turn_id,
                TurnEventPayload::Finished {
                    status,
                    detail,
                    final_answer: (!progress.answer.is_empty()).then_some(progress.answer),
                    verification: VerificationStatus::NotRequested,
                    verification_evidence: None,
                    unknown_effect: unknown,
                },
            )
            .await;
    }

    async fn drive_acp(
        &self,
        job: &Job,
        prompt: &str,
        cancel: &CancellationToken,
        deadline: tokio::time::Instant,
        progress: &mut Progress,
    ) -> Result<(), ServiceError> {
        let turn = &progress.owner.turn_id;
        self.append(turn, TurnEventPayload::Started).await?;
        self.acp_gate(turn, job).await?;
        if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
            return Err("Original ACP duration or execution control closed before startup".into());
        }
        let start = job.continuation.as_ref().map_or_else(
            || Action::Start.key(turn),
            |saved| format!("acp-{turn}-restore-{}", saved.binding.run_id),
        );
        self.commit_records(
            turn,
            &[ExecutionRecord::AcpDispatch {
                owner: progress.owner.clone(),
                action: Action::Start,
                action_id: start.clone(),
            }],
        )
        .await?;
        progress.start_dispatched = true;
        // Startup is owned and awaited to settlement. No prompt has been sent;
        // dropping this future on a transport timeout would lose its mapping.
        let binding = match &job.continuation {
            Some(saved) => job.driver.restore(&progress.owner, &start, saved).await,
            None => job.driver.start(&progress.owner, &start).await,
        }
        .map_err(ServiceError::storage)?;
        let config = {
            let state = self.lock_state();
            state
                .threads
                .get(&progress.owner.thread_id)
                .ok_or("ACP Thread disappeared")?
                .config
                .clone()
        };
        let crate::thread::executor::Config::Acp { acp } = config else {
            return Err("ACP driver has a native Thread".into());
        };
        progress.binding = Some(binding.clone());
        if job.continuation.as_ref().is_some_and(|saved| {
            binding.session_id != saved.binding.session_id
                || binding.agent_id != saved.binding.agent_id
                || binding.restored_from.as_ref() != Some(&saved.binding.run_id)
                || binding.run_id == saved.binding.run_id
        }) || binding.run_id.is_empty()
            || binding.session_id.is_empty()
            || binding.agent_id != acp.agent_id
            || acp
                .requested_model_route
                .as_ref()
                .is_some_and(|route| binding.confirmed_model_route.as_ref() != Some(route))
        {
            return Err("ACP session identity or required model route was not confirmed".into());
        }
        self.commit_records(
            turn,
            &[ExecutionRecord::AcpSession {
                binding: Box::new(binding.clone()),
            }],
        )
        .await?;
        self.acp_gate(turn, job).await?;
        if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
            return Err("ACP input closed before prompt dispatch".into());
        }
        let key = Action::Prompt.key(turn);
        self.commit_records(
            turn,
            &[ExecutionRecord::AcpDispatch {
                owner: progress.owner.clone(),
                action: Action::Prompt,
                action_id: key.clone(),
            }],
        )
        .await?;
        progress.prompt_dispatched = true;
        job.driver
            .prompt(&progress.owner, &binding, &key, prompt)
            .await
            .map_err(ServiceError::storage)?;
        loop {
            self.observe_acp(job, progress).await?;
            if progress.outcome.is_some() {
                return Ok(());
            }
            self.acp_gate(&progress.owner.turn_id, job).await?;
            loop {
                let choice = self
                    .lock_state()
                    .turns
                    .get_mut(&progress.owner.turn_id)
                    .and_then(|turn| turn.acp_replies.pop_front());
                let Some(choice) = choice else {
                    break;
                };
                self.acp_gate(&progress.owner.turn_id, job).await?;
                self.commit_records(
                    &progress.owner.turn_id,
                    &[ExecutionRecord::AcpPermissionDispatch {
                        request_id: choice.request_id.clone(),
                        option_id: choice.option_id.clone(),
                        action_id: choice.action_id.clone(),
                    }],
                )
                .await?;
                job.driver
                    .permission(
                        &progress.owner,
                        &binding,
                        &choice.action_id,
                        &choice.request_id,
                        &choice.option_id,
                    )
                    .await
                    .map_err(ServiceError::storage)?;
            }
            tokio::select! {
                () = cancel.cancelled() => return Err("ACP Turn cancellation requested".into()),
                () = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + Duration::from_millis(100))) => {},
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("ACP Turn duration exhausted".into());
            }
        }
    }

    async fn observe_acp(&self, job: &Job, progress: &mut Progress) -> Result<(), ServiceError> {
        let binding = progress
            .binding
            .as_ref()
            .ok_or("ACP session mapping missing")?;
        let observation = job
            .driver
            .observe(&progress.owner, binding, progress.cursor)
            .await
            .map_err(ServiceError::storage)?;
        if observation.cursor < progress.cursor
            || observation.snapshot_cursor < observation.cursor
            || observation.events.len() > 64
            || observation.permissions.len() > 16
            || serde_json::to_vec(&observation)
                .map_err(|error| error.to_string())?
                .len()
                > self.inner.limits.recovery_page_bytes
            || progress
                .answer
                .len()
                .saturating_add(observation.answer_delta.len())
                > crate::goal::MAX_INPUT_BYTES
        {
            return Err("ACP observation exceeds its cursor or byte bounds".into());
        }
        let mut last = progress.cursor;
        for event in &observation.events {
            let seq = event
                .get("seq")
                .and_then(serde_json::Value::as_u64)
                .ok_or("ACP public event lacks its original sequence")?;
            if seq <= last
                || seq > observation.cursor
                || observation.history_complete && seq != last.saturating_add(1)
            {
                return Err("ACP public event order or complete-history claim is invalid".into());
            }
            last = seq;
        }
        if last != observation.cursor
            || observation.events.is_empty() && !observation.answer_delta.is_empty()
        {
            return Err("ACP cursor/answer advanced without original public events".into());
        }
        for permission in &observation.permissions {
            if permission.request_id.is_empty()
                || permission.request_id.len() > 128
                || permission.options.is_empty()
                || permission.options.len() > 16
                || serde_json::to_vec(permission)
                    .map_err(|error| error.to_string())?
                    .len()
                    > self.inner.limits.request_bytes
            {
                return Err("ACP permission exceeds its original identity or byte bounds".into());
            }
        }
        if observation.cursor == progress.cursor
            && observation.events.is_empty()
            && observation.outcome.is_none()
        {
            progress.history_complete &= observation.history_complete;
            return Ok(());
        }
        self.commit_records(
            &progress.owner.turn_id,
            &[ExecutionRecord::AcpObservation {
                observation: Box::new(observation.clone()),
            }],
        )
        .await?;
        progress.cursor = observation.cursor;
        progress.history_complete &= observation.history_complete;
        progress.answer.push_str(&observation.answer_delta);
        if observation.outcome.is_some() {
            progress.outcome = observation.outcome;
        }
        // Keep only original pending request material; the shared approval
        // transaction chooses an actual option before the adapter sees it.
        Ok(())
    }

    pub(in crate::service) async fn acp_gate(
        &self,
        turn_id: &str,
        job: &Job,
    ) -> Result<(), ServiceError> {
        job.control
            .authorize()
            .await
            .map_err(|error| ServiceError::new(ErrorCode::Unauthorized, error.to_string()))?;
        let state = self.lock_state();
        let turn = state
            .turns
            .get(turn_id)
            .ok_or_else(crate::service::unknown_turn)?;
        let thread = state
            .threads
            .get(&turn.thread_id)
            .ok_or("ACP Thread disappeared")?;
        self.check_thread_grant(&state, thread)?;
        if state.closing
            || turn.cancel.is_cancelled()
            || turn.fence.pending()
            || turn.storage_error.is_some()
            || turn.snapshot.status.terminal()
            || turn.snapshot.status == TurnStatus::RecoveryRequired
        {
            return Err("ACP Turn is closed for new work".into());
        }
        Ok(())
    }
}
