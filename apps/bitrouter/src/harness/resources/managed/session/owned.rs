//! Caller futures wait; the native owner retains initialization and cleanup.
use super::*;
use crate::{
    managed_store::owned::Store,
    tenancy::execution::session::{Authority, Binding, core_error},
};
use futures::FutureExt;
use std::{future::pending, panic::AssertUnwindSafe, time::Duration};
use tokio::sync::oneshot;

pub(super) struct Opening {
    pub authority: Arc<Authority>,
    stop: CancellationToken,
    pub store: Arc<Store>,
    pub binding: Option<Arc<Binding>>,
    pub workspace: Option<Arc<WorkspaceFence>>,
    pub resources: Option<Arc<NativeResources>>,
    pub mcp: Arc<tokio::sync::Mutex<crate::harness::resources::mcp::McpConnections>>,
    pub port: Option<Arc<NativePort>>,
    pub core: Option<CoreSession>,
    pub epoch_started: bool,
    pub epoch_committed: bool,
    pub ambiguous: bool,
}

enum Command {
    Run {
        input: Box<TaskInput>,
        approval: Arc<dyn NativeApproval>,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<SessionSnapshot, CoreError>>,
    },
    Close(oneshot::Sender<Result<(), CoreError>>),
}

pub(super) struct Handle {
    commands: mpsc::Sender<Command>,
    stop: CancellationToken,
    finished: tokio::sync::watch::Receiver<bool>,
}

impl Handle {
    pub fn cancel(&self) {
        self.stop.cancel();
    }
    pub async fn run(
        &self,
        input: TaskInput,
        approval: Arc<dyn NativeApproval>,
        cancel: CancellationToken,
    ) -> Result<SessionSnapshot, CoreError> {
        input_bound(&input)?;
        let cancel = cancel.child_token();
        let _cancel_on_drop = cancel.clone().drop_guard();
        let (reply, receive) = oneshot::channel();
        tokio::time::timeout(
            Duration::from_secs(5),
            self.commands.send(Command::Run {
                input: Box::new(input),
                approval,
                cancel,
                reply,
            }),
        )
        .await
        .map_err(|_| error("native host queue deadline"))?
        .map_err(|_| error("native owner stopped"))?;
        receive
            .await
            .map_err(|_| error("native owner response unavailable"))?
    }
    pub async fn close(self) -> Result<(), CoreError> {
        if *self.finished.borrow() {
            return Ok(());
        }
        let (reply, receive) = oneshot::channel();
        let sent = tokio::time::timeout(
            Duration::from_secs(5),
            self.commands.send(Command::Close(reply)),
        )
        .await
        .map_err(|_| error("native host queue deadline"))?;
        if sent.is_ok() {
            tokio::time::timeout(Duration::from_secs(30), receive)
                .await
                .map_err(|_| error("native owner cleanup is unconfirmed"))?
                .map_err(|_| error("native owner cleanup response unavailable"))??;
        }
        let mut finished = self.finished.clone();
        tokio::time::timeout(Duration::from_secs(30), async {
            while !*finished.borrow_and_update() {
                finished
                    .changed()
                    .await
                    .map_err(|_| error("native owner completion watch closed"))?;
            }
            Ok(())
        })
        .await
        .map_err(|_| error("native owner cleanup is unconfirmed"))?
    }
}

pub(super) async fn open(
    app: Arc<App>,
    headers: http::HeaderMap,
    name: &str,
    workspace: &Path,
    mode: ToolMode,
    config: &HarnessConfig,
) -> Result<NativeSession, CoreError> {
    let service = app.native_session_service()?;
    let authority = service
        .session_authority(&headers, name)
        .await
        .map_err(core_error)?;
    let slot = service.host_slot(authority.host()).map_err(core_error)?;
    let stop = CancellationToken::new();
    let cancel_waiter = stop.clone().drop_guard();
    let (commands, receiver) = mpsc::channel(1);
    let handle = Handle {
        commands,
        stop: stop.clone(),
        finished: slot.finished(),
    };
    let (reply, receive) = oneshot::channel();
    let work = owner(
        Request {
            app,
            workspace: workspace.to_owned(),
            mode,
            config: config.clone(),
        },
        authority,
        handle,
        receiver,
        reply,
    );
    service.own_host(slot, work);
    let session = receive
        .await
        .map_err(|_| error("native opener response unavailable"))??;
    cancel_waiter.disarm();
    Ok(session)
}

struct Request {
    app: Arc<App>,
    workspace: std::path::PathBuf,
    mode: ToolMode,
    config: HarnessConfig,
}

async fn owner(
    request: Request,
    authority: Arc<Authority>,
    handle: Handle,
    commands: mpsc::Receiver<Command>,
    reply: oneshot::Sender<Result<NativeSession, CoreError>>,
) {
    let service = authority.service().clone();
    let stop = handle.stop.clone();
    let service_stop = service.host_stop();
    let mut opening = Opening {
        store: Store::new(authority.clone()),
        authority,
        stop: stop.clone(),
        binding: None,
        workspace: None,
        resources: None,
        mcp: Arc::new(tokio::sync::Mutex::new(
            crate::harness::resources::mcp::McpConnections::default(),
        )),
        port: None,
        core: None,
        epoch_started: false,
        epoch_committed: false,
        ambiguous: false,
    };
    let mut live = None;
    // Opening and live remain outside the caught future. Its panic cannot drop
    // the only workspace fence, resource owner or initial pending Core handle.
    let result = {
        let work = AssertUnwindSafe(serve(
            request,
            &mut opening,
            &mut live,
            handle,
            commands,
            reply,
        ))
        .catch_unwind();
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => result,
            () = async { tokio::select! { () = stop.cancelled() => {}, () = service_stop.cancelled() => {} } } => {
                stop.cancel();
                match tokio::time::timeout(Duration::from_secs(30), &mut work).await {
                    Ok(result) => result,
                    Err(_) => {
                        service.host_cleanup_unknown();
                        // Keep polling the same work; the deadline did not prove cleanup.
                        work.await
                    }
                }
            }
        }
    };
    let clean = matches!(result, Ok(Ok(())));
    if !clean {
        tracing::error!("native host cleanup failed or unwound; owner retained");
        service.host_cleanup_unknown();
        pending::<()>().await;
    }
    // These drops occur only after successful resource shutdown and durable release.
    drop((live, opening));
}

async fn serve(
    request: Request,
    opening: &mut Opening,
    live: &mut Option<NativeSession>,
    handle: Handle,
    mut commands: mpsc::Receiver<Command>,
    reply: oneshot::Sender<Result<NativeSession, CoreError>>,
) -> Result<(), CoreError> {
    let stop = handle.stop.clone();
    let service_stop = opening.authority.service().host_stop();
    if let Err(error) = opening.check() {
        if reply.send(Err(error)).is_err() {
            tracing::debug!("cancelled native opener has no response receiver");
        }
        return Ok(());
    }
    let opened = NativeSession::open_inner(
        request.app,
        opening.authority.caller(),
        opening.store.clone(),
        &request.workspace,
        request.mode,
        &request.config,
        Some(opening),
    )
    .await;
    let session = match opened {
        Ok(session) => session,
        Err(error) => {
            if reply.send(Err(error)).is_err() {
                tracing::debug!("native opener rejected without a response receiver");
            }
            return opening.cleanup_failed_open().await;
        }
    };
    let (_, receiver) = mpsc::channel(1);
    let proxy = NativeSession {
        core: session.core.clone(),
        port: session.port.clone(),
        resources: session.resources.clone(),
        receiver,
        workspace: session.workspace.clone(),
        effects: session.effects.clone(),
        owned_handle: Some(handle),
    };
    *live = Some(session);
    if reply.send(Ok(proxy)).is_err() {
        tracing::debug!("native opener completed without a response receiver");
        stop.cancel();
    }
    let close_reply = loop {
        let command = tokio::select! {
            biased;
            () = stop.cancelled() => break None,
            () = service_stop.cancelled() => break None,
            command = commands.recv() => command,
        };
        match command {
            Some(Command::Run {
                input,
                approval,
                cancel,
                reply,
            }) => {
                if cancel.is_cancelled() {
                    if reply
                        .send(Err(error(
                            "native run waiter disconnected before admission",
                        )))
                        .is_err()
                    {
                        tracing::debug!("cancelled queued native run has no response receiver");
                    }
                    continue;
                }
                let session = live.as_mut().ok_or_else(|| error("native owner missing"))?;
                let work = session.run(*input, approval, cancel.clone());
                tokio::pin!(work);
                let outcome = tokio::select! {
                    biased;
                    () = stop.cancelled() => { cancel.cancel(); work.await },
                    () = service_stop.cancelled() => { cancel.cancel(); work.await },
                    () = cancel.cancelled() => {
                        match tokio::time::timeout(Duration::from_secs(30), &mut work).await {
                            Ok(result) => result,
                            Err(_) => {
                                opening.authority.service().host_cleanup_unknown();
                                work.await
                            }
                        }
                    }
                    result = &mut work => result,
                };
                let failed = outcome.is_err();
                if reply.send(outcome).is_err() {
                    tracing::debug!("native run completed without a response receiver");
                }
                if failed {
                    break None;
                }
            }
            Some(Command::Close(reply)) => break Some(reply),
            None => break None,
        }
    };
    commands.close();
    while let Ok(command) = commands.try_recv() {
        match command {
            Command::Run { reply, cancel, .. } => {
                cancel.cancel();
                if reply.send(Err(error("native owner is closing"))).is_err() {
                    tracing::debug!("queued native run has no response receiver");
                }
            }
            Command::Close(reply) => {
                if reply
                    .send(Err(error("native close is already pending")))
                    .is_err()
                {
                    tracing::debug!("duplicate native close has no response receiver");
                }
            }
        }
    }
    if let Some(binding) = &opening.binding {
        binding.close_new_work()?;
    }
    opening.readback().await?;
    let session = live
        .as_mut()
        .ok_or_else(|| error("native owner missing at cleanup"))?;
    session.port.cancel_tools();
    let durable = session.port.state.lock().await.journal.head.clone();
    session.core.reconcile_owned(&durable).await?;
    cancel_current_run(&session.core).await?;
    if session.core.snapshot().await.run.is_some() {
        // Reuse Core's cancellation path. It may settle old work but no owned
        // dispatch gate admits a new provider/tool start during shutdown.
        session.core.drive().await?;
    }
    let session = live
        .take()
        .ok_or_else(|| error("native owner missing at release"))?;
    let outcome = session.close().await;
    if let Some(reply) = close_reply
        && reply.send(outcome.clone()).is_err()
    {
        tracing::debug!("native close completed without a response receiver");
    }
    outcome
}

impl Opening {
    pub(super) fn cancellation(&self) -> &CancellationToken {
        &self.stop
    }
    pub(super) fn check(&self) -> Result<(), CoreError> {
        if self.stop.is_cancelled() || self.authority.service().host_stop().is_cancelled() {
            return Err(error("native owner opening was cancelled"));
        }
        self.authority.check().map_err(core_error)
    }
    async fn readback(&mut self) -> Result<(), CoreError> {
        let Some(port) = &self.port else {
            return Ok(());
        };
        if !port.failed.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        let loaded = self.store.reconcile().await?;
        let journal: Journal = serde_json::from_slice(&loaded.journal)
            .map_err(|_| error("native readback journal is invalid"))?;
        let binding = self
            .binding
            .as_ref()
            .ok_or_else(|| error("native readback binding missing"))?;
        binding.grant(&journal.grant)?;
        let attempted = self.store.matches_attempt(&loaded)?;
        let mut state = port.state.lock().await;
        let same = crate::managed_store::owned::encode(&journal)
            .map_err(|_| error("native readback bound"))?
            == crate::managed_store::owned::encode(&state.journal)
                .map_err(|_| error("native readback bound"))?;
        if !((same && loaded.revision == state.revision) || attempted) {
            return Err(error("native readback does not match the retained write"));
        }
        state.revision = loaded.revision;
        state.journal = journal;
        self.epoch_committed = true;
        self.store.confirm_readback();
        port.failed
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    async fn cleanup_failed_open(&mut self) -> Result<(), CoreError> {
        if let Some(binding) = &self.binding {
            binding.close_new_work()?;
        }
        self.readback().await?;
        if self.ambiguous
            || self.store.load_unconfirmed()
            || (self.epoch_started && !self.epoch_committed && !self.store.write_not_committed())
        {
            return Err(error(
                "native startup journal or resource cleanup is unconfirmed",
            ));
        }
        if let Some(core) = &self.core {
            let port = self
                .port
                .as_ref()
                .ok_or_else(|| error("native startup port missing"))?;
            port.restored();
            let durable = port.state.lock().await.journal.head.clone();
            core.reconcile_owned(&durable).await?;
            core.release(
                &format!("release_{}", uuid::Uuid::new_v4()),
                core.head().await.state_revision,
            )
            .await?;
        }
        if let Some(resources) = &self.resources {
            resources.shutdown().await.map_err(error)?;
        } else {
            self.mcp.lock().await.shutdown().await.map_err(error)?;
        }
        if let Some(port) = self.port.as_ref().filter(|_| self.epoch_committed) {
            let mut state = port.state.lock().await;
            let mut next = state.journal.clone();
            next.released = true;
            port.persist(&mut state, next).await?;
        }
        if let Some(workspace) = &self.workspace {
            workspace.finish().map_err(|e| error(e.to_string()))?;
        }
        Ok(())
    }
}

fn input_bound(input: &TaskInput) -> Result<(), CoreError> {
    struct Count(u64);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len() as u64)
                .filter(|n| *n <= NativeResources::limits().input_bytes)
                .ok_or_else(|| std::io::Error::other("native input bound"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(&mut Count(0), input)
        .map_err(|_| CoreError::rejected(ErrorCode::LimitExceeded, "native input exceeds bound"))
}
