//! The owning host retains this port and exact pending append across waiter loss.
use super::{
    Binding,
    queue::{Outcome, Reply, Request},
};
use crate::{
    harness::{
        acp::capture::{CaptureError, CapturePort},
        trajectory::owned::{Append, protocol::State as Correlation},
    },
    storage::capture::{CaptureDirection, CaptureEvent, CaptureKind},
    tenancy::transaction::Disposition,
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};
use tokio::time::Instant;

struct Pending {
    input: Request,
    deadline: Instant,
    receive: Option<oneshot::Receiver<Outcome>>,
}
struct State {
    current: Correlation,
    pending: Option<Pending>,
    created: bool,
    bootstrap: u8,
    failed: bool,
    ambiguous: bool,
}
pub(crate) struct Port {
    binding: Arc<Binding>,
    state: Mutex<State>,
}
impl Port {
    pub(crate) fn new(binding: Arc<Binding>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                current: Correlation::initial(binding.association()),
                pending: None,
                created: false,
                bootstrap: 0,
                failed: false,
                ambiguous: false,
            }),
            binding,
        })
    }
    pub(crate) fn deliver_observed(
        &self,
        work: impl FnOnce() -> Result<(), agent_client_protocol::Error>,
    ) -> Result<(), agent_client_protocol::Error> {
        self.binding.deliver_observed(work).map_err(|_| {
            agent_client_protocol::Error::invalid_request().data("owned ACP ingress closed")
        })?
    }
    pub(crate) fn deliver_new(
        &self,
        work: impl FnOnce() -> Result<(), agent_client_protocol::Error>,
    ) -> Result<(), agent_client_protocol::Error> {
        self.binding.with_start(work).map_err(|_| {
            agent_client_protocol::Error::invalid_request()
                .data("owned forwarding authority changed")
        })?
    }
    pub(crate) async fn create(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        self.reconcile_predecessor(&mut state).await?;
        self.call(&mut state, Request::Create(chrono::Utc::now().to_rfc3339()))
            .await?;
        Ok(())
    }
    pub(crate) async fn initialized(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        self.reconcile_predecessor(&mut state).await?;
        ensure!(state.bootstrap == 2, "owned provider bootstrap incomplete");
        self.binding
            .check()
            .map_err(|_| anyhow::anyhow!("capture authority rejected"))?;
        state.bootstrap = 3;
        Ok(())
    }
    pub(crate) async fn check_forward(
        &self,
        direction: CaptureDirection,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<()> {
        // Initialization never loads history. All other unknown methods, including
        // list/load/fork/resume, fail before the capture-skipping forwarding branch.
        let mut state = self.state.lock().await;
        self.reconcile_predecessor(&mut state).await?;
        let terminal_notification = matches!(
            (direction, method),
            (CaptureDirection::Agent, "session/update")
                | (CaptureDirection::Client, "session/cancel")
        ) && params
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|native| {
                state.current.calls.values().any(|call| {
                    call.method == "session/prompt" && call.session.as_deref() == Some(native)
                })
            });
        if !terminal_notification {
            self.binding
                .check()
                .map_err(|_| anyhow::anyhow!("capture authority rejected"))?;
        }
        ensure!(
            !state.failed && !state.ambiguous && !state.current.closed,
            "capture is closing"
        );
        // These two requests originate in the shared Controller initialization
        // gate. The public facade never exposes the inner ACP client or provider
        // configuration. Permit exactly its set/list handshake before admission.
        if direction == CaptureDirection::Client
            && state.current.sessions.is_empty()
            && ((state.bootstrap == 0 && method == "providers/set")
                || (state.bootstrap == 1 && method == "providers/list"))
        {
            state.bootstrap += 1;
            return Ok(());
        }
        ensure!(state.bootstrap == 3, "owned provider bootstrap unavailable");
        if direction == CaptureDirection::Client && method == "session/new" {
            return Ok(());
        }
        let native = params
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .context("owned session association required")?;
        ensure!(
            state.current.sessions.contains_key(native),
            "owned session association unavailable"
        );
        ensure!(
            matches!(
                (direction, method),
                (
                    CaptureDirection::Client,
                    "session/prompt" | "session/cancel" | "session/set_config_option"
                ) | (
                    CaptureDirection::Agent,
                    "session/update"
                        | "session/request_permission"
                        | "fs/read_text_file"
                        | "fs/write_text_file"
                        | "terminal/create"
                        | "terminal/output"
                        | "terminal/release"
                        | "terminal/wait_for_exit"
                        | "terminal/kill"
                )
            ),
            "unsupported owned ACP method"
        );
        Ok(())
    }
    pub(crate) async fn checkpoint(
        &self,
        native: &str,
    ) -> Result<Option<crate::acp_trajectory::checkpoint::owned::Summary>> {
        let mut state = self.state.lock().await;
        self.reconcile_predecessor(&mut state).await?;
        self.binding
            .check()
            .map_err(|_| anyhow::anyhow!("capture authority rejected"))?;
        ensure!(
            !state.failed && state.current.sessions.contains_key(native),
            "owned session association unavailable"
        );
        let summary = match self
            .call(&mut state, Request::Freeze(native.to_owned()))
            .await?
        {
            Reply::Frozen(summary) => summary,
            _ => anyhow::bail!("capture checkpoint reply mismatch"),
        };
        drop(state);
        if let Some(summary) = &summary
            && self.binding.assessment().is_some()
        {
            crate::tenancy::learning::checkpoint(self.binding.clone(), native, summary).await?;
        }
        Ok(summary)
    }
    async fn call(&self, state: &mut State, input: Request) -> Result<Reply> {
        ensure!(
            state.pending.is_none(),
            "capture pending append must be reconciled by its owner"
        );
        let human = matches!(&input, Request::Human(_));
        let deadline = Instant::now() + Duration::from_secs(5);
        state.pending = Some(Pending {
            input: input.clone(),
            deadline,
            receive: None,
        });
        match self.binding.enqueue_before(input, deadline).await {
            Ok(receive) => {
                state
                    .pending
                    .as_mut()
                    .context("capture pending input disappeared")?
                    .receive = Some(receive)
            }
            Err(error) => {
                state.pending = None;
                if !human {
                    state.failed = true;
                }
                return Err(crate::storage::operation_budget::Error::Rejected(error.0.code).into());
            }
        }
        self.settle(state).await
    }
    async fn reconcile_predecessor(&self, state: &mut State) -> Result<()> {
        ensure!(!state.ambiguous, "capture SQL cleanup unconfirmed");
        let Some(pending) = &state.pending else {
            return Ok(());
        };
        let human = matches!(&pending.input, Request::Human(_));
        if human && pending.receive.is_none() {
            // enqueue cannot yield after send: no receiver means this Human
            // request was never admitted. Ordinary capture keeps its policy.
            state.pending = None;
            return Ok(());
        }
        let result = self.settle(state).await;
        if human && state.pending.is_none() && !state.failed && !state.ambiguous {
            // A previous caller's known result is not this callback's reply.
            drop(result);
            return Ok(());
        }
        result.map(|_| ())
    }

    pub(crate) async fn human(
        &self,
        native: &str,
        input: super::human::Input,
    ) -> Result<super::human::Reply> {
        let request = Arc::new(super::human::Request::new(
            native,
            input,
            self.binding.association(),
        )?);
        let mut state = self.state.try_lock().map_err(|_| {
            crate::storage::operation_budget::Error::Rejected(
                bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded,
            )
        })?;
        self.reconcile_predecessor(&mut state).await?;
        self.binding
            .check()
            .map_err(|error| crate::storage::operation_budget::Error::Rejected(error.0.code))?;
        ensure!(
            !state.failed && !state.ambiguous && !state.current.closed,
            "controlled capture is closing"
        );
        let reply = self.call(&mut state, Request::Human(request)).await?;
        self.binding
            .check()
            .map_err(|error| crate::storage::operation_budget::Error::Rejected(error.0.code))?;
        match reply {
            Reply::Human(reply, _) => Ok(*reply),
            _ => anyhow::bail!("human reply mismatch"),
        }
    }

    async fn settle(&self, state: &mut State) -> Result<Reply> {
        ensure!(!state.ambiguous, "capture SQL cleanup unconfirmed");
        let pending = state
            .pending
            .as_mut()
            .context("capture pending operation missing")?;
        let Some(receive) = &mut pending.receive else {
            state.pending = None;
            state.failed = true;
            anyhow::bail!("capture enqueue was interrupted");
        };
        // Consume an already-ready result even at the original deadline. No
        // new callback can renew the actor's operation or H cleanup deadline.
        let (received, completed) = match receive.try_recv() {
            Ok(outcome) => (Ok(outcome), true),
            Err(oneshot::error::TryRecvError::Closed) => (Err(()), true),
            Err(oneshot::error::TryRecvError::Empty)
                if !matches!(&pending.input, Request::Human(_)) =>
            {
                (receive.await.map_err(|_| ()), true)
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                match tokio::time::timeout_at(
                    pending.deadline + crate::tenancy::transaction::CLEANUP_TIMEOUT,
                    receive,
                )
                .await
                {
                    Ok(result) => (result.map_err(|_| ()), true),
                    Err(_) => (Err(()), false),
                }
            }
        };
        // A canceled wait keeps the same receiver and original deadline. A
        // resolved receiver is retired before any fallible outcome handling;
        // the exact Pending input still survives ambiguous custody. Timeout
        // retains the unresolved receiver, but the sticky guard forbids repoll.
        if completed {
            pending.receive = None;
        }
        let outcome = match received {
            Ok(outcome) => outcome,
            Err(()) => {
                // Port custody is unconfirmed; this does not set H's SQL phase.
                state.ambiguous = true;
                self.binding.service().host_cleanup_unknown();
                anyhow::bail!("capture actor completion unconfirmed");
            }
        };
        if matches!(
            outcome.disposition,
            Disposition::Pending | Disposition::Ambiguous
        ) {
            state.ambiguous = true;
            self.binding.service().host_cleanup_unknown();
            anyhow::bail!("capture transaction outcome is ambiguous");
        }
        if let Request::Human(request) = &pending.input {
            if let Ok(reply) = &outcome.result
                && !matches!(reply, Reply::Human(value, wrote) if request.matches(value)
                    && outcome.disposition == if *wrote { Disposition::Committed } else { Disposition::RolledBack })
            {
                state.ambiguous = true;
                self.binding.service().host_cleanup_unknown();
                anyhow::bail!("human actor reply does not match its request");
            }
            let ordinary = outcome.result.as_ref().err().is_none_or(|error| {
                use bitrouter_ai::tenancy::operation::ErrorCode;
                matches!(
                    error.0.code,
                    ErrorCode::InvalidRequest
                        | ErrorCode::NotFound
                        | ErrorCode::RevisionConflict
                        | ErrorCode::CapacityExceeded
                        | ErrorCode::PayloadTooLarge
                        | ErrorCode::Unauthorized
                        | ErrorCode::Forbidden
                        | ErrorCode::TenantInactive
                        | ErrorCode::AuthorityUnavailable
                        | ErrorCode::CapabilityMismatch
                )
            });
            state.pending = None;
            if !ordinary {
                state.failed = true;
            }
            return outcome.result.map_err(|error| {
                crate::storage::operation_budget::Error::Rejected(error.0.code).into()
            });
        }
        if outcome.disposition == Disposition::Committed {
            match &pending.input {
                Request::Create(_) => state.created = true,
                Request::Append { value, .. } => match state.current.next(value) {
                    Ok((next, _, _)) => state.current = next,
                    Err(error) => {
                        state.ambiguous = true;
                        self.binding.service().host_cleanup_unknown();
                        return Err(error);
                    }
                },
                Request::Freeze(_) => {}
                Request::Human(_) => anyhow::bail!("unexpected human capture transition"),
                Request::Block(_) => anyhow::bail!("HTTP Block intent cannot be a capture append"),
            }
        }
        state.pending = None;
        match outcome.result {
            Ok(reply) => Ok(reply),
            Err(_) => {
                state.failed = true;
                anyhow::bail!("capture operation rejected");
            }
        }
    }
    /// Called only after the controller and child have stopped producing. The
    /// receiver remains in this port even when a proxy callback was dropped.
    pub(crate) async fn finish(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        // Preserve ordinary capture's owner-bound disconnect after a known
        // rejection; unknown H/custody still prevents any new append.
        let predecessor = self.reconcile_predecessor(&mut state).await;
        if state.pending.is_some() || state.ambiguous {
            predecessor?;
            anyhow::bail!("capture SQL cleanup unconfirmed");
        }
        drop(predecessor);
        if state.created && !state.current.closed {
            let append = Arc::new(Append::new(
                state.current.head + 1,
                chrono::Utc::now().to_rfc3339(),
                CaptureEvent {
                    direction: CaptureDirection::Controller,
                    kind: CaptureKind::Disconnected,
                    call_id: None,
                    method: "controller/disconnect".into(),
                    payload: serde_json::json!({"clean": false}),
                },
            )?);
            self.call(
                &mut state,
                Request::Append {
                    value: append,
                    cleanup: true,
                },
            )
            .await?;
        }
        ensure!(state.pending.is_none(), "capture append remains pending");
        Ok(())
    }
}

#[async_trait]
impl CapturePort for Port {
    async fn record(&self, mut event: CaptureEvent) -> Result<(), CaptureError> {
        let result: Result<()> = async {
            let mut state = self.state.lock().await;
            self.reconcile_predecessor(&mut state).await?;
            ensure!(!state.failed, "capture connection is incomplete");
            if event.kind == CaptureKind::Disconnected && self.binding.ingress_rejected() {
                event.payload = serde_json::json!({"clean": false});
            }
            let cleanup = matches!(
                event.kind,
                CaptureKind::Response | CaptureKind::Disconnected
            ) || (event.kind == CaptureKind::Notification
                && self.binding.check().is_err()
                && matches!(event.method.as_str(), "session/update" | "session/cancel"));
            if !cleanup {
                self.binding
                    .check()
                    .map_err(|_| anyhow::anyhow!("capture authority rejected"))?;
            }
            let append = Arc::new(Append::new(
                state.current.head + 1,
                chrono::Utc::now().to_rfc3339(),
                event,
            )?);
            // Reject unsupported input before any SQL or actor unavailability.
            state.current.next(&append)?;
            match self
                .call(
                    &mut state,
                    Request::Append {
                        value: append,
                        cleanup,
                    },
                )
                .await?
            {
                Reply::Appended(value) if value.head == state.current.head => Ok(()),
                _ => anyhow::bail!("capture append reply mismatch"),
            }
        }
        .await;
        result.map_err(|_| CaptureError("owned durable observation unavailable".into()))
    }
}
