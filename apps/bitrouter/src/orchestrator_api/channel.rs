//! WebSocket carries the versioned harness protocol, not Responses WebSocket
//! injection. Axum's bounded upgrade and message API is documented at
//! <https://docs.rs/axum/latest/axum/extract/ws/index.html>.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use base64::{Engine, engine::general_purpose::STANDARD};
use bitrouter_orchestrator::core::checkpoint::{CheckpointAck, CheckpointBatch};
use bitrouter_orchestrator::core::collaboration::Action;
use bitrouter_orchestrator::core::protocol::{
    ArtifactRef, BETA, ClientMessage, Command, ServerMessage, ToolStatus,
};
use bitrouter_orchestrator::core::session::HarnessPort;
use futures::{SinkExt, StreamExt};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{
    ApiError, CoreError, CoreSession, Entry, ErrorCode, Limits, ManagedCoreApi, SessionKey,
    auth::Principal,
};

const IO_TIMEOUT: Duration = Duration::from_secs(20);

struct Outgoing {
    text: String,
    sent: oneshot::Sender<Result<(), CoreError>>,
}

#[derive(Clone)]
struct Connection {
    generation: u64,
    sender: mpsc::Sender<Outgoing>,
    closed: CancellationToken,
}

struct PendingAck {
    generation: u64,
    batch_id: String,
    sender: oneshot::Sender<CheckpointAck>,
}

struct PendingRead {
    generation: u64,
    request_id: String,
    reference: ArtifactRef,
    offset: u64,
    max_bytes: u64,
    sender: oneshot::Sender<Vec<u8>>,
}

pub(super) struct RemotePort {
    principal: Principal,
    grant: bitrouter_orchestrator::core::protocol::OwnershipGrant,
    limits: Limits,
    connection: Mutex<Option<Connection>>,
    ack: Mutex<Option<PendingAck>>,
    read: Mutex<Option<PendingRead>>,
    writing: Mutex<()>,
}

impl RemotePort {
    fn new(
        principal: Principal,
        grant: bitrouter_orchestrator::core::protocol::OwnershipGrant,
        limits: Limits,
    ) -> Self {
        Self {
            principal,
            grant,
            limits,
            connection: Mutex::new(None),
            ack: Mutex::new(None),
            read: Mutex::new(None),
            writing: Mutex::new(()),
        }
    }

    async fn attach(&self, sender: mpsc::Sender<Outgoing>) -> Result<Connection, CoreError> {
        let mut current = self.connection.lock().await;
        if current
            .as_ref()
            .is_some_and(|connection| !connection.closed.is_cancelled())
        {
            return Err(error(
                ErrorCode::Busy,
                "harness connection is already active",
            ));
        }
        let generation = current.as_ref().map_or(Ok(1), |connection| {
            connection
                .generation
                .checked_add(1)
                .ok_or_else(|| error(ErrorCode::LimitExceeded, "connection generation exhausted"))
        })?;
        self.ack.lock().await.take();
        self.read.lock().await.take();
        let connection = Connection {
            generation,
            sender,
            closed: CancellationToken::new(),
        };
        *current = Some(connection.clone());
        Ok(connection)
    }

    async fn current(&self) -> Result<Connection, CoreError> {
        self.connection
            .lock()
            .await
            .as_ref()
            .filter(|connection| !connection.closed.is_cancelled())
            .cloned()
            .ok_or_else(disconnected)
    }

    async fn failed(&self, connection: &Connection) {
        connection.closed.cancel();
        let mut ack = self.ack.lock().await;
        if ack
            .as_ref()
            .is_some_and(|pending| pending.generation == connection.generation)
        {
            ack.take();
        }
        let mut read = self.read.lock().await;
        if read
            .as_ref()
            .is_some_and(|pending| pending.generation == connection.generation)
        {
            read.take();
        }
    }

    async fn transmit(
        &self,
        connection: &Connection,
        message: ServerMessage,
    ) -> Result<(), CoreError> {
        let sending = async {
            // One encoded frame across the queue and socket writer. Waiters
            // hold core-owned typed values, never additional encoded batches.
            let _writing = self.writing.lock().await;
            self.principal.revalidate().await?;
            let text = serde_json::to_string(&message).map_err(|_| {
                error(
                    ErrorCode::CheckpointConflict,
                    "cannot encode harness message",
                )
            })?;
            let bound = if matches!(message, ServerMessage::Checkpoint(_)) {
                self.limits.unacknowledged_bytes
            } else {
                self.limits.ephemeral_bytes
            };
            if text.len() as u64 > bound {
                return Err(error(
                    ErrorCode::LimitExceeded,
                    "harness output exceeds negotiated byte bound",
                ));
            }
            let (sent, receive) = oneshot::channel();
            connection
                .sender
                .send(Outgoing { text, sent })
                .await
                .map_err(|_| disconnected())?;
            receive.await.map_err(|_| disconnected())?
        };
        let result = tokio::select! {
            _ = connection.closed.cancelled() => Err(disconnected()),
            result = tokio::time::timeout(IO_TIMEOUT, sending) => result.unwrap_or_else(|_| Err(disconnected())),
        };
        if result.is_err() {
            self.failed(connection).await;
        }
        result
    }

    async fn accept_ack(&self, generation: u64, ack: CheckpointAck) -> Result<(), CoreError> {
        let mut pending = self.ack.lock().await;
        if pending.as_ref().is_none_or(|pending| {
            pending.generation != generation || pending.batch_id != ack.batch_id
        }) {
            return Err(error(
                ErrorCode::CheckpointConflict,
                "ACK has no matching pending checkpoint",
            ));
        }
        if let Some(pending) = pending.take() {
            let _ = pending.sender.send(ack);
        }
        Ok(())
    }

    async fn accept_chunk(
        &self,
        generation: u64,
        request_id: &str,
        reference: &ArtifactRef,
        offset: u64,
        content: &str,
    ) -> Result<(), CoreError> {
        let mut pending = self.read.lock().await;
        let expected = pending
            .as_ref()
            .ok_or_else(|| error(ErrorCode::ArtifactUnavailable, "no pending artifact read"))?;
        if expected.generation != generation
            || expected.request_id != request_id
            || &expected.reference != reference
            || expected.offset != offset
        {
            return Err(error(
                ErrorCode::ArtifactUnavailable,
                "artifact chunk differs from requested range",
            ));
        }
        let bytes = STANDARD.decode(content).map_err(|_| {
            error(
                ErrorCode::ArtifactUnavailable,
                "invalid artifact chunk encoding",
            )
        })?;
        if bytes.is_empty() || bytes.len() as u64 > expected.max_bytes {
            return Err(error(
                ErrorCode::ArtifactUnavailable,
                "artifact chunk exceeds requested range",
            ));
        }
        if let Some(pending) = pending.take() {
            let _ = pending.sender.send(bytes);
        }
        Ok(())
    }
}

#[async_trait]
impl HarnessPort for RemotePort {
    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        let connection = self.current().await?;
        let (sender, receive) = oneshot::channel();
        {
            let mut pending = self.ack.lock().await;
            if pending.is_some() {
                return Err(error(ErrorCode::Busy, "checkpoint ACK is still pending"));
            }
            *pending = Some(PendingAck {
                generation: connection.generation,
                batch_id: batch.identity.batch_id.clone(),
                sender,
            });
        }
        let result = async {
        self.transmit(&connection, ServerMessage::Checkpoint(batch)).await?;
        tokio::select! {
            _ = connection.closed.cancelled() => Err(disconnected()),
            result = tokio::time::timeout(IO_TIMEOUT, receive) => result.map_err(|_| disconnected())?.map_err(|_| disconnected()),
        }
        }.await;
        if result.is_err() {
            self.failed(&connection).await;
        }
        result
    }

    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.transmit(&self.current().await?, message).await
    }

    async fn read_artifact(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
        let connection = self.current().await?;
        let request_id = format!("read_{}", uuid::Uuid::new_v4().simple());
        let (sender, receive) = oneshot::channel();
        // Leave room for the reference and base64 encoding in the input envelope.
        let max_bytes = max_bytes.min(self.limits.input_bytes / 4);
        {
            let mut pending = self.read.lock().await;
            if pending.is_some() {
                return Err(error(ErrorCode::Busy, "artifact read is pending"));
            }
            *pending = Some(PendingRead {
                generation: connection.generation,
                request_id: request_id.clone(),
                reference: reference.clone(),
                offset,
                max_bytes,
                sender,
            });
        }
        let result = async {
        self.transmit(&connection, ServerMessage::ArtifactRead { request_id, reference: reference.clone(), offset, max_bytes }).await?;
        tokio::select! {
            _ = connection.closed.cancelled() => Err(disconnected()),
            result = tokio::time::timeout(IO_TIMEOUT, receive) => result.map_err(|_| disconnected())?.map_err(|_| disconnected()),
        }
        }.await;
        if result.is_err() {
            self.failed(&connection).await;
        }
        result
    }
}

pub(super) async fn upgrade(
    State(api): State<ManagedCoreApi>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if api.shared.shutdown.is_cancelled() {
        return Err(ApiError::unavailable());
    }
    let principal = Principal::authenticate(&api.shared.db, &headers).await?;
    if headers
        .get("bitrouter-beta")
        .and_then(|value| value.to_str().ok())
        != Some(BETA)
    {
        return Err(ApiError::core(
            ErrorCode::UnsupportedVersion,
            "channel requires BitRouter-Beta: orchestrator_core=v1",
        ));
    }
    let permit = api
        .shared
        .connections
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::core(ErrorCode::Busy, "harness connection capacity reached"))?;
    let max = api.shared.capabilities.limits.unacknowledged_bytes as usize;
    Ok(upgrade
        .max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| serve(api, principal, socket, permit)))
}

async fn serve(
    api: ManagedCoreApi,
    principal: Principal,
    mut socket: WebSocket,
    _permit: OwnedSemaphorePermit,
) {
    let first = tokio::select! {
        _ = api.shared.shutdown.cancelled() => return,
        first = tokio::time::timeout(IO_TIMEOUT, socket.recv()) => first,
    };
    let message = match first {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<ClientMessage>(&text).ok(),
        _ => None,
    };
    let Some(message) = message else {
        return;
    };
    let (sender, mut outgoing) = mpsc::channel::<Outgoing>(1);
    let prepared = prepare(&api, &principal, &message, sender).await;
    let (key, port, connection, existing) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Ok(text) = serde_json::to_string(&ServerMessage::Error(error)) {
                let _ = socket.send(Message::Text(text.into())).await;
            }
            return;
        }
    };
    let (mut writer, mut reader) = socket.split();
    let write_closed = connection.closed.clone();
    let writing = tokio::spawn(async move {
        while let Some(output) = tokio::select! {
            _ = write_closed.cancelled() => None,
            output = outgoing.recv() => output,
        } {
            let delivered =
                tokio::time::timeout(IO_TIMEOUT, writer.send(Message::Text(output.text.into())))
                    .await;
            let ok = matches!(delivered, Ok(Ok(())));
            let _ = output
                .sent
                .send(if ok { Ok(()) } else { Err(disconnected()) });
            if !ok {
                break;
            }
        }
        write_closed.cancel();
    });
    let (commands, receive) = mpsc::channel(1);
    let worker_api = api.clone();
    let worker_port = port.clone();
    let worker_principal = principal.clone();
    let worker_key = key.clone();
    let worker_closed = connection.closed.clone();
    let worker = tokio::spawn(async move {
        let result = bind(
            &worker_api,
            &worker_principal,
            message,
            worker_port.clone(),
            existing,
        )
        .await;
        match result {
            Ok(session) => {
                if let Some(entry) = worker_api.shared.sessions.lock().await.get_mut(&worker_key) {
                    entry.session = Some(session.clone());
                    entry.ready = true;
                }
                let _ = worker_port
                    .send(ServerMessage::Head(session.head().await))
                    .await;
                run_commands(session, worker_port, receive, worker_closed.clone()).await;
            }
            Err(error) => {
                let _ = worker_port.send(ServerMessage::Error(error)).await;
            }
        }
        worker_closed.cancel();
    });
    loop {
        let next = tokio::select! {
            _ = connection.closed.cancelled() => break,
            _ = api.shared.shutdown.cancelled() => break,
            next = reader.next() => next,
        };
        let text = match next {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            _ => break,
        };
        let result = async {
            principal.revalidate().await?;
            let message: ClientMessage = serde_json::from_str(&text)
                .map_err(|_| error(ErrorCode::UnsupportedCapability, "invalid harness message"))?;
            message.validate(&port.grant, &port.limits)?;
            match message.command {
                Command::Ack(ack) => port.accept_ack(connection.generation, ack).await,
                Command::ArtifactChunk {
                    request_id,
                    reference,
                    offset,
                    content_base64,
                } => {
                    port.accept_chunk(
                        connection.generation,
                        &request_id,
                        &reference,
                        offset,
                        &content_base64,
                    )
                    .await
                }
                _ => commands
                    .try_send(message)
                    .map_err(|_| error(ErrorCode::Busy, "harness command queue is full")),
            }
        }
        .await;
        if let Err(error) = result
            && port.send(ServerMessage::Error(error)).await.is_err()
        {
            break;
        }
    }
    connection.closed.cancel();
    drop(commands);
    // The core owns pending transition identity even if this connection's
    // worker was awaiting its ACK. Fence it before admitting another binding.
    let session = api
        .shared
        .sessions
        .lock()
        .await
        .get(&key)
        .and_then(|entry| entry.session.clone());
    let released = if let Some(session) = session {
        session.disconnect().await;
        session
            .snapshot()
            .await
            .releases
            .values()
            .any(|release| release.grant == port.grant)
    } else {
        false
    };
    worker.abort();
    let _ = worker.await;
    writing.abort();
    let _ = writing.await;
    let mut sessions = api.shared.sessions.lock().await;
    if let Some(entry) = sessions.get_mut(&key) {
        entry.connected = false;
        if entry.session.is_none() || released {
            sessions.remove(&key);
        }
    }
}

async fn prepare(
    api: &ManagedCoreApi,
    principal: &Principal,
    message: &ClientMessage,
    sender: mpsc::Sender<Outgoing>,
) -> Result<(SessionKey, Arc<RemotePort>, Connection, Option<CoreSession>), CoreError> {
    let binding = match &message.command {
        Command::Bind(binding) => binding.as_ref(),
        Command::Restore(request) => &request.binding,
        _ => {
            return Err(error(
                ErrorCode::UnauthorizedScope,
                "first channel message must bind or restore a session",
            ));
        }
    };
    binding.grant.validate()?;
    binding
        .manifest
        .validate(&api.shared.capabilities, &binding.limits)?;
    message.validate(&binding.grant, &binding.limits)?;
    if binding.grant.core_instance_id != api.shared.capabilities.core_instance_id {
        return Err(error(
            ErrorCode::UnauthorizedScope,
            "binding names another core instance",
        ));
    }
    let key = ManagedCoreApi::key(principal, &binding.grant.session_id);
    let mut sessions = api.shared.sessions.lock().await;
    if let Some(entry) = sessions.get_mut(&key) {
        if entry.connected {
            return Err(error(
                ErrorCode::Busy,
                "session already has an active channel",
            ));
        }
        if matches!(message.command, Command::Restore(_)) {
            if entry.grant.harness_id != binding.grant.harness_id
                || binding.grant.execution_epoch < entry.grant.execution_epoch
            {
                return Err(error(
                    ErrorCode::StaleEpoch,
                    "restoration must preserve the authority and advance or retain its epoch",
                ));
            }
            if let Some(session) = &entry.session {
                let active = session.fence_for_restoration(&binding.durable_head).await?;
                for report in session.pending_provider_evidence().await.reports {
                    if !entry.recovery_evidence.contains(&report) {
                        entry.recovery_evidence.push(report);
                    }
                }
                if let Command::Restore(request) = &message.command
                    && active.as_ref().is_some_and(|minimum| {
                        request.active_time.as_ref().is_none_or(|reported| {
                            reported.run_id != minimum.run_id
                                || reported.active_ms < minimum.active_ms
                        })
                    })
                {
                    return Err(error(
                        ErrorCode::RecoveryRequired,
                        "restoration activity omits locally observed work",
                    ));
                }
            }
            let port = Arc::new(RemotePort::new(
                principal.clone(),
                binding.grant.clone(),
                binding.limits.clone(),
            ));
            let connection = port.attach(sender).await?;
            entry.connected = true;
            entry.ready = false;
            entry.job = None;
            return Ok((key, port, connection, None));
        }
        if entry.grant != binding.grant || entry.limits != binding.limits {
            return Err(error(
                ErrorCode::Busy,
                "reconnect must preserve its existing grant and limits",
            ));
        }
        if let Some(session) = &entry.session
            && session.snapshot().await.manifest != binding.manifest
        {
            return Err(error(
                ErrorCode::OperationConflict,
                "reconnect cannot change the tool manifest",
            ));
        }
        let connection = entry.port.attach(sender).await?;
        entry.connected = true;
        entry.ready = false;
        return Ok((key, entry.port.clone(), connection, entry.session.clone()));
    }
    if sessions.len() >= api.shared.capabilities.max_sessions as usize {
        return Err(error(ErrorCode::Busy, "managed session capacity reached"));
    }
    let port = Arc::new(RemotePort::new(
        principal.clone(),
        binding.grant.clone(),
        binding.limits.clone(),
    ));
    let connection = port.attach(sender).await?;
    sessions.insert(
        key.clone(),
        Entry {
            session: None,
            port: port.clone(),
            grant: binding.grant.clone(),
            limits: binding.limits.clone(),
            connected: true,
            ready: false,
            job: None,
            recovery_evidence: Vec::new(),
        },
    );
    Ok((key, port, connection, None))
}

async fn bind(
    api: &ManagedCoreApi,
    principal: &Principal,
    message: ClientMessage,
    port: Arc<RemotePort>,
    existing: Option<CoreSession>,
) -> Result<CoreSession, CoreError> {
    principal.revalidate().await?;
    let register_api = api.clone();
    let register_principal = principal.clone();
    let register_port = port.clone();
    let register =
        move |session| register_session(register_api, register_principal, register_port, session);
    let session = match message.command {
        Command::Bind(binding) => match existing {
            Some(session) => {
                session
                    .reconnect(&binding.grant, &binding.durable_head)
                    .await?;
                Ok(session)
            }
            None => {
                CoreSession::bind_registered(
                    *binding,
                    &api.shared.capabilities,
                    api.shared.app.clone(),
                    principal.caller.clone(),
                    principal.headers(),
                    port,
                    register,
                )
                .await
            }
        },
        Command::Restore(request) => {
            if request
                .tools
                .iter()
                .any(|tool| tool.status == ToolStatus::Running)
            {
                return Err(error(
                    ErrorCode::UnsupportedCapability,
                    "running restoration requires a measured remote clock handoff",
                ));
            }
            CoreSession::restore_registered(
                *request,
                &api.shared.capabilities,
                api.shared.app.clone(),
                principal.caller.clone(),
                principal.headers(),
                port,
                register,
            )
            .await
        }
        _ => Err(error(
            ErrorCode::UnauthorizedScope,
            "binding command required",
        )),
    }?;
    let key = ManagedCoreApi::key(principal, &session.snapshot().await.session_id);
    let reports = api
        .shared
        .sessions
        .lock()
        .await
        .get(&key)
        .map(|entry| entry.recovery_evidence.clone())
        .unwrap_or_default();
    for report in reports {
        session
            .provider_evidence(&format!("evidence:{}", report.attempt_id), report.clone())
            .await?;
        if let Some(entry) = api.shared.sessions.lock().await.get_mut(&key) {
            entry.recovery_evidence.retain(|pending| pending != &report);
        }
    }
    Ok(session)
}

async fn register_session(
    api: ManagedCoreApi,
    principal: Principal,
    port: Arc<RemotePort>,
    session: CoreSession,
) -> Result<(), CoreError> {
    let key = ManagedCoreApi::key(&principal, &port.grant.session_id);
    let mut entries = api.shared.sessions.lock().await;
    let entry = entries
        .get_mut(&key)
        .filter(|entry| entry.connected)
        .ok_or_else(disconnected)?;
    entry.port = port.clone();
    entry.grant = port.grant.clone();
    entry.limits = port.limits.clone();
    entry.session = Some(session);
    entry.ready = false;
    Ok(())
}

async fn run_commands(
    session: CoreSession,
    port: Arc<RemotePort>,
    mut commands: mpsc::Receiver<ClientMessage>,
    closed: CancellationToken,
) {
    let changed = Arc::new(Notify::new());
    changed.notify_one();
    let mut driving = tokio::task::JoinSet::new();
    driving.spawn({
        let session = session.clone();
        let changed = changed.clone();
        let closed = closed.clone();
        let port = port.clone();
        async move {
            loop {
                tokio::select! { _ = closed.cancelled() => break, _ = changed.notified() => {} }
                let state = session.snapshot().await;
                let active = state
                    .responses
                    .latest
                    .as_ref()
                    .and_then(|id| state.responses.exchanges.get(id))
                    .filter(|exchange| exchange.completed_state_revision.is_none())
                    .map(|exchange| exchange.response_id.clone());
                let result = match active {
                    Some(id) => session.drive_response(&id).await.map(|_| ()),
                    None => session.drive().await.map(|_| ()),
                };
                if let Err(error) = result
                    && error.code != ErrorCode::Busy
                {
                    let _ = port.send(ServerMessage::Error(error)).await;
                }
            }
        }
    });
    while let Some(message) = commands.recv().await {
        let response = apply(&session, message).await;
        match response {
            Ok(response) => {
                let _ = port.send(response).await;
                changed.notify_one();
            }
            Err(error) => {
                let _ = port.send(ServerMessage::Error(error)).await;
            }
        }
    }
    driving.shutdown().await;
}

async fn apply(session: &CoreSession, message: ClientMessage) -> Result<ServerMessage, CoreError> {
    let operation = message.operation_id;
    let revision = message.expected_state_revision.unwrap_or(0);
    let receipt = match message.command {
        Command::Enqueue(input) => session.enqueue(&operation, revision, input).await?,
        Command::Steer {
            run_id,
            agent_turn_id,
            text,
        } => {
            session
                .steer(&operation, revision, &run_id, &agent_turn_id, text)
                .await?
        }
        Command::CancelRun { run_id } => session.cancel_run(&operation, revision, &run_id).await?,
        Command::CancelAgent { agent_id } => {
            let root = session.snapshot().await.agent_id;
            session
                .collaborate(&operation, revision, &root, Action::Interrupt { agent_id })
                .await?
        }
        Command::ResumeQueue => session.resume_queue(&operation, revision).await?,
        Command::Signals(update) => session.signals(&operation, *update).await?,
        Command::ToolResult(result) => session.tool_result(&operation, result).await?,
        Command::ToolStatus(observation) => session.tool_status(&operation, observation).await?,
        Command::ProviderEvidence(evidence) => {
            session.provider_evidence(&operation, *evidence).await?
        }
        Command::Material {
            request_id,
            material,
            unavailable_reason,
        } => {
            session
                .material_result(&operation, &request_id, material, unavailable_reason)
                .await?
        }
        Command::Release => session.release(&operation, revision).await?,
        Command::Operation {
            target_operation_id,
        } => {
            return Ok(match session.operation(&target_operation_id).await {
                Some(receipt) => ServerMessage::Receipt(receipt),
                None => ServerMessage::UnknownOperation {
                    operation_id: target_operation_id,
                },
            });
        }
        Command::Head { durable_head } => {
            let head = session.head().await;
            if durable_head != head {
                return Err(error(
                    ErrorCode::CheckpointConflict,
                    "head differs; reconcile through a new binding",
                ));
            }
            return Ok(ServerMessage::Head(head));
        }
        _ => {
            return Err(error(
                ErrorCode::UnsupportedCapability,
                "command is not accepted after binding",
            ));
        }
    };
    Ok(ServerMessage::Receipt(receipt))
}

fn error(code: ErrorCode, message: &str) -> CoreError {
    CoreError::rejected(code, message)
}
fn disconnected() -> CoreError {
    error(
        ErrorCode::CheckpointUnavailable,
        "harness channel interrupted or timed out",
    )
}
