//! Connection-local lifecycle selection, independently corroborated by native
//! rollout records. A lifecycle path alone is never a file-opening authority.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session_evidence::store::rollouts::{RolloutIdentity, rollout_id};
use crate::session_evidence::types::{MAX_GRAPH_ITEMS, RecordRef, SourceRange, identifier};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutObservation {
    pub thread_id: String,
    pub rollout_id: String,
    pub path: String,
    pub method: String,
    pub request: Option<RecordRef>,
    pub response: RecordRef,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodexHistoryEvidence {
    pub lifecycle: Option<RolloutObservation>,
    pub source: Option<RolloutIdentity>,
    /// Original, source-local turn context records; copied replacement messages
    /// and a parent's inherited prefix cannot substitute for these records.
    pub turn_contexts: Vec<RecordRef>,
    pub inspected: Vec<SourceRange>,
    pub gaps: BTreeSet<String>,
}

impl CodexHistoryEvidence {
    fn missing() -> Self {
        Self {
            gaps: BTreeSet::from(["native_input_rollout_unselected".into()]),
            ..Default::default()
        }
    }
}

#[derive(Default)]
struct ThreadState {
    epoch: u64,
    pending: usize,
    active: Option<RolloutObservation>,
    known: bool,
    expected_started: Vec<String>,
    expected_reverts: usize,
}

pub(super) struct Request {
    thread: Option<String>,
    epoch: u64,
    mutation: bool,
    overlap: bool,
    alternative: bool,
    inline: bool,
    reference: RecordRef,
    history: Option<CodexHistoryEvidence>,
}

#[derive(Default)]
pub(super) struct Scanner {
    threads: BTreeMap<String, ThreadState>,
    poisoned: bool,
    pending_notifications: usize,
}

impl Scanner {
    fn state(&mut self, thread: &str) -> Result<&mut ThreadState> {
        identifier(thread)?;
        ensure!(
            self.threads.contains_key(thread) || self.threads.len() < MAX_GRAPH_ITEMS,
            "native rollout lifecycle thread limit"
        );
        Ok(self.threads.entry(thread.into()).or_default())
    }

    pub(super) fn poison(&mut self) {
        // An overlapping RPC id can hide either request's response method. No
        // subsequent path on this connection repairs that missing provenance.
        self.poisoned = true;
    }

    pub(super) fn request(
        &mut self,
        method: &str,
        payload: &Value,
        reference: RecordRef,
    ) -> Result<Request> {
        let thread = payload.get("threadId").and_then(Value::as_str);
        let mutation = matches!(
            method,
            "thread/resume"
                | "thread/revert"
                | "thread/rollback"
                | "thread/archive"
                | "thread/unarchive"
                | "thread/delete"
                | "thread/unsubscribe"
        );
        let mut request = Request {
            thread: thread.map(str::to_owned),
            epoch: 0,
            mutation,
            overlap: false,
            alternative: payload
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| !path.is_empty()),
            inline: payload.get("bitrouter_inline_history") == Some(&Value::Bool(true)),
            reference,
            history: (method == "turn/start").then(CodexHistoryEvidence::missing),
        };
        if let Some(thread) = thread {
            let poisoned = self.poisoned;
            let state = self.state(thread)?;
            request.overlap = state.pending > 0 && (mutation || request.history.is_some());
            if mutation {
                state.epoch = request.reference.range.start + 1;
                state.pending += 1;
                state.active = None;
            }
            request.epoch = state.epoch;
            if let Some(history) = &mut request.history
                && !poisoned
                && state.pending == 0
                && let Some(active) = &state.active
            {
                history.lifecycle = Some(active.clone());
                history.gaps.clear();
            }
        }
        Ok(request)
    }

    pub(super) fn response(
        &mut self,
        method: &str,
        payload: &Value,
        request: Request,
        reference: RecordRef,
    ) -> Result<Option<CodexHistoryEvidence>> {
        let mut admissible = !self.poisoned && !request.overlap;
        if let Some(thread) = &request.thread
            && (request.mutation || request.history.is_some())
        {
            let state = self.state(thread)?;
            if request.mutation {
                state.pending = state.pending.saturating_sub(1);
            }
            admissible &= state.epoch == request.epoch && state.pending == 0;
        }
        if let Some(mut history) = request.history {
            if !admissible {
                history.lifecycle = None;
                history
                    .gaps
                    .insert("native_input_rollout_transition_overlap".into());
            }
            return Ok(Some(history));
        }
        if !matches!(
            method,
            "thread/start" | "thread/resume" | "thread/fork" | "thread/revert" | "thread/rollback"
        ) || payload.get("error_code").is_some()
        {
            return Ok(None);
        }
        // Resume path/history can override the requested thread id. Revert's
        // error can follow a runtime replacement, so a failed RPC never restores
        // the previous path. Protocol and producer contract:
        // https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/app-server/src/request_processors/thread_processor.rs
        let Some(thread) = payload.pointer("/thread/id").and_then(Value::as_str) else {
            return Ok(None);
        };
        let same_thread = !matches!(
            method,
            "thread/resume" | "thread/revert" | "thread/rollback"
        ) || (method == "thread/resume" && request.alternative)
            || request.thread.as_deref() == Some(thread);
        let selected = observation(method, payload, Some(request.reference.clone()), reference);
        let capacity = self.pending_notifications < MAX_GRAPH_ITEMS;
        let state = self.state(thread)?;
        let started = state
            .active
            .as_ref()
            .zip(selected.as_ref())
            .is_some_and(|(old, new)| {
                !state.known
                    && old.method == "thread/started"
                    && old.path == new.path
                    && old.response.range.start > request.reference.range.start
            });
        if !request.mutation || request.thread.as_deref() != Some(thread) {
            // A late creation response cannot overwrite a more recent close,
            // resume or revert of that returned thread.
            admissible &=
                (state.epoch <= request.reference.range.start || started) && state.pending == 0;
        }
        let mut added = 0;
        if same_thread
            && matches!(method, "thread/start" | "thread/fork")
            && !started
            && let Some(selected) = &selected
        {
            ensure!(capacity, "native lifecycle notification limit");
            state.expected_started.push(selected.path.clone());
            added = 1;
        } else if same_thread && method == "thread/revert" {
            ensure!(capacity, "native lifecycle notification limit");
            state.expected_reverts += 1;
            added = 1;
        }
        if admissible && same_thread && !request.inline {
            state.active = selected;
            state.known = true;
        }
        self.pending_notifications += added;
        Ok(None)
    }

    pub(super) fn notification(
        &mut self,
        method: &str,
        payload: &Value,
        reference: RecordRef,
    ) -> Result<()> {
        if method == "thread/started" {
            let Some(thread) = payload.pointer("/thread/id").and_then(Value::as_str) else {
                return Ok(());
            };
            let poisoned = self.poisoned;
            let state = self.state(thread)?;
            let selected = observation(method, payload, None, reference);
            if let Some(index) = selected.as_ref().and_then(|selected| {
                state
                    .expected_started
                    .iter()
                    .position(|path| path == &selected.path)
            }) {
                // Each creation response has a separately awaited notification.
                // Its delayed notification must not overwrite a later revert.
                state.expected_started.remove(index);
                self.pending_notifications = self.pending_notifications.saturating_sub(1);
                return Ok(());
            }
            // Start notifications can precede/follow the response. Keep the
            // stronger correlated response when both describe the same rollout.
            if !poisoned {
                if state
                    .active
                    .as_ref()
                    .zip(selected.as_ref())
                    .is_some_and(|(old, new)| old.path == new.path)
                {
                    return Ok(());
                }
                let initial = !state.known && state.epoch == 0 && state.pending == 0;
                state.epoch = selected
                    .as_ref()
                    .map_or(state.epoch + 1, |item| item.response.range.start + 1);
                state.active = if initial { selected } else { None };
            }
        } else if matches!(
            method,
            "thread/reverted"
                | "thread/closed"
                | "thread/archived"
                | "thread/unarchived"
                | "thread/deleted"
        ) {
            let Some(thread) = payload.get("threadId").and_then(Value::as_str) else {
                return Ok(());
            };
            let state = self.state(thread)?;
            // The pinned producer emits its revert response before this
            // path-less notification. A notification without that response
            // invalidates the cached path (e.g. another connected client).
            if method == "thread/reverted" && state.expected_reverts > 0 {
                state.expected_reverts -= 1;
                self.pending_notifications = self.pending_notifications.saturating_sub(1);
            } else {
                state.epoch = reference.range.start + 1;
                state.active = None;
            }
        }
        Ok(())
    }
}

fn observation(
    method: &str,
    payload: &Value,
    request: Option<RecordRef>,
    response: RecordRef,
) -> Option<RolloutObservation> {
    let thread = payload.get("thread")?;
    if thread.get("ephemeral") == Some(&Value::Bool(true)) {
        return None;
    }
    let thread_id = thread.get("id")?.as_str()?;
    let path = thread.get("path")?.as_str()?;
    let native_path = Path::new(path);
    if path.len() > 8192 || path.contains('\0') || !native_path.is_absolute() {
        return None;
    }
    let rollout_id = rollout_id(native_path.file_name()?.to_str()?, thread_id)?;
    Some(RolloutObservation {
        thread_id: thread_id.into(),
        rollout_id,
        path: path.into(),
        method: method.into(),
        request,
        response,
    })
}
