//! ACP turn ownership for the interactive Code loop.
//!
//! Cancellation retains the prompt task until its response or grace expiry.

use std::collections::HashMap;
use std::pin::Pin;

use agent_client_protocol::schema::v1::{PromptResponse, RequestPermissionOutcome, SessionUpdate};
use anyhow::{Result, ensure};
use bitrouter_sdk::acp::client::PendingPermission;
use futures::{FutureExt, Stream, StreamExt};

use crate::acp_cli::SessionHandle;

type TurnTask = tokio::task::JoinHandle<Result<PromptResponse>>;
type CancelFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type ClosedFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

pub(crate) enum WireEvent {
    Update(SessionUpdate),
    Permission(PendingPermission),
    Settled(Result<PromptResponse>),
    CancellationExpired,
    CancelFailed(String),
    Disconnected,
}

pub(crate) struct CodeWire {
    pub handle: Option<SessionHandle>,
    updates: Pin<Box<dyn Stream<Item = SessionUpdate> + Send>>,
    permissions: Pin<Box<dyn Stream<Item = PendingPermission> + Send>>,
    updates_open: bool,
    permissions_open: bool,
    pending: HashMap<String, PendingPermission>,
    turn: Option<TurnTask>,
    cancel_request: Option<CancelFuture>,
    cancel_deadline: Option<tokio::time::Instant>,
    cancel_sent: bool,
    /// Generic teardown denies outstanding permissions before it asks the
    /// agent to stop. This stays false for an explicit user cancellation,
    /// whose pending permissions must retain ACP's `Cancelled` outcome.
    tearing_down: bool,
    settled: Option<Result<PromptResponse>>,
    closed: Option<ClosedFuture>,
    disconnected: bool,
}

impl Default for CodeWire {
    fn default() -> Self {
        Self {
            handle: None,
            updates: Box::pin(futures::stream::empty()),
            permissions: Box::pin(futures::stream::empty()),
            updates_open: false,
            permissions_open: false,
            pending: HashMap::new(),
            turn: None,
            cancel_request: None,
            cancel_deadline: None,
            cancel_sent: false,
            tearing_down: false,
            settled: None,
            closed: None,
            disconnected: false,
        }
    }
}

impl CodeWire {
    pub fn attach(&mut self, mut handle: SessionHandle) {
        self.updates = handle.take_updates();
        self.permissions = handle.take_permissions();
        self.updates_open = true;
        self.permissions_open = true;
        self.closed = Some(handle.closed());
        self.handle = Some(handle);
    }

    /// Reopening a session keeps the single permission receiver already held.
    pub fn reattach(&mut self, mut handle: SessionHandle) {
        self.updates = handle.take_updates();
        self.updates_open = true;
        self.closed = Some(handle.closed());
        self.handle = Some(handle);
    }

    pub fn working(&self) -> bool {
        self.turn.is_some() || self.settled.is_some()
    }
    pub fn cancelling(&self) -> bool {
        self.cancel_sent
    }
    pub fn has_permissions(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn submit(&mut self, prompt: String) -> Result<()> {
        ensure!(
            !self.working() && !self.cancelling(),
            "Wait for the current turn to settle"
        );
        ensure!(
            self.pending.is_empty(),
            "Answer the pending permission before submitting"
        );
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Choose an agent before submitting"))?;
        let client = handle.client.clone();
        let id = handle.session_id.clone();
        self.turn = Some(tokio::spawn(
            async move { client.prompt(&id, &prompt).await },
        ));
        Ok(())
    }

    pub fn resolve(&mut self, id: &str, outcome: RequestPermissionOutcome) {
        if let Some(permission) = self.pending.remove(id) {
            permission.resolve(outcome);
        }
    }

    pub fn cancel(&mut self) {
        if self.turn.is_none() || self.cancelling() {
            return;
        }
        self.cancel_pending();
        if let Some(handle) = &self.handle {
            handle.client.cancel_session_permissions(&handle.session_id);
        }
        self.begin_cooperative_cancel();
    }

    fn cancel_pending(&mut self) {
        for (_, permission) in self.pending.drain() {
            permission.resolve(RequestPermissionOutcome::Cancelled);
        }
    }

    fn deny_pending(&mut self) {
        for (_, permission) in self.pending.drain() {
            permission.deny();
        }
    }

    /// Start the one `session/cancel` notification without deciding any
    /// permission outcome. Explicit user cancellation and generic teardown
    /// choose their distinct outcomes before reaching this shared step.
    fn begin_cooperative_cancel(&mut self) {
        if self.turn.is_none() || self.cancelling() {
            return;
        }
        let request = self
            .handle
            .as_ref()
            .map(|handle| (handle.client.clone(), handle.session_id.clone()));
        self.cancel_sent = true;
        self.cancel_deadline = Some(
            tokio::time::Instant::now()
                + bitrouter_sdk::acp::client::AcpClient::cancellation_grace(),
        );
        if let Some((client, id)) = request {
            self.cancel_request = Some(Box::pin(async move { client.cancel(&id).await }));
        }
    }

    /// Begin a connection teardown. This is an abandonment path, so it denies
    /// both items still retained by the UI and unresolved entries in the
    /// client's ledger before a cooperative prompt cancellation is sent.
    fn begin_teardown(&mut self) {
        if self.cancelling() {
            // An explicit user cancellation already chose `Cancelled` for its
            // turn. Do not rewrite a later request from that turn as denial.
            return;
        }
        self.tearing_down = true;
        self.deny_outstanding();
        self.begin_cooperative_cancel();
    }

    fn deny_outstanding(&mut self) {
        self.deny_pending();
        if let Some(handle) = &self.handle {
            handle.client.deny_session_permissions(&handle.session_id);
        }
    }

    fn receive_permission(&mut self, permission: PendingPermission) -> Option<WireEvent> {
        if self.tearing_down {
            permission.deny();
            return None;
        }
        if self.cancelling() {
            permission.resolve(RequestPermissionOutcome::Cancelled);
            return None;
        }
        if self.pending.contains_key(&permission.request_id) {
            return None;
        }
        self.pending
            .insert(permission.request_id.clone(), permission.clone());
        Some(WireEvent::Permission(permission))
    }

    /// Poll the one cancellation notification before its grace deadline. This
    /// deliberately checks the send first: when both become ready in the same
    /// scheduler tick, a prompt result already retained in `settled` is the
    /// actual outcome and must not be replaced by a timeout/disconnect state.
    fn poll_cancellation_send(&mut self) -> Option<WireEvent> {
        if self.cancel_request.is_some()
            && let Some(result) = pending_cancel(&mut self.cancel_request).now_or_never()
        {
            self.cancel_request = None;
            if let Err(error) = result {
                return Some(WireEvent::CancelFailed(format!("{error:#}")));
            }
        }
        if self.cancel_request.is_some()
            && cancellation_deadline(self.cancel_deadline)
                .now_or_never()
                .is_some()
        {
            return Some(WireEvent::CancellationExpired);
        }
        None
    }

    pub async fn next(&mut self) -> WireEvent {
        loop {
            if self
                .closed
                .as_mut()
                .is_some_and(|closed| closed.as_mut().now_or_never().is_some())
            {
                self.closed = None;
                self.disconnected = true;
            }
            if self.disconnected {
                // Keep the final received output before reporting process
                // death. The in-process controller transport can outlive its
                // upstream adapter, so transport EOF alone is insufficient.
                if self.updates_open {
                    match self.updates.next().now_or_never() {
                        Some(Some(update)) => return WireEvent::Update(update),
                        Some(None) => self.updates_open = false,
                        None => {}
                    }
                }
                return WireEvent::Disconnected;
            }
            // Notifications written before the prompt response may still be
            // buffered in the UI subscription. Deliver those before allowing
            // the reducer to dispatch a queued follow-up.
            if self.settled.is_some() {
                // Keep cancellation bounded even if updates keep arriving:
                // inspect the ready cancellation send/deadline before draining
                // another update. A ready send wins an exact deadline tie.
                if let Some(event) = self.poll_cancellation_send() {
                    return event;
                }
                if self.updates_open {
                    match self.updates.next().now_or_never() {
                        Some(Some(update)) => return WireEvent::Update(update),
                        Some(None) => {
                            self.updates_open = false;
                            return WireEvent::Disconnected;
                        }
                        None => {}
                    }
                }
                if self.permissions_open {
                    match self.permissions.next().now_or_never() {
                        Some(Some(permission)) => {
                            if let Some(event) = self.receive_permission(permission) {
                                return event;
                            }
                            continue;
                        }
                        Some(None) => self.permissions_open = false,
                        None => {}
                    }
                }
                // Finish the one cancellation notification before releasing
                // the turn. A deferred send must never cancel its successor.
                if self.cancel_request.is_some() {
                    tokio::select! {
                        biased;
                        result = pending_cancel(&mut self.cancel_request) => {
                            self.cancel_request = None;
                            if let Err(error) = result {
                                return WireEvent::CancelFailed(format!("{error:#}"));
                            }
                        }
                        () = cancellation_deadline(self.cancel_deadline) => {
                            return WireEvent::CancellationExpired;
                        }
                    }
                    continue;
                }
                if let Some(result) = self.settled.take() {
                    self.cancel_sent = false;
                    self.cancel_deadline = None;
                    self.cancel_request = None;
                    return WireEvent::Settled(result);
                }
            }
            tokio::select! {
                biased;
                result = pending_turn(&mut self.turn) => {
                    self.turn = None;
                    self.settled = Some(result);
                }
                result = pending_cancel(&mut self.cancel_request) => {
                    self.cancel_request = None;
                    if let Err(error) = result { return WireEvent::CancelFailed(format!("{error:#}")); }
                }
                () = pending_closed(&mut self.closed) => {
                    self.closed = None;
                    self.disconnected = true;
                }
                () = cancellation_deadline(self.cancel_deadline) => {
                    return WireEvent::CancellationExpired;
                }
                update = self.updates.next(), if self.updates_open => {
                    match update {
                        Some(update) => return WireEvent::Update(update),
                        None => { self.updates_open = false; return WireEvent::Disconnected; }
                    }
                }
                permission = self.permissions.next(), if self.permissions_open => {
                    match permission {
                        Some(permission) => {
                            if let Some(event) = self.receive_permission(permission) {
                                return event;
                            }
                        }
                        None => self.permissions_open = false,
                    }
                }
            }
        }
    }

    pub async fn shutdown(&mut self) -> bool {
        self.begin_teardown();
        let deadline = self.cancel_deadline.unwrap_or_else(|| {
            tokio::time::Instant::now()
                + bitrouter_sdk::acp::client::AcpClient::cancellation_grace()
        });
        while self.working() {
            tokio::select! {
                event = self.next() => {
                    if matches!(event, WireEvent::CancellationExpired | WireEvent::Disconnected) { break; }
                }
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
        if self.tearing_down {
            // Catch entries that arrived while waiting for the prompt to
            // settle. Requests arriving after this point are covered by the
            // SDK's connection-wide teardown denial below.
            self.deny_outstanding();
        }
        let clean = match self.handle.as_mut() {
            Some(handle) => handle.shutdown().await,
            None => true,
        };
        // The transport has now been closed and controlled teardown attempted.
        if let Some(turn) = self.turn.take() {
            turn.abort();
        }
        self.handle = None;
        self.updates_open = false;
        self.permissions_open = false;
        self.cancel_deadline = None;
        self.cancel_request = None;
        self.cancel_sent = false;
        self.tearing_down = false;
        self.settled = None;
        self.closed = None;
        self.disconnected = false;
        clean
    }
}

async fn pending_turn(turn: &mut Option<TurnTask>) -> Result<PromptResponse> {
    match turn {
        Some(turn) => turn
            .await
            .map_err(|error| anyhow::anyhow!("ACP prompt task failed: {error}"))?,
        None => std::future::pending().await,
    }
}

async fn pending_cancel(request: &mut Option<CancelFuture>) -> Result<()> {
    match request {
        Some(request) => request.await,
        None => std::future::pending().await,
    }
}

async fn pending_closed(closed: &mut Option<ClosedFuture>) {
    match closed {
        Some(closed) => closed.await,
        None => std::future::pending().await,
    }
}

async fn cancellation_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        AgentCapabilities, ContentChunk, InitializeRequest, InitializeResponse, PermissionOption,
        PermissionOptionKind, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
        StopReason, TextContent, ToolCallUpdate, ToolCallUpdateFields,
    };
    use agent_client_protocol::{Agent, Client, ConnectTo};
    use bitrouter_sdk::acp::client::{AcpClient, ClientOptions};

    struct PermissionAgent {
        outcomes: tokio::sync::mpsc::UnboundedSender<RequestPermissionOutcome>,
    }

    impl ConnectTo<Client> for PermissionAgent {
        async fn connect_to(
            self,
            client: impl ConnectTo<Agent>,
        ) -> std::result::Result<(), agent_client_protocol::Error> {
            let outcomes = self.outcomes;
            Agent
                .builder()
                .name("code-wire-permission-agent")
                .on_receive_request(
                    async move |request: InitializeRequest, responder, _connection| {
                        responder.respond(
                            InitializeResponse::new(request.protocol_version)
                                .agent_capabilities(AgentCapabilities::new()),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: PromptRequest, responder, connection| {
                        let outcomes = outcomes.clone();
                        let ask = connection.send_request(RequestPermissionRequest::new(
                            request.session_id,
                            ToolCallUpdate::new("wire-tool", ToolCallUpdateFields::default()),
                            vec![PermissionOption::new(
                                "reject",
                                "Reject once",
                                PermissionOptionKind::RejectOnce,
                            )],
                        ));
                        connection.spawn(async move {
                            let response = ask.block_task().await?;
                            let _ = outcomes.send(response.outcome);
                            responder.respond(PromptResponse::new(StopReason::EndTurn))
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(client)
                .await
        }
    }

    async fn pending_permission() -> anyhow::Result<(
        AcpClient,
        PendingPermission,
        tokio::task::JoinHandle<anyhow::Result<PromptResponse>>,
        tokio::sync::mpsc::UnboundedReceiver<RequestPermissionOutcome>,
    )> {
        let (outcomes, outcome_rx) = tokio::sync::mpsc::unbounded_channel();
        let client =
            AcpClient::connect(PermissionAgent { outcomes }, ClientOptions::default()).await?;
        let mut permissions = client.subscribe_permissions();
        let prompt_client = client.clone();
        let prompt = tokio::spawn(async move { prompt_client.prompt("native-wire", "test").await });
        let permission =
            tokio::time::timeout(std::time::Duration::from_secs(3), permissions.next())
                .await
                .map_err(|_| anyhow::anyhow!("agent did not request permission"))?
                .ok_or_else(|| anyhow::anyhow!("permission stream ended before its request"))?;
        Ok((client, permission, prompt, outcome_rx))
    }

    async fn received_outcome(
        outcomes: &mut tokio::sync::mpsc::UnboundedReceiver<RequestPermissionOutcome>,
    ) -> anyhow::Result<RequestPermissionOutcome> {
        tokio::time::timeout(std::time::Duration::from_secs(3), outcomes.recv())
            .await
            .map_err(|_| anyhow::anyhow!("agent did not receive the permission outcome"))?
            .ok_or_else(|| anyhow::anyhow!("agent outcome receiver closed"))
    }

    async fn finish_permission_fixture(
        client: AcpClient,
        prompt: tokio::task::JoinHandle<anyhow::Result<PromptResponse>>,
    ) -> anyhow::Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(3), prompt)
            .await
            .map_err(|_| anyhow::anyhow!("prompt did not settle after permission resolution"))?
            .map_err(anyhow::Error::from)??;
        client.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn buffered_final_updates_precede_queue_release() {
        let mut wire = CodeWire {
            settled: Some(Ok(PromptResponse::new(StopReason::EndTurn))),
            updates: Box::pin(
                futures::stream::iter([SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    agent_client_protocol::schema::v1::ContentBlock::Text(TextContent::new(
                        "final output",
                    )),
                ))])
                .chain(futures::stream::pending()),
            ),
            updates_open: true,
            ..Default::default()
        };
        assert!(wire.working());
        assert!(matches!(wire.next().await, WireEvent::Update(_)));
        assert!(wire.working());
        assert!(
            matches!(wire.next().await, WireEvent::Settled(Ok(response)) if response.stop_reason == StopReason::EndTurn)
        );
        assert!(!wire.working());
    }

    #[tokio::test]
    async fn closed_transport_prevents_queued_follow_up_release() {
        let mut wire = CodeWire {
            settled: Some(Ok(PromptResponse::new(StopReason::EndTurn))),
            updates_open: true,
            ..Default::default()
        };
        assert!(matches!(wire.next().await, WireEvent::Disconnected));
        assert!(wire.working());
    }

    #[tokio::test]
    async fn adapter_death_drains_received_output_without_releasing_queued_work() {
        let mut wire = CodeWire {
            settled: Some(Ok(PromptResponse::new(StopReason::EndTurn))),
            closed: Some(Box::pin(async {})),
            updates: Box::pin(
                futures::stream::iter([SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    agent_client_protocol::schema::v1::ContentBlock::Text(TextContent::new(
                        "received before adapter exit",
                    )),
                ))])
                .chain(futures::stream::pending()),
            ),
            updates_open: true,
            ..Default::default()
        };
        assert!(matches!(wire.next().await, WireEvent::Update(_)));
        assert!(matches!(wire.next().await, WireEvent::Disconnected));
        assert!(wire.working());
    }

    #[tokio::test]
    async fn cancellation_send_finishes_before_a_racing_completion_is_released() {
        let sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = sent.clone();
        let mut wire = CodeWire {
            settled: Some(Ok(PromptResponse::new(StopReason::EndTurn))),
            cancel_sent: true,
            cancel_request: Some(Box::pin(async move {
                tokio::task::yield_now().await;
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })),
            cancel_deadline: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(3)),
            ..Default::default()
        };
        assert!(
            matches!(wire.next().await, WireEvent::Settled(Ok(response)) if response.stop_reason == StopReason::EndTurn)
        );
        assert!(sent.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!wire.cancelling());
    }

    #[tokio::test]
    async fn cancelling_retains_prompt_and_delivers_late_updates() -> Result<()> {
        let (sender, response) = tokio::sync::oneshot::channel();
        let mut wire = CodeWire {
            turn: Some(tokio::spawn(async move { Ok(response.await?) })),
            cancel_deadline: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(3)),
            cancel_sent: true,
            updates: Box::pin(
                futures::stream::iter([SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    agent_client_protocol::schema::v1::ContentBlock::Text(TextContent::new("late")),
                ))])
                .chain(futures::stream::pending()),
            ),
            updates_open: true,
            ..Default::default()
        };
        assert!(matches!(wire.next().await, WireEvent::Update(_)));
        assert!(wire.working());
        assert!(wire.cancelling());
        sender
            .send(PromptResponse::new(StopReason::Cancelled))
            .map_err(|_| anyhow::anyhow!("prompt receiver was dropped"))?;
        assert!(
            matches!(wire.next().await, WireEvent::Settled(Ok(response)) if response.stop_reason == StopReason::Cancelled)
        );
        assert!(!wire.working());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn grace_expiry_does_not_pretend_the_prompt_settled() {
        let mut wire = CodeWire {
            turn: Some(tokio::spawn(std::future::pending())),
            cancel_deadline: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(3)),
            cancel_sent: true,
            ..Default::default()
        };
        assert!(matches!(wire.next().await, WireEvent::CancellationExpired));
        assert!(wire.working());
        if let Some(turn) = wire.turn.take() {
            turn.abort();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn prompt_settlement_wins_a_cancellation_deadline_tie() {
        let mut wire = CodeWire {
            turn: Some(tokio::spawn(async {
                Ok(PromptResponse::new(StopReason::EndTurn))
            })),
            cancel_sent: true,
            cancel_deadline: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(3)),
            ..Default::default()
        };
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(3)).await;

        assert!(
            matches!(wire.next().await, WireEvent::Settled(Ok(response)) if response.stop_reason == StopReason::EndTurn)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn settled_result_and_cancel_send_win_a_deadline_tie() {
        let mut wire = CodeWire {
            settled: Some(Ok(PromptResponse::new(StopReason::EndTurn))),
            cancel_sent: true,
            cancel_request: Some(Box::pin(async { Ok(()) })),
            cancel_deadline: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(3)),
            ..Default::default()
        };
        tokio::time::advance(std::time::Duration::from_secs(3)).await;

        assert!(
            matches!(wire.next().await, WireEvent::Settled(Ok(response)) if response.stop_reason == StopReason::EndTurn)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_deadline_beats_endless_updates() {
        let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(
            agent_client_protocol::schema::v1::ContentBlock::Text(TextContent::new(
                "still working",
            )),
        ));
        let mut wire = CodeWire {
            turn: Some(tokio::spawn(std::future::pending())),
            cancel_sent: true,
            cancel_deadline: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(3)),
            updates: Box::pin(futures::stream::iter(std::iter::repeat(update))),
            updates_open: true,
            ..Default::default()
        };
        tokio::time::advance(std::time::Duration::from_secs(3)).await;

        assert!(matches!(wire.next().await, WireEvent::CancellationExpired));
        if let Some(turn) = wire.turn.take() {
            turn.abort();
        }
    }

    #[tokio::test]
    async fn teardown_denies_retained_permission_before_shutdown() -> anyhow::Result<()> {
        let (client, permission, prompt, mut outcomes) = pending_permission().await?;
        let mut wire = CodeWire::default();
        wire.pending
            .insert(permission.request_id.clone(), permission);

        assert!(wire.shutdown().await);
        assert!(matches!(
            received_outcome(&mut outcomes).await?,
            RequestPermissionOutcome::Selected(selected)
                if selected.option_id.0.as_ref() == "reject"
        ));
        finish_permission_fixture(client, prompt).await
    }

    #[tokio::test]
    async fn late_teardown_permission_is_denied() -> anyhow::Result<()> {
        let (client, permission, prompt, mut outcomes) = pending_permission().await?;
        let mut wire = CodeWire::default();
        wire.begin_teardown();

        assert!(wire.receive_permission(permission).is_none());
        assert!(matches!(
            received_outcome(&mut outcomes).await?,
            RequestPermissionOutcome::Selected(selected)
                if selected.option_id.0.as_ref() == "reject"
        ));
        finish_permission_fixture(client, prompt).await
    }

    #[tokio::test]
    async fn explicit_cancel_keeps_retained_permission_cancelled() -> anyhow::Result<()> {
        let (client, permission, prompt, mut outcomes) = pending_permission().await?;
        let mut wire = CodeWire {
            turn: Some(tokio::spawn(std::future::pending())),
            ..Default::default()
        };
        wire.pending
            .insert(permission.request_id.clone(), permission);

        wire.cancel();
        assert!(matches!(
            received_outcome(&mut outcomes).await?,
            RequestPermissionOutcome::Cancelled
        ));
        if let Some(turn) = wire.turn.take() {
            turn.abort();
        }
        finish_permission_fixture(client, prompt).await
    }
}
