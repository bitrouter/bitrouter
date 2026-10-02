//! Fenced foreground transport. The resident owner retains prompts and
//! permissions when this terminal disappears.

use crate::actions::supervised::SupervisedHandle;
use crate::supervisor::{
    PendingPermissionSnapshot, ProcessState, ReplayBatch, SessionAction, SessionEventKind,
};
use agent_client_protocol::schema::v1::{PromptResponse, RequestPermissionOutcome, SessionUpdate};
use anyhow::{Result, ensure};
use futures::{FutureExt, future::BoxFuture};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

pub(crate) enum WireEvent {
    Update(SessionUpdate),
    Permission(PendingPermissionSnapshot),
    PermissionResolved(String),
    Settled(Result<PromptResponse>),
    CancelFailed(String),
    Notice(String),
    Disconnected,
}

type MutationTask = BoxFuture<'static, Result<()>>;
type PollTask = BoxFuture<'static, Result<ReplayBatch>>;

#[derive(Default)]
pub(crate) struct CodeWire {
    pub handle: Option<SupervisedHandle>,
    pending: HashMap<String, PendingPermissionSnapshot>,
    working: bool,
    cancelling: bool,
    cursor: u64,
    events: VecDeque<WireEvent>,
    actions: VecDeque<SessionAction>,
    mutation: Option<MutationTask>,
    mutation_is_prompt: bool,
    mutation_action: Option<SessionAction>,
    poll: Option<PollTask>,
    heartbeat_at: Option<tokio::time::Instant>,
    disconnected: bool,
}

impl CodeWire {
    pub fn attach(&mut self, handle: SupervisedHandle) {
        self.cursor = handle.last_seq;
        self.handle = Some(handle);
        self.poll = None;
        self.heartbeat_at = None;
        self.disconnected = false;
    }
    pub fn reattach(&mut self, handle: SupervisedHandle) {
        self.events.clear();
        self.pending.clear();
        self.attach(handle);
    }
    pub fn working(&self) -> bool {
        self.working
    }
    pub fn has_permissions(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn submit(&mut self, prompt: String) -> Result<()> {
        ensure!(
            self.handle.is_some() && !self.disconnected,
            "Choose an agent before submitting"
        );
        ensure!(
            !self.working && !self.cancelling,
            "Wait for the current turn to settle"
        );
        ensure!(
            self.pending.is_empty(),
            "Answer the pending permission before submitting"
        );
        self.working = true;
        self.actions
            .push_back(SessionAction::Prompt { text: prompt });
        Ok(())
    }
    pub fn resolve(&mut self, id: &str, outcome: RequestPermissionOutcome) {
        if !self.pending.contains_key(id) {
            return;
        }
        if let RequestPermissionOutcome::Selected(selected) = outcome {
            self.actions.push_back(SessionAction::Permission {
                permission_id: id.to_string(),
                option_id: selected.option_id.to_string(),
            });
        } else {
            self.cancel();
        }
    }
    pub fn cancel(&mut self) {
        if !self.working || self.cancelling {
            return;
        }
        self.cancelling = true;
        self.actions.push_back(SessionAction::Cancel);
    }

    fn begin_requests(&mut self) {
        let Some(handle) = &self.handle else {
            return;
        };
        if self.mutation.is_none()
            && let Some(action) = self.actions.pop_front()
        {
            self.mutation_is_prompt = matches!(action, SessionAction::Prompt { .. });
            self.mutation_action = Some(action.clone());
            let client = handle.client.clone();
            self.mutation = Some(async move { client.mutate(action).await.map(|_| ()) }.boxed());
        }
        if self.poll.is_none() {
            let client = handle.client.clone();
            let cursor = self.cursor;
            let heartbeat = self
                .heartbeat_at
                .is_none_or(|deadline| tokio::time::Instant::now() >= deadline);
            if heartbeat {
                self.heartbeat_at = Some(tokio::time::Instant::now() + Duration::from_secs(5));
            }
            self.poll = Some(
                async move {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    if heartbeat {
                        client.heartbeat().await?;
                    }
                    client.events(cursor).await
                }
                .boxed(),
            );
        }
    }

    fn ingest(&mut self, replay: ReplayBatch) -> Result<()> {
        // Native scrollback cannot be erased/replayed. Reject a gap instead of
        // merging stale history; recovery uses an explicit retained inspector.
        let mut cursor = self.cursor;
        for event in &replay.events {
            if event.seq <= cursor {
                continue;
            }
            ensure!(
                event.seq == cursor + 1,
                "Foreground event history has a gap; detach and inspect retained history"
            );
            cursor = event.seq;
        }
        for event in replay.events {
            if event.seq <= self.cursor {
                continue;
            }
            self.cursor = event.seq;
            match event.kind {
                SessionEventKind::Update { update } => {
                    self.events.push_back(WireEvent::Update(update))
                }
                SessionEventKind::Permission { permission } => {
                    if self
                        .pending
                        .insert(permission.permission_id.clone(), permission.clone())
                        .is_none()
                    {
                        self.events.push_back(WireEvent::Permission(permission));
                    }
                }
                SessionEventKind::PermissionResolved { permission_id, .. } => {
                    self.pending.remove(&permission_id);
                    self.events
                        .push_back(WireEvent::PermissionResolved(permission_id));
                }
                SessionEventKind::TurnSettled { result } => {
                    self.working = false;
                    self.cancelling = false;
                    self.pending.clear();
                    let result =
                        serde_json::from_value(serde_json::Value::String(result.stop_reason))
                            .map(PromptResponse::new)
                            .map_err(anyhow::Error::from);
                    self.events.push_back(WireEvent::Settled(result));
                }
                SessionEventKind::TurnFailed { message } => {
                    self.working = false;
                    self.cancelling = false;
                    self.pending.clear();
                    self.events
                        .push_back(WireEvent::Settled(Err(anyhow::anyhow!(message))));
                }
                SessionEventKind::Lifecycle {
                    process:
                        ProcessState::Stopped | ProcessState::Failed | ProcessState::Interrupted,
                } => {
                    self.events.push_back(WireEvent::Disconnected);
                    self.disconnected = true;
                }
                SessionEventKind::PayloadOmitted { description } => {
                    self.events.push_back(WireEvent::Notice(description))
                }
                _ => {}
            }
        }
        self.cursor = cursor;
        Ok(())
    }

    pub async fn next(&mut self) -> WireEvent {
        loop {
            if let Some(event) = self.events.pop_front() {
                return event;
            }
            if self.handle.is_none() || self.disconnected {
                return std::future::pending().await;
            }
            self.begin_requests();
            tokio::select! {
                result = pending_mutation(&mut self.mutation) => {
                    self.mutation = None;
                    if let Err(error) = result {
                        match self.mutation_action.take() {
                            Some(SessionAction::Permission { permission_id, .. }) => {
                                if let Some(permission) = self.pending.get(&permission_id) {
                                    self.events.push_back(WireEvent::Permission(permission.clone()));
                                }
                            }
                            Some(SessionAction::Cancel) => self.cancelling = false,
                            _ => {}
                        }
                        if self.mutation_is_prompt {
                            self.working = false;
                            return WireEvent::Settled(Err(error));
                        }
                        return WireEvent::CancelFailed(format!("{error:#}"));
                    }
                }
                result = pending_poll(&mut self.poll) => {
                    self.poll = None;
                    if let Err(error) = result.and_then(|replay| self.ingest(replay)) {
                        self.disconnected = true;
                        self.events.push_back(WireEvent::Disconnected);
                        return WireEvent::Notice(format!("Session connection lost: {error:#}. The run remains available in Agents."));
                    }
                }
            }
        }
    }
    pub async fn shutdown(&mut self) -> bool {
        self.poll = None;
        self.actions.clear();
        if let Some(mutation) = self.mutation.take() {
            let _ = mutation.await;
        }
        let Some(handle) = self.handle.take() else {
            return true;
        };
        handle.client.stop().await.is_ok()
    }
    pub async fn detach(&mut self, graceful: bool) -> bool {
        self.poll = None;
        let Some(handle) = self.handle.take() else {
            return true;
        };
        // Lost authority must never turn a best-effort detach into a stop.
        if self.disconnected {
            return true;
        }
        if graceful {
            // An explicit detach must not silently discard a just-submitted
            // prompt that has not yet reached the daemon. Preserve action order
            // and wait only for acceptance, never for the agent turn to finish.
            if let Some(mutation) = self.mutation.take()
                && mutation.await.is_err()
            {
                return false;
            }
            while let Some(action) = self.actions.pop_front() {
                if handle.client.mutate(action).await.is_err() {
                    return false;
                }
            }
        } else {
            // Accepted commands already belong to the daemon. An unexpected
            // terminal loss must not keep this client alive awaiting a reply.
            self.mutation = None;
            self.actions.clear();
        }
        handle.client.detach().await.is_ok()
    }
}

async fn pending_mutation(task: &mut Option<MutationTask>) -> Result<()> {
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}
async fn pending_poll(task: &mut Option<PollTask>) -> Result<ReplayBatch> {
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::SessionEvent;
    fn batch(sequences: &[u64]) -> ReplayBatch {
        ReplayBatch {
            first_retained_seq: 1,
            snapshot_seq: sequences.last().copied().unwrap_or_default(),
            history_complete: true,
            events: sequences
                .iter()
                .map(|seq| SessionEvent {
                    seq: *seq,
                    at: chrono::Utc::now(),
                    kind: SessionEventKind::Activity {
                        text: "Working".into(),
                    },
                })
                .collect(),
        }
    }
    #[test]
    fn duplicate_delivery_is_not_reapplied() -> Result<()> {
        let mut wire = CodeWire::default();
        wire.ingest(batch(&[1, 2]))?;
        wire.ingest(batch(&[1, 2, 3]))?;
        assert_eq!(wire.cursor, 3);
        assert!(wire.events.is_empty());
        Ok(())
    }
    #[test]
    fn event_gap_is_rejected_without_advancing_cursor() {
        let mut wire = CodeWire::default();
        assert!(wire.ingest(batch(&[1, 3])).is_err());
        assert_eq!(wire.cursor, 0);
        assert!(wire.events.is_empty());
    }

    #[test]
    fn permission_resolution_and_settlement_are_delivered_once_in_order() -> Result<()> {
        let permission: PendingPermissionSnapshot = serde_json::from_value(serde_json::json!({
            "permission_id": "p1",
            "tool_call": {"toolCallId": "tool1", "title": "Read file"},
            "options": [{"optionId": "allow", "name": "Allow once", "kind": "allow_once"}]
        }))?;
        let mut replay = batch(&[1, 2, 3]);
        replay.events[0].kind = SessionEventKind::Permission { permission };
        replay.events[1].kind = SessionEventKind::PermissionResolved {
            permission_id: "p1".into(),
            option_id: Some("allow".into()),
            automatic: false,
        };
        replay.events[2].kind = SessionEventKind::TurnSettled {
            result: crate::supervisor::TurnResult {
                stop_reason: "end_turn".into(),
                result: None,
                schema_ok: None,
            },
        };
        let mut wire = CodeWire {
            working: true,
            ..Default::default()
        };
        wire.ingest(replay.clone())?;
        wire.ingest(replay)?;
        assert!(!wire.working());
        assert!(!wire.has_permissions());
        assert!(matches!(
            wire.events.pop_front(),
            Some(WireEvent::Permission(_))
        ));
        assert!(
            matches!(wire.events.pop_front(), Some(WireEvent::PermissionResolved(id)) if id == "p1")
        );
        assert!(
            matches!(wire.events.pop_front(), Some(WireEvent::Settled(Ok(response))) if response.stop_reason == agent_client_protocol::schema::v1::StopReason::EndTurn)
        );
        assert!(wire.events.is_empty());
        Ok(())
    }
}
