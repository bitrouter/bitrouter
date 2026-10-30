//! Finite durable correlation state for a fresh controller connection.

use super::{
    Append, Association, Descriptor, MAX_CALLS, MAX_CAPTURE_BYTES, MAX_EVENT_BYTES, MAX_EVENTS,
    MAX_SESSIONS, bounded_id, entities::connections,
};
use crate::storage::capture::{CaptureDirection as Direction, CaptureKind as Kind};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pending {
    pub(crate) method: String,
    pub(crate) direction: Direction,
    pub(crate) session: Option<String>,
}
/// Data written only after actual local-owner cleanup; not a live capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalCleanup {
    pub(crate) version: u32,
    pub(crate) origin_json: String,
    pub(crate) head: i64,
    pub(crate) metadata_digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub(crate) descriptor: Descriptor,
    pub(crate) connected: bool,
    pub(crate) closed: bool,
    pub(crate) complete: bool,
    pub(crate) head: i64,
    pub(crate) bytes: usize,
    pub(crate) calls: BTreeMap<u64, Pending>,
    pub(crate) sessions: BTreeMap<String, i64>,
    pub(crate) pinned: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) local_cleanup: Option<LocalCleanup>,
}

impl State {
    pub(crate) fn initial(association: &Association) -> Self {
        Self {
            descriptor: association.descriptor().clone(),
            connected: false,
            closed: false,
            complete: false,
            head: 0,
            bytes: 0,
            calls: BTreeMap::new(),
            sessions: BTreeMap::new(),
            pinned: BTreeSet::new(),
            local_cleanup: None,
        }
    }
    pub(crate) fn state_name(&self) -> &'static str {
        if self.complete {
            "complete"
        } else if self.closed {
            "interrupted"
        } else {
            "recording"
        }
    }
    pub(crate) fn validate(
        &self,
        association: &Association,
        row: &connections::Model,
    ) -> Result<()> {
        ensure!(
            &self.descriptor == association.descriptor()
                && row.head == self.head
                && row.state == self.state_name(),
            "capture metadata association mismatch"
        );
        ensure!(
            self.head >= 0
                && self.head <= i64::try_from(MAX_EVENTS)?
                && self.bytes <= MAX_CAPTURE_BYTES
                && self.calls.len() <= MAX_CALLS
                && self.sessions.len() <= MAX_SESSIONS,
            "capture metadata bound exceeded"
        );
        ensure!(
            !self.complete || (self.connected && self.closed && self.calls.is_empty()),
            "capture completion is inconsistent"
        );
        ensure!(
            self.connected
                || (self.calls.is_empty()
                    && self.sessions.is_empty()
                    && ((self.head == 0 && !self.closed)
                        || (self.head == 1 && self.closed && !self.complete))),
            "capture connection is inconsistent"
        );
        for (native, head) in &self.sessions {
            bounded_id(native, 256)?;
            ensure!(
                *head > 0 && *head <= self.head,
                "capture session watermark is invalid"
            );
        }
        ensure!(
            self.pinned.iter().all(|id| self.sessions.contains_key(id)),
            "pinned model session is absent"
        );
        for (id, pending) in &self.calls {
            ensure!(
                *id > 0 && pending.method.len() <= 128,
                "capture pending call is invalid"
            );
            if let Some(native) = &pending.session {
                ensure!(
                    self.sessions.contains_key(native),
                    "capture pending session is absent"
                );
            }
        }
        if let Some(cleanup) = &self.local_cleanup {
            ensure!(
                self.closed
                    && cleanup.version == 3
                    && cleanup.head == self.head
                    && !cleanup.origin_json.is_empty()
                    && cleanup.origin_json.len() <= 256,
                "capture cleanup marker shape mismatch"
            );
            // Full actual-origin decoding belongs to the stopped-owner consumer.
            // Ordinary capture only verifies the bounded immutable state link.
            let mut original = self.clone();
            original.local_cleanup = None;
            ensure!(
                crate::storage::evolution::digest(&original)? == cleanup.metadata_digest,
                "capture cleanup state digest mismatch"
            );
        }
        Ok(())
    }

    /// Caller applies this only to the retained event, before any DML. Live
    /// authority to forward is a separate owner/proxy check; a Scope is not it.
    pub(crate) fn next(&self, append: &Append) -> Result<(Self, Option<String>, bool)> {
        ensure!(
            !self.closed
                && append.sequence
                    == self
                        .head
                        .checked_add(1)
                        .context("capture sequence overflow")?,
            "capture append is not the next event"
        );
        let event = &append.event;
        bounded_id(&event.method, 128)?;
        let direct = event.payload.get("sessionId");
        let direct = direct
            .map(|v| v.as_str().context("native session ID is not a string"))
            .transpose()?;
        if let Some(native) = direct {
            bounded_id(native, 256)?;
        }
        let mut state = self.clone();
        state.head = append.sequence;
        state.bytes = state
            .bytes
            .checked_add(append.bytes.len())
            .context("capture byte count overflow")?;
        ensure!(
            state.bytes <= MAX_CAPTURE_BYTES,
            "capture connection byte bound exceeded"
        );
        let mut native = direct.map(str::to_owned);
        let mut created = false;
        match event.kind {
            Kind::Connected => {
                ensure!(
                    !self.connected
                        && self.head == 0
                        && event.direction == Direction::Controller
                        && event.method == "controller/connect"
                        && event.call_id.is_none()
                        && direct.is_none(),
                    "invalid capture connect"
                );
                state.connected = true;
            }
            Kind::Request => {
                ensure!(
                    self.connected && event.direction != Direction::Controller,
                    "invalid capture request"
                );
                let id = event
                    .call_id
                    .filter(|id| *id > 0)
                    .context("capture request call missing")?;
                ensure!(
                    !self.calls.contains_key(&id) && self.calls.len() < MAX_CALLS,
                    "capture pending call bound or duplicate"
                );
                match (event.direction, event.method.as_str()) {
                    (Direction::Client, "session/new") => {
                        ensure!(
                            direct.is_none() && self.sessions.len() < MAX_SESSIONS,
                            "capture session creation rejected"
                        );
                    }
                    (Direction::Client, "session/prompt" | "session/set_config_option")
                    | (
                        Direction::Agent,
                        "session/request_permission"
                        | "fs/read_text_file"
                        | "fs/write_text_file"
                        | "terminal/create"
                        | "terminal/output"
                        | "terminal/release"
                        | "terminal/wait_for_exit"
                        | "terminal/kill",
                    ) => {
                        ensure!(
                            direct.is_some_and(|id| self.sessions.contains_key(id)),
                            "capture request session is not associated"
                        );
                    }
                    _ => anyhow::bail!("ACP method is unsupported by the owned capture profile"),
                }
                if event.method == "session/prompt" {
                    ensure!(
                        direct.is_some_and(|id| self.pinned.contains(id)),
                        "session fixed model is unconfirmed"
                    );
                }
                if event.method == "session/set_config_option" {
                    ensure!(
                        event
                            .payload
                            .get("configId")
                            .and_then(serde_json::Value::as_str)
                            == Some("model")
                            && event
                                .payload
                                .get("value")
                                .and_then(serde_json::Value::as_str)
                                == Some(self.descriptor.model.as_str()),
                        "model override differs from retained route"
                    );
                }
                state.calls.insert(
                    id,
                    Pending {
                        method: event.method.clone(),
                        direction: event.direction,
                        session: native.clone(),
                    },
                );
            }
            Kind::Response => {
                let id = event.call_id.context("capture response call missing")?;
                let pending = self
                    .calls
                    .get(&id)
                    .context("capture response has no admitted request")?;
                ensure!(
                    pending.method == event.method
                        && opposite(pending.direction) == event.direction
                        && direct.is_none(),
                    "capture response correlation mismatch"
                );
                let object = event
                    .payload
                    .as_object()
                    .context("capture response is not an object")?;
                ensure!(
                    object.len() == 1
                        && (object.contains_key("result") || object.contains_key("error")),
                    "capture response wrapper is malformed"
                );
                native = pending.session.clone();
                if event.method == "session/new"
                    && let Some(result) = object.get("result")
                {
                    let id = result
                        .get("sessionId")
                        .and_then(serde_json::Value::as_str)
                        .context("new session result has no ID")?;
                    bounded_id(id, 256)?;
                    ensure!(
                        !state.sessions.contains_key(id) && state.sessions.len() < MAX_SESSIONS,
                        "native session ID reused or capture session bound exceeded"
                    );
                    state.sessions.insert(id.to_owned(), 0);
                    native = Some(id.to_owned());
                    created = true;
                }
                if event.method == "session/set_config_option"
                    && let Some(result) = object.get("result")
                {
                    let options = result
                        .get("configOptions")
                        .and_then(serde_json::Value::as_array)
                        .context("model confirmation options absent")?;
                    ensure!(
                        options
                            .iter()
                            .filter(
                                |option| option.get("id").and_then(serde_json::Value::as_str)
                                    == Some("model")
                            )
                            .count()
                            == 1
                            && options.iter().any(|option| option
                                .get("id")
                                .and_then(serde_json::Value::as_str)
                                == Some("model")
                                && option
                                    .get("currentValue")
                                    .and_then(serde_json::Value::as_str)
                                    == Some(self.descriptor.model.as_str())),
                        "harness did not confirm retained model"
                    );
                    state.pinned.insert(
                        native
                            .clone()
                            .context("model confirmation session absent")?,
                    );
                }
                state.calls.remove(&id);
            }
            Kind::Notification => {
                ensure!(
                    self.connected
                        && event.call_id.is_none()
                        && direct.is_some_and(|id| self.sessions.contains_key(id)),
                    "capture notification session is not associated"
                );
                ensure!(
                    matches!(
                        (event.direction, event.method.as_str()),
                        (Direction::Agent, "session/update")
                            | (Direction::Client, "session/cancel")
                    ),
                    "unsupported owned capture notification"
                );
            }
            Kind::Disconnected => {
                ensure!(
                    (self.connected || self.head == 0)
                        && event.direction == Direction::Controller
                        && event.method == "controller/disconnect"
                        && event.call_id.is_none()
                        && direct.is_none(),
                    "invalid capture disconnect"
                );
                state.closed = true;
                state.complete = self.connected
                    && event
                        .payload
                        .get("clean")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                    && state.calls.is_empty();
            }
        }
        if event.kind != Kind::Disconnected {
            // Admission leaves enough durable space for each already-admitted
            // maximum-size response and one interrupted terminal observation.
            let reserved_bytes = state
                .calls
                .len()
                .checked_mul(MAX_EVENT_BYTES)
                .and_then(|value| value.checked_add(1024))
                .context("capture reserve overflow")?;
            ensure!(
                state
                    .bytes
                    .checked_add(reserved_bytes)
                    .is_some_and(|value| value <= MAX_CAPTURE_BYTES)
                    && usize::try_from(state.head)?
                        .checked_add(state.calls.len() + 1)
                        .is_some_and(|value| value <= MAX_EVENTS),
                "capture terminal reserve exhausted"
            );
        }
        if let Some(id) = &native {
            let head = state
                .sessions
                .get_mut(id)
                .context("capture session is absent")?;
            *head = head
                .checked_add(1)
                .context("capture session head overflow")?;
        }
        Ok((state, native, created))
    }
}

fn opposite(direction: Direction) -> Direction {
    match direction {
        Direction::Client => Direction::Agent,
        Direction::Agent => Direction::Client,
        Direction::Controller => Direction::Controller,
    }
}
