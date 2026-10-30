//! The existing parent task set owns the entire child intent/send/outcome lifetime.
use super::*;
use crate::{
    gateway::model::decision::types::{
        DecisionError, DecisionFailure, DecisionRequest, DecisionResponse,
    },
    storage::checkpoint::snapshot::context::{DecisionReceipt, view::CandidateSet},
};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;

pub(in crate::core::session) struct Completed {
    pub id: String,
    pub response: Option<DecisionResponse>,
}

struct Driver {
    control: Arc<StepControl>,
    profile: ChoiceProfile,
    source: SourceRevision,
    candidates: CandidateSet,
    request: DecisionRequest,
    native: Arc<NativeRequest>,
    native_digest: String,
    journal: JournalView,
    cancellation: CancellationToken,
    step_id: String,
    owed: bool,
    complete: bool,
    outcome: Option<Result<DecisionResponse, DecisionError>>,
    child: Option<Arc<crate::tenancy::receipts::owner::Owner>>,
}

impl CoreSession {
    pub(in crate::core::session) async fn owned_choice(
        &self,
        control: &StepControl,
        step_id: &str,
        source: SourceRevision,
        candidates: CandidateSet,
        request: (DecisionRequest, wire::Prepared),
        profile: ChoiceProfile,
    ) -> Result<Completed, CoreError> {
        let (request, prepared) = request;
        let journal = self.shared.owned_journal.clone().ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "owned Choice journal missing",
            )
        })?;
        let retained = self
            .shared
            .live
            .lock()
            .await
            .model_controls
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .find(|candidate| std::ptr::eq(candidate.as_ref(), control))
            .ok_or_else(|| {
                reject(
                    ErrorCode::CheckpointUnavailable,
                    "owned model control missing",
                )
            })?;
        let step = retained.owned_step.as_ref().ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "owned model binding missing",
            )
        })?;
        let cleanup_deadline = step
            .cleanup_deadline()
            .map_err(crate::tenancy::execution::session::core_error)?;
        let tasks = step
            .tasks()
            .map_err(crate::tenancy::execution::session::core_error)?;
        let cancellation = control.provider_cancellation.child_token();
        let cancel_on_drop = cancellation.clone().drop_guard();
        let mut driver = Driver {
            control: retained,
            profile,
            source,
            candidates,
            request,
            native: Arc::new(prepared.request),
            native_digest: prepared.digest,
            journal,
            cancellation,
            step_id: step_id.into(),
            owed: false,
            complete: false,
            outcome: None,
            child: None,
        };
        let (reply, receive) = tokio::sync::oneshot::channel();
        let owner = tasks.clone();
        // The current Generation task already belongs to this closed root.
        // Register its descendant synchronously before any child/intent await;
        // it needs no new external Source and remains in this same drain.
        tasks.spawn_settlement(async move {
            let mut reply = Some(reply);
            if tokio::time::Instant::now() >= cleanup_deadline {
                #[cfg(test)]
                eprintln!("headroom Choice boundary: stage=BeforeFirstPoll, reason=CleanupDeadline");
                respond(
                    &mut reply,
                    Err(reject(
                        ErrorCode::CheckpointUnavailable,
                        "owned Choice cleanup deadline already elapsed",
                    )),
                );
                return;
            }
            let authority = driver.control.session.shared.owned.clone();
            let (result, timed_out) = {
                let work = AssertUnwindSafe(driver.run()).catch_unwind();
                tokio::pin!(work);
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(cleanup_deadline) => {
                        #[cfg(test)]
                        eprintln!("headroom Choice boundary: stage=Running, reason=CleanupDeadline");
                        quarantine(&owner, authority.as_deref());
                        respond(&mut reply, Err(reject(ErrorCode::CheckpointUnavailable,
                            "owned Choice cleanup deadline elapsed")));
                        // Keep polling this same owned work. Expiration cannot
                        // drop an actor reply, its pending ACK or provider owner.
                        (work.await, true)
                    }
                    result = &mut work => (result, false),
                }
            };
            let unknown = result.is_err()
                || (driver.owed && !driver.complete)
                || driver.child.as_ref().is_some_and(|child| !child.durable());
            #[cfg(test)]
            if unknown || timed_out {
                eprintln!(
                    "headroom Choice retained: unknown={}, unwind={}, owed={}, complete={}, child={}, timed_out={}",
                    unknown, result.is_err(), driver.owed, driver.complete, driver.child.is_some(), timed_out
                );
            }
            if unknown && !timed_out {
                quarantine(&owner, authority.as_deref());
            }
            let (outcome, unwind) = match result {
                Ok(outcome) => (outcome, None),
                Err(payload) => (
                    Err(reject(
                        ErrorCode::CheckpointUnavailable,
                        "owned Choice completion unwound",
                    )),
                    Some(payload),
                ),
            };
            respond(&mut reply, outcome);
            if unknown || timed_out {
                // Retain the caught payload as well: dropping an arbitrary panic
                // payload is not cleanup proof and can itself unwind.
                std::future::pending::<()>().await;
            }
            drop(unwind);
            drop(driver);
        });
        let outcome = receive.await.map_err(|_| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "owned Choice completion response unavailable",
            )
        })?;
        drop(cancel_on_drop.disarm());
        outcome
    }
}

impl Driver {
    async fn run(&mut self) -> Result<Completed, CoreError> {
        let result = self.perform().await;
        #[cfg(test)]
        if let Err(error) = &result {
            eprintln!(
                "headroom Choice perform failed: code={:?}, commit={:?}, owed={}, complete={}",
                error.code, error.commit_status, self.owed, self.complete
            );
        }
        if let Some(child) = &self.child {
            // Join the actual receipt proposal, even when intent preparation
            // failed before attachment. Core ACK never substitutes for this ACK.
            child
                .finish_or_join(bitrouter_ai::tenancy::execution::Outcome::Failed)
                .await
                .map_err(|_code| {
                    #[cfg(test)]
                    eprintln!("headroom Choice receipt join failed: code={_code:?}");
                    reject(
                        ErrorCode::CheckpointUnavailable,
                        "owned Choice receipt completion is unconfirmed",
                    )
                })?;
        }
        result
    }

    async fn perform(&mut self) -> Result<Completed, CoreError> {
        if self.cancellation.is_cancelled() {
            return Err(reject(
                ErrorCode::CheckpointUnavailable,
                "owned Choice preparation cancelled",
            ));
        }
        let session = self.control.session.clone();
        let binding = session.shared.owned.as_ref().ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "owned Choice authority missing",
            )
        })?;
        let step = self.control.owned_step.as_ref().ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "owned Choice step missing",
            )
        })?;
        let child = step
            .try_admit_choice()
            .map_err(crate::tenancy::execution::session::core_error)?;
        self.child = Some(child.receipt_owner().map_err(|_| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "owned Choice receipt owner missing",
            )
        })?);
        if child.profile() != &self.profile {
            return Err(reject(
                ErrorCode::UnauthorizedScope,
                "owned Choice profile changed",
            ));
        }
        let id = id("context_decision");
        let response_limit_bytes = self
            .profile
            .policy
            .max_request_bytes
            .checked_mul(2)
            .and_then(|n| n.checked_add(8192))
            .ok_or_else(|| reject(ErrorCode::LimitExceeded, "owned Choice response bound"))?;
        let mut receipt = DecisionReceipt {
            decision_id: id.clone(),
            run_id: self.control.run_id.clone(),
            task_id: self.control.agent_turn_id.clone(),
            agent_id: self.control.agent_id.clone(),
            source: self.source.clone(),
            candidates: self.candidates.clone(),
            request: self.request.clone(),
            request_sha256: digest(&self.request)?,
            policy: self.profile.policy.clone(),
            pricing: None,
            intent_state_revision: 0,
            response_limit_bytes,
            outcome: None,
            elapsed_ms: None,
            stale: false,
            view_ids: Vec::new(),
        };
        let descriptor = binding.descriptor();
        let mut record = Record {
            version: if child.recovery_seeds().is_some() {
                2
            } else {
                1
            },
            seeds: child.recovery_seeds(),
            terminal_digests: None,
            installation: descriptor.installation.clone(),
            tenant: descriptor.tenant.clone(),
            revision: descriptor.revision.clone(),
            parent: self.profile.parent.clone(),
            child: self.profile.child.clone(),
            parent_execution: child.parent_id().clone(),
            child_execution: child.execution_id().clone(),
            profile_digest: self.profile.digest.clone(),
            native_digest: self.native_digest.clone(),
            decision_id: id.clone(),
            step_id: self.step_id.clone(),
            epoch: 0,
            intent_digest: String::new(),
            outcome_digest: None,
        };
        record.epoch = session
            .shared
            .live
            .lock()
            .await
            .gate
            .grant()
            .execution_epoch;
        self.owed = true;
        let intent = session.transition_for(Some(&self.control.agent_id), "context.decision.intent", |state, head| {
            budget::ensure(state)?;
            context_decisions::validate_decision_source(state, &self.control.agent_id, &self.step_id, &self.source)?;
            let run = state.run.as_ref().ok_or_else(|| reject(ErrorCode::Busy, "owned Choice run missing"))?;
            if run.run_id != self.control.run_id || run.model_attempts >= run.limits.model_attempts
                || state.context_store.decisions.values().filter(|entry| entry.run_id == run.run_id).count() >= run.limits.model_attempts as usize {
                return Err(reject(ErrorCode::LimitExceeded, "run decision attempt limit reached"));
            }
            receipt.intent_state_revision = head.state_revision + 1;
            record.intent_digest = crate::harness::resources::managed::store::choice::intent(&receipt)?;
            state.context_store.decisions.insert(id.clone(), receipt.clone());
            Ok(json!({"decision_id":id,"task_id":receipt.task_id,"request_sha256":receipt.request_sha256,"owned_choice":record}))
        }).await;
        if let Err(error) = intent {
            if error.commit_status == CommitStatus::NotCommitted {
                self.owed = false;
            }
            return Err(error);
        }
        let gate = Arc::new(SendGate {
            control: self.control.clone(),
            source: self.source.clone(),
            profile: self.profile.clone(),
            record,
            native: self.native.clone(),
            journal: self.journal.clone(),
            cancellation: self.cancellation.clone(),
            deadline: child.deadline(),
            claimed: AtomicBool::new(false),
            armed: AtomicBool::new(false),
        });
        let started = Instant::now();
        let outcome = match child
            .execute(self.native.as_ref().clone(), gate.clone())
            .await
        {
            Ok(response) if response.request_id == gate.record.child_execution.as_str() => {
                match response.result.decisions() {
                    Some(result) => {
                        wire::response(&self.request, &self.native, result, response_limit_bytes)
                    }
                    None => Err(failure(DecisionFailure::InvalidResponse, true)),
                }
            }
            Ok(_) => Err(failure(DecisionFailure::InvalidResponse, true)),
            Err(error) => {
                let mut outcome = failure(
                    if self.cancellation.is_cancelled() {
                        DecisionFailure::Cancelled
                    } else {
                        DecisionFailure::Transport
                    },
                    gate.armed.load(Ordering::Acquire),
                );
                if let crate::gateway::error::BitrouterError::UpstreamInvalidResponse {
                    usage: Some(usage),
                    ..
                } = error
                {
                    outcome.kind = DecisionFailure::InvalidResponse;
                    outcome.usage = usage.raw.as_deref().and_then(|raw| {
                        Some(crate::gateway::model::decision::types::DecisionUsage {
                            input_tokens: raw.get("input_tokens")?.as_u64()?,
                            output_tokens: raw.get("output_tokens")?.as_u64()?,
                        })
                    });
                }
                Err(outcome)
            }
        };
        self.outcome = Some(outcome.clone());
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let active_ms = session.shared.live.lock().await.activity.finish(&id);
        let mut stale = false;
        let recorded = session
            .transition_for(
                Some(&self.control.agent_id),
                "context.decision.outcome",
                |state, _| {
                    stale = context_decisions::validate_decision_source(
                        state,
                        &self.control.agent_id,
                        &self.step_id,
                        &self.source,
                    )
                    .is_err();
                    let receipt = state
                        .context_store
                        .decisions
                        .get_mut(&id)
                        .filter(|receipt| receipt.outcome.is_none())
                        .ok_or_else(|| {
                            reject(
                                ErrorCode::OperationConflict,
                                "owned Choice intent is not pending",
                            )
                        })?;
                    receipt.outcome = Some(outcome.clone());
                    receipt.elapsed_ms = Some(elapsed_ms);
                    receipt.stale = stale;
                    let run = active_run(state)?;
                    run.active_ms = run.active_ms.max(active_ms);
                    Ok(json!({"decision_id":id,"stale":stale,"elapsed_ms":elapsed_ms}))
                },
            )
            .await;
        if let Err(error) = recorded {
            // An exact full-row readback can adopt this submitted outcome, but
            // cannot re-enable dispatch or infer an ACK from a worker join.
            if error.commit_status != CommitStatus::NotCommitted
                && self.recover_outcome(&id).await.is_ok()
            {
                self.complete = true;
            }
            return Err(error);
        }
        self.complete = true;
        if stale {
            return Err(reject(
                ErrorCode::StaleRevision,
                "owned Choice source changed before application",
            ));
        }
        Ok(Completed {
            id,
            response: outcome.ok(),
        })
    }

    async fn recover_outcome(&self, id: &str) -> Result<(), CoreError> {
        let session = &self.control.session;
        let _commit = session.shared.commits.lock().await;
        let durable = self.journal.readback().await?;
        let recorded = self.journal.choice(id, &durable).await?.ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "owned Choice readback missing",
            )
        })?;
        if recorded.outcome_digest.is_none() {
            return Err(reject(
                ErrorCode::CheckpointUnavailable,
                "owned Choice outcome not durable",
            ));
        }
        let mut live = session.shared.live.lock().await;
        let payload = live.gate.reconnect(&durable)?.ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "owned Choice pending ACK not established",
            )
        })?;
        let candidate = live.pending.as_ref().ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "owned Choice candidate is missing",
            )
        })?;
        let receipt = candidate
            .state
            .context_store
            .decisions
            .get(id)
            .ok_or_else(|| {
                reject(
                    ErrorCode::CheckpointUnavailable,
                    "owned Choice candidate outcome is missing",
                )
            })?;
        if receipt.outcome != self.outcome
            || recorded.outcome_digest
                != crate::harness::resources::managed::store::choice::outcome(receipt)?
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "owned Choice readback outcome changed",
            ));
        }
        reconnect::adopt_pending(&mut live, &payload)?;
        live.gate.disconnect();
        live.disconnected.cancel();
        Ok(())
    }
}

fn failure(kind: DecisionFailure, may_have_run: bool) -> DecisionError {
    DecisionError {
        kind,
        message: "owned Choice execution did not produce a usable answer".into(),
        usage: None,
        may_have_run,
    }
}

fn respond(
    reply: &mut Option<tokio::sync::oneshot::Sender<Result<Completed, CoreError>>>,
    result: Result<Completed, CoreError>,
) {
    if let Some(reply) = reply.take()
        && reply.send(result).is_err()
    {
        tracing::debug!("owned Choice completed without its preparation waiter");
    }
}

fn quarantine(
    tasks: &crate::gateway::pipeline::owned_tasks::OwnedTasks,
    binding: Option<&crate::tenancy::execution::session::Binding>,
) {
    tasks.record_finalizer_failure();
    if let Some(binding) = binding {
        binding.authority().service().host_cleanup_unknown();
    }
    tracing::error!("owned Choice ACK is unconfirmed; actual completion owner retained");
}
