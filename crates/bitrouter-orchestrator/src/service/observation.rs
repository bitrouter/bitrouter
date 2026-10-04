//! Public Thread projections are published only after their execution
//! transaction commits. They never supply SDK context or execution authority.

use std::collections::VecDeque;
use std::time::Instant;

use bitrouter_sdk::caller::CallerContext;
use tokio::sync::broadcast;

use super::{
    ErrorCode, MAX_EVENT_PAGE, RuntimeLimits, ServiceError, ThreadService, now_ms, threads,
};
use crate::agent::AgentConfig;
use crate::store::{EffectStatus, ExecutionRecord};
use crate::thread::{
    ThreadChange, ThreadEvent, ThreadHistoryPage, ThreadHistoryRequest, ThreadObservation,
    ThreadSnapshot, ThreadStatus, ThreadTarget, ThreadView,
};
use crate::turn::{
    TurnEvent, TurnEventPayload, TurnReceipt, TurnSnapshot, TurnStatus, VerificationStatus,
};

pub(super) struct Presentation {
    pub(super) view: ThreadView,
    events: VecDeque<(ThreadEvent, usize)>,
    bytes: usize,
    evicted_through: u64,
    pub(super) publisher: broadcast::Sender<ThreadObservation>,
    finished_items: std::collections::HashSet<String>,
}

impl Presentation {
    pub(super) fn recovered(view: ThreadView, limits: &RuntimeLimits) -> Self {
        let cutoff = view.thread.cursor;
        let mut presentation = Self::new(view, limits);
        presentation.evicted_through = cutoff;
        presentation
    }
    pub(super) fn new(view: ThreadView, limits: &RuntimeLimits) -> Self {
        // Durable events are capped by history_page_bytes. This conservative
        // capacity bounds broadcast storage by both bytes and entry count.
        let queue = limits
            .subscriber_queue
            .min(limits.subscriber_bytes_per_thread / limits.history_page_bytes)
            .max(1);
        Self {
            view,
            events: VecDeque::new(),
            bytes: 0,
            evicted_through: 0,
            publisher: broadcast::channel(queue).0,
            finished_items: std::collections::HashSet::new(),
        }
    }

    pub(super) fn publish(&mut self, event: ThreadEvent, limits: &RuntimeLimits) {
        for change in &event.changes {
            match change {
                ThreadChange::TurnActivated { turn_id, .. }
                    if self
                        .view
                        .latest_turn
                        .as_ref()
                        .is_none_or(|turn| &turn.turn_id != turn_id) =>
                {
                    self.finished_items.clear()
                }
                ThreadChange::AssistantResponse { item_id, .. }
                | ThreadChange::AssistantInterrupted { item_id, .. }
                | ThreadChange::ToolResult { item_id, .. } => {
                    self.finished_items.insert(item_id.clone());
                }
                ThreadChange::VerificationResult { call, .. } => {
                    self.finished_items.insert(call.item_id.clone());
                }
                _ => {}
            }
        }
        self.view.apply(&event);
        let size = serde_json::to_vec(&event).map_or(usize::MAX, |encoded| encoded.len());
        self.bytes = self.bytes.saturating_add(size);
        self.events.push_back((event.clone(), size));
        while self.events.len() > limits.events_per_thread
            || self.bytes > limits.event_bytes_per_thread
        {
            if let Some((evicted, bytes)) = self.events.pop_front() {
                self.bytes = self.bytes.saturating_sub(bytes);
                self.evicted_through = evicted.seq;
            } else {
                break;
            }
        }
        let _ = self.publisher.send(ThreadObservation::Event {
            event: Box::new(event),
        });
    }

    pub(super) fn live(&mut self, event: &TurnEvent) {
        let item_id = match &event.payload {
            TurnEventPayload::AssistantDelta { item_id, .. } => item_id,
            TurnEventPayload::ToolOutputDelta { id, .. } => id,
            _ => return,
        };
        if self.finished_items.contains(item_id) {
            return;
        }
        if let Some(turn) = &mut self.view.latest_turn
            && turn.turn_id == event.turn_id
        {
            turn.apply(event);
        }
        let _ = self.publisher.send(ThreadObservation::Live {
            after_cursor: self.view.thread.cursor,
            event: Box::new(event.clone()),
        });
    }

    pub(super) fn blocked(&mut self, reason: &str) {
        self.view.thread.status = ThreadStatus::RecoveryRequired;
        self.view.thread.pause_reason = Some(reason.into());
        if let Some(turn) = &mut self.view.latest_turn
            && !turn.status.terminal()
        {
            turn.status = TurnStatus::RecoveryRequired;
            turn.unknown_effect = true;
            turn.detail = Some(reason.into());
        }
        let _ = self.publisher.send(ThreadObservation::Snapshot {
            view: Box::new(self.view.clone()),
            resynchronized: true,
            catchup: Vec::new(),
        });
    }

    pub(super) fn waiting_for_capacity(&mut self) {
        if !self.view.thread.waiting_for_capacity {
            self.view.thread.waiting_for_capacity = true;
            let _ = self.publisher.send(ThreadObservation::Snapshot {
                view: Box::new(self.view.clone()),
                resynchronized: false,
                catchup: Vec::new(),
            });
        }
    }

    fn snapshot(&self, after: Option<u64>) -> ThreadObservation {
        let resynchronized = after.is_some_and(|cursor| cursor < self.evicted_through);
        let catchup = match after {
            Some(cursor) if !resynchronized => self
                .events
                .iter()
                .filter(|(event, _)| event.seq > cursor)
                .map(|(event, _)| event.clone())
                .collect(),
            _ => Vec::new(),
        };
        ThreadObservation::Snapshot {
            view: Box::new(self.view.clone()),
            resynchronized,
            catchup,
        }
    }
}

/// A caller-bound attachment; dropping it never cancels or answers an input.
pub struct ThreadSubscription {
    service: ThreadService,
    target: ThreadTarget,
    caller: CallerContext,
    receiver: broadcast::Receiver<ThreadObservation>,
    initial: Option<ThreadObservation>,
}

impl ThreadSubscription {
    pub async fn next(&mut self) -> Result<Option<ThreadObservation>, ServiceError> {
        self.service.read_thread_view(&self.target, &self.caller)?;
        if let Some(initial) = self.initial.take() {
            return Ok(Some(initial));
        }
        let result = match self.receiver.recv().await {
            Ok(observation) => Some(observation),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // Re-registration and cutoff share the publication mutex.
                let state = self.service.lock_state();
                let thread = state
                    .threads
                    .get(&self.target.thread_id)
                    .ok_or_else(threads::unknown_thread)?;
                thread.authorize(&self.caller)?;
                self.service.check_thread_grant(&state, thread)?;
                self.receiver = thread.presentation.publisher.subscribe();
                let mut view = thread.presentation.view.clone();
                view.thread.waiting_for_capacity = thread.snapshot.waiting_for_capacity;
                Some(ThreadObservation::Snapshot {
                    view: Box::new(view),
                    resynchronized: true,
                    catchup: Vec::new(),
                })
            }
            Err(broadcast::error::RecvError::Closed) => None,
        };
        // A grant can be revoked while recv waits. Do not deliver queued data
        // using an authorization check from before the wait.
        self.service.read_thread_view(&self.target, &self.caller)?;
        Ok(result)
    }
}

impl ThreadService {
    pub fn read_thread_view(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
    ) -> Result<ThreadView, ServiceError> {
        self.ensure_instance(Some(&target.server_instance_id))?;
        let mut state = self.lock_state();
        let thread = state
            .threads
            .get_mut(&target.thread_id)
            .ok_or_else(threads::unknown_thread)?;
        thread.authorize(caller)?;
        thread.last_used = Instant::now();
        let mut view = thread.presentation.view.clone();
        // Capacity waiting is transient scheduler state, without new context.
        view.thread.waiting_for_capacity = thread.snapshot.waiting_for_capacity;
        drop(state);
        self.check_snapshot_grant(&view.thread)?;
        Ok(view)
    }

    pub fn observe_thread(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        after: Option<u64>,
    ) -> Result<ThreadSubscription, ServiceError> {
        self.ensure_instance(Some(&target.server_instance_id))?;
        let state = self.lock_state();
        let thread = state
            .threads
            .get(&target.thread_id)
            .ok_or_else(threads::unknown_thread)?;
        thread.authorize(caller)?;
        self.check_thread_grant(&state, thread)?;
        let presentation = &thread.presentation;
        if after.is_some_and(|cursor| cursor > presentation.view.thread.cursor) {
            return Err("cursor is ahead of Thread".into());
        }
        if presentation.publisher.receiver_count() >= self.inner.limits.subscribers_per_thread {
            return Err(ServiceError::new(
                ErrorCode::Overloaded,
                "too many Thread observers",
            ));
        }
        let mut initial = presentation.snapshot(after);
        if let ThreadObservation::Snapshot { view, .. } = &mut initial {
            view.thread.waiting_for_capacity = thread.snapshot.waiting_for_capacity;
        }
        Ok(ThreadSubscription {
            service: self.clone(),
            target: target.clone(),
            caller: caller.clone(),
            receiver: presentation.publisher.subscribe(),
            initial: Some(initial),
        })
    }

    pub async fn thread_history(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        request: ThreadHistoryRequest,
    ) -> Result<ThreadHistoryPage, ServiceError> {
        let view = self.read_stored_thread_view(target, caller).await?;
        let cutoff = request.cutoff.unwrap_or(view.thread.cursor);
        if request.limit == 0
            || request.limit > MAX_EVENT_PAGE
            || request.after > cutoff
            || cutoff > view.thread.cursor
        {
            return Err("invalid Thread history cutoff or page size".into());
        }
        let chunk = self
            .inner
            .store
            .thread_history(
                &target.thread_id,
                request.after,
                cutoff,
                request.limit,
                self.inner.limits.history_page_bytes,
            )
            .await
            .map_err(ServiceError::storage)?;
        self.check_snapshot_grant(&view.thread)?;
        let next_after = if chunk.more {
            Some(
                chunk
                    .events
                    .last()
                    .ok_or("history backend returned no progress")?
                    .seq,
            )
        } else {
            None
        };
        Ok(ThreadHistoryPage {
            server_instance_id: self.inner.instance_id.clone(),
            thread_id: target.thread_id.clone(),
            cutoff,
            events: chunk.events,
            next_after,
        })
    }

    pub(super) fn thread_transaction(
        &self,
        thread_id: &str,
        version: u64,
        facts: &[ExecutionRecord],
    ) -> Result<(Vec<ExecutionRecord>, ThreadEvent), ServiceError> {
        let seq = version
            .checked_add(u64::try_from(facts.len()).map_err(|error| error.to_string())?)
            .and_then(|seq| seq.checked_add(1))
            .ok_or("Thread cursor exhausted")?;
        let mut records = facts.to_vec();
        for record in &mut records {
            if let ExecutionRecord::ThreadCreated { snapshot, .. }
            | ExecutionRecord::ThreadCheckpoint { snapshot, .. } = record
            {
                snapshot.cursor = seq;
            }
        }
        let mut changes = Vec::new();
        for record in &records {
            project(record, thread_id, None, &mut changes);
        }
        let event = ThreadEvent {
            server_instance_id: self.inner.instance_id.clone(),
            thread_id: thread_id.into(),
            seq,
            timestamp_ms: now_ms(),
            changes,
        };
        if serde_json::to_vec(&event)
            .map_err(|error| error.to_string())?
            .len()
            > self.inner.limits.history_page_bytes
        {
            return Err(ServiceError::new(
                ErrorCode::StorageUnavailable,
                "Thread transaction projection exceeds history byte bound",
            ));
        }
        records.push(ExecutionRecord::ThreadEvent {
            event: event.clone(),
        });
        Ok((records, event))
    }
}

pub(crate) fn project(
    record: &ExecutionRecord,
    thread_id: &str,
    turn_id: Option<&str>,
    changes: &mut Vec<ThreadChange>,
) {
    let turn = turn_id.map_or_else(String::new, str::to_owned);
    let change = match record {
        ExecutionRecord::ThreadCreated {
            snapshot,
            config,
            verification_command,
            ..
        } => ThreadChange::Created {
            view: Box::new(ThreadView {
                recovery: None,
                thread: snapshot.clone(),
                config: config.as_ref().clone(),
                verification_command: verification_command.clone(),
                latest_turn: None,
            }),
        },
        ExecutionRecord::TurnQueued {
            turn_id,
            user_item_id,
            prompt,
            queue_order,
        } => ThreadChange::TurnQueued {
            receipt: TurnReceipt {
                thread_id: thread_id.into(),
                turn_id: turn_id.clone(),
                queue_order: *queue_order,
                status: TurnStatus::Queued,
            },
            user_item_id: user_item_id.clone(),
            prompt: prompt.clone(),
        },
        ExecutionRecord::TurnActivated {
            turn_id,
            context_version,
        } => ThreadChange::TurnActivated {
            turn_id: turn_id.clone(),
            context_version: *context_version,
        },
        ExecutionRecord::QueuedTurnCancelled { turn_id } => ThreadChange::QueuedTurnCancelled {
            turn_id: turn_id.clone(),
        },
        ExecutionRecord::QueueResumed => ThreadChange::QueueResumed,
        ExecutionRecord::ThreadRecovered {
            source_server_instance_id,
            source_cursor,
        } => ThreadChange::Recovered {
            source_server_instance_id: source_server_instance_id.clone(),
            source_cursor: *source_cursor,
        },
        ExecutionRecord::ThreadCheckpoint { snapshot, .. } => ThreadChange::Checkpoint {
            snapshot: snapshot.clone(),
        },
        ExecutionRecord::TurnRecord { turn_id, fact } => {
            project(fact, thread_id, Some(turn_id), changes);
            return;
        }
        ExecutionRecord::TurnLifecycle { turn_id, lifecycle } => ThreadChange::TurnLifecycle {
            turn_id: turn_id.clone(),
            lifecycle: lifecycle.clone(),
        },
        ExecutionRecord::ModelRequest {
            step_id,
            item_id,
            context_version,
            ..
        } => ThreadChange::ModelStep {
            turn_id: turn,
            step_id: step_id.clone(),
            item_id: item_id.clone(),
            context_version: *context_version,
        },
        ExecutionRecord::ModelResponse {
            step_id,
            item_id,
            request_id,
            requested_model,
            usage,
            message,
            calls,
            ..
        } => ThreadChange::AssistantResponse {
            turn_id: turn,
            step_id: step_id.clone(),
            item_id: item_id.clone(),
            request_id: request_id.clone(),
            requested_model: requested_model.clone(),
            usage: usage.clone(),
            message: message.clone(),
            calls: calls.clone(),
        },
        ExecutionRecord::ModelInterrupted {
            step_id,
            item_id,
            partial,
            detail,
            ..
        } => ThreadChange::AssistantInterrupted {
            turn_id: turn,
            step_id: step_id.clone(),
            item_id: item_id.clone(),
            partial: partial.clone(),
            detail: detail.clone(),
        },
        ExecutionRecord::ToolIntent { step_id, call } => ThreadChange::ToolIntent {
            turn_id: turn,
            step_id: step_id.clone(),
            call: call.clone(),
        },
        ExecutionRecord::ToolResult {
            step_id,
            item_id,
            message,
            effect,
        } => ThreadChange::ToolResult {
            turn_id: turn,
            step_id: step_id.clone(),
            item_id: item_id.clone(),
            message: message.clone(),
            effect: *effect,
        },
        ExecutionRecord::VerificationResult {
            call,
            evidence,
            effect,
            active_duration_ms,
            tool_calls,
            ..
        } => ThreadChange::VerificationResult {
            turn_id: turn,
            call: call.clone(),
            evidence: evidence.clone(),
            effect: *effect,
            active_duration_ms: *active_duration_ms,
            tool_calls: *tool_calls,
        },
        ExecutionRecord::Settled {
            context_version, ..
        } => ThreadChange::ContextAdvanced {
            turn_id: turn,
            context_version: *context_version,
        },
        _ => return,
    };
    changes.push(change);
}

impl ThreadView {
    pub fn apply(&mut self, event: &ThreadEvent) {
        if event.thread_id != self.thread.thread_id
            || event.server_instance_id != self.thread.server_instance_id
            || event.seq <= self.thread.cursor
        {
            return;
        }
        for change in &event.changes {
            match change {
                ThreadChange::Created { view } => *self = view.as_ref().clone(),
                ThreadChange::Checkpoint { snapshot } => self.thread = snapshot.clone(),
                ThreadChange::TurnQueued { receipt, .. } => {
                    self.thread.queued.push(receipt.clone())
                }
                ThreadChange::QueuedTurnCancelled { turn_id } => {
                    self.thread.queued.retain(|entry| &entry.turn_id != turn_id)
                }
                ThreadChange::QueueResumed => {
                    self.thread.status = ThreadStatus::Idle;
                    self.thread.pause_reason = None;
                }
                ThreadChange::Recovered { .. } => self.recovery = None,
                ThreadChange::TurnActivated {
                    turn_id,
                    context_version,
                } => {
                    self.thread.status = ThreadStatus::Busy;
                    self.thread.active_turn_id = Some(turn_id.clone());
                    self.thread.queued.retain(|entry| &entry.turn_id != turn_id);
                    self.thread.context_version = *context_version;
                    self.thread.waiting_for_capacity = false;
                    if self
                        .latest_turn
                        .as_ref()
                        .is_none_or(|turn| &turn.turn_id != turn_id)
                    {
                        self.latest_turn = Some(empty_turn(&self.thread, &self.config, turn_id));
                    }
                }
                ThreadChange::TurnLifecycle { turn_id, lifecycle } => {
                    if let Some(turn) = &mut self.latest_turn
                        && &turn.turn_id == turn_id
                    {
                        turn.apply_payload(&lifecycle.payload());
                    }
                }
                ThreadChange::ModelStep {
                    context_version, ..
                }
                | ThreadChange::ContextAdvanced {
                    context_version, ..
                } => self.thread.context_version = *context_version,
                ThreadChange::AssistantResponse { turn_id, .. }
                | ThreadChange::AssistantInterrupted { turn_id, .. }
                | ThreadChange::ToolResult { turn_id, .. } => {
                    if let Some(turn) = &mut self.latest_turn
                        && &turn.turn_id == turn_id
                    {
                        turn.live = None;
                    }
                }
                ThreadChange::VerificationResult {
                    turn_id,
                    evidence,
                    effect,
                    ..
                } => {
                    if let Some(turn) = &mut self.latest_turn
                        && &turn.turn_id == turn_id
                    {
                        turn.verification_evidence = Some(evidence.clone());
                        turn.unknown_effect |= *effect == EffectStatus::Unknown;
                        turn.live = None;
                    }
                }
                ThreadChange::ToolIntent { .. } => {}
            }
        }
        if let Some(turn) = &mut self.latest_turn {
            turn.cursor = event.seq;
        }
        self.thread.cursor = event.seq;
    }
}

pub(super) fn empty_turn(
    thread: &ThreadSnapshot,
    config: &AgentConfig,
    turn_id: &str,
) -> TurnSnapshot {
    TurnSnapshot {
        steering: Vec::new(),
        thread_id: thread.thread_id.clone(),
        server_instance_id: thread.server_instance_id.clone(),
        model: thread.model.clone(),
        turn_id: turn_id.into(),
        status: TurnStatus::Accepted,
        cursor: 0,
        workspace: thread.workspace.clone(),
        tool_mode: config.tool_mode(),
        final_answer: None,
        detail: None,
        unknown_effect: false,
        verification: VerificationStatus::Unavailable,
        verification_evidence: None,
        pending_input_id: None,
        pending_input: None,
        live: None,
    }
}
