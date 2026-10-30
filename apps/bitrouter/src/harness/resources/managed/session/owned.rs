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

// This is storage in the original host frame, not another task or owner. A
// concurrent command may fail or unwind while this producer is still awaited.
struct Paid {
    work: Option<futures::future::BoxFuture<'static, ()>>,
    completion_stop: CancellationToken,
}
impl Paid {
    async fn finish(&mut self) {
        self.completion_stop.cancel();
        // Borrow through the await: an enclosing unwind must not drop the only
        // producer by moving it out of the retained host frame first.
        if let Some(work) = &mut self.work {
            work.await;
        }
        self.work = None;
    }
}

enum Command {
    Run {
        input: Box<TaskInput>,
        approval: Arc<dyn NativeApproval>,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<SessionSnapshot, CoreError>>,
        _permit: tokio::sync::OwnedSemaphorePermit,
    },
    Cohort {
        model: String,
        root: String,
        permit: tokio::sync::OwnedSemaphorePermit,
        reply:
            oneshot::Sender<Result<crate::core::optimization::cohort::CohortAssessment, CoreError>>,
    },
    Assessment {
        input: Box<crate::tenancy::capture::human::Input>,
        deadline: tokio::time::Instant,
        permit: tokio::sync::OwnedSemaphorePermit,
        reply: oneshot::Sender<Result<crate::tenancy::session::assessment::Reply, CoreError>>,
    },
    Paid {
        checkpoint: String,
        expected: Option<String>,
        deadline: tokio::time::Instant,
        permit: tokio::sync::OwnedSemaphorePermit,
        reply: oneshot::Sender<Result<(), CoreError>>,
    },
    Close(oneshot::Sender<Result<(), CoreError>>),
}

pub(super) struct Handle {
    commands: mpsc::Sender<Command>,
    capacity: Arc<tokio::sync::Semaphore>,
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
        let permit = tokio::time::timeout(
            Duration::from_secs(5),
            self.capacity.clone().acquire_owned(),
        )
        .await
        .map_err(|_| error("native host command deadline"))?
        .map_err(|_| error("native owner stopped"))?;
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
                _permit: permit,
            }),
        )
        .await
        .map_err(|_| error("native host queue deadline"))?
        .map_err(|_| error("native owner stopped"))?;
        receive
            .await
            .map_err(|_| error("native owner response unavailable"))?
    }
    pub async fn cohort(
        &self,
        model: &str,
        root: &str,
    ) -> Result<crate::core::optimization::cohort::CohortAssessment, CoreError> {
        if model.is_empty() || model.len() > 512 || root.len() > 80 {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "native cohort selector bound",
            ));
        }
        let permit =
            self.capacity.clone().try_acquire_owned().map_err(|_| {
                CoreError::rejected(ErrorCode::Busy, "native owner command is active")
            })?;
        let (reply, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Cohort {
                model: model.to_owned(),
                root: root.to_owned(),
                permit,
                reply,
            })
            .map_err(|_| {
                CoreError::rejected(ErrorCode::Busy, "native owner queue is unavailable")
            })?;
        tokio::time::timeout(Duration::from_secs(5), receive)
            .await
            .map_err(|_| error("native cohort deadline"))?
            .map_err(|_| error("native cohort response unavailable"))?
    }
    pub async fn assessment(
        &self,
        input: crate::tenancy::capture::human::Input,
    ) -> Result<crate::tenancy::session::assessment::Reply, CoreError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let input = crate::tenancy::session::assessment::bounded(input)?;
        let permit =
            self.capacity.clone().try_acquire_owned().map_err(|_| {
                CoreError::rejected(ErrorCode::Busy, "native owner command is active")
            })?;
        let (reply, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Assessment {
                input: Box::new(input),
                deadline,
                permit,
                reply,
            })
            .map_err(|_| {
                CoreError::rejected(ErrorCode::Busy, "native owner queue is unavailable")
            })?;
        tokio::time::timeout_at(deadline, receive)
            .await
            .map_err(|_| {
                let mut error = error("native assessment outcome is unconfirmed");
                error.commit_status = bitrouter_ai::core::protocol::CommitStatus::Unknown;
                error
            })?
            .map_err(|_| {
                let mut error = error("native assessment owner response unavailable");
                error.commit_status = bitrouter_ai::core::protocol::CommitStatus::Unknown;
                error
            })?
    }
    pub async fn assess_run(
        &self,
        checkpoint: &str,
        expected: Option<&str>,
    ) -> Result<(), CoreError> {
        if checkpoint.is_empty()
            || checkpoint.len() > 256
            || checkpoint.chars().any(char::is_control)
            || expected.is_some_and(|id| {
                id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)
            })
        {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "native paid selector bound",
            ));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let permit =
            self.capacity.clone().try_acquire_owned().map_err(|_| {
                CoreError::rejected(ErrorCode::Busy, "native owner command is active")
            })?;
        let (reply, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Paid {
                checkpoint: checkpoint.to_owned(),
                expected: expected.map(str::to_owned),
                deadline,
                permit,
                reply,
            })
            .map_err(|_| error("native paid owner queue unavailable"))?;
        // The detached caller owns no producer. The one existing host owner
        // polls the accepted job through its original execution/cleanup deadline.
        receive.await.map_err(|_| {
            let mut failure = error("native paid reply unavailable");
            failure.commit_status = bitrouter_ai::core::protocol::CommitStatus::Unknown;
            failure
        })?
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
        capacity: Arc::new(tokio::sync::Semaphore::new(1)),
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

fn owner(
    request: Request,
    authority: Arc<Authority>,
    handle: Handle,
    commands: mpsc::Receiver<Command>,
    reply: oneshot::Sender<Result<NativeSession, CoreError>>,
) -> futures::future::BoxFuture<'static, ()> {
    // Heap-own the host frame before it crosses the Registry spawn wrappers.
    // Custody, the retained paid future and every cleanup boundary stay inside it.
    async move {
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
        let mut paid = Paid {
            work: None,
            completion_stop: CancellationToken::new(),
        };
        // These remain outside the caught command future. Its failure cannot drop
        // the only workspace fence, Core handle or accepted paid producer.
        let result = {
            let work = AssertUnwindSafe(serve(
                request,
                &mut opening,
                &mut live,
                handle,
                commands,
                reply,
                &mut paid,
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
            // Fencing new work does not join or discard an already accepted judge.
            // Keep polling its original response/observation future on this host.
            paid.finish().await;
            pending::<()>().await;
        }
        // These drops occur only after successful resource shutdown and durable release.
        drop((live, opening));
    }
    .boxed()
}

async fn serve(
    request: Request,
    opening: &mut Opening,
    live: &mut Option<NativeSession>,
    handle: Handle,
    mut commands: mpsc::Receiver<Command>,
    reply: oneshot::Sender<Result<NativeSession, CoreError>>,
    paid: &mut Paid,
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
            () = async {
                match &mut paid.work { Some(work) => work.await, None => pending::<()>().await }
            } => { paid.work = None; continue; }
            command = commands.recv() => command,
        };
        match command {
            Some(Command::Run {
                input,
                approval,
                cancel,
                reply,
                _permit,
            }) => {
                if opening
                    .binding
                    .as_ref()
                    .is_some_and(|binding| !matches!(binding.has_paid_source(), Ok(false)))
                    || paid.work.is_some()
                {
                    if reply
                        .send(Err(CoreError::rejected(
                            ErrorCode::Busy,
                            "native completed source has an unfinished paid assessment",
                        )))
                        .is_err()
                    {
                        tracing::debug!("blocked native run waiter disconnected");
                    }
                    continue;
                }
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
            Some(Command::Cohort {
                model,
                root,
                permit,
                reply,
            }) => {
                let binding = opening
                    .binding
                    .clone()
                    .ok_or_else(|| error("native binding absent"))?;
                let outcome = crate::tenancy::experiment::queue::cohort(
                    binding.clone(),
                    &model,
                    &root,
                    permit,
                )
                .await
                .map_err(core_error);
                crate::tenancy::experiment::queue::deliver(&binding, reply, outcome);
            }
            Some(Command::Assessment {
                input,
                deadline,
                permit,
                reply,
            }) => {
                let binding = opening
                    .binding
                    .clone()
                    .ok_or_else(|| error("native binding absent"))?;
                let input = *input;
                let write = !matches!(input, crate::tenancy::capture::human::Input::Review(_));
                let request = crate::tenancy::session::assessment::Request::new(
                    binding.clone(),
                    input,
                    deadline,
                    permit,
                )
                .await;
                let outcome = match request {
                    Ok(request) => binding
                        .authority()
                        .service()
                        .native_assessment(request)
                        .await
                        .map_err(|error| error.core()),
                    Err(_) => Err(CoreError::rejected(
                        ErrorCode::UnauthorizedScope,
                        "native assessment admission rejected",
                    )),
                };
                let unknown = outcome.as_ref().err().is_some_and(|error| {
                    error.commit_status == bitrouter_ai::core::protocol::CommitStatus::Unknown
                });
                if unknown {
                    opening.ambiguous = true;
                }
                crate::tenancy::session::assessment::deliver(
                    &binding, write, deadline, reply, outcome,
                );
                // A consumed ambiguous receiver is not an unsent operation. The
                // existing owner retains its cleanup/quarantine obligation.
                if unknown {
                    return Err(error("native assessment transaction remains unconfirmed"));
                }
            }
            Some(Command::Paid {
                checkpoint,
                expected,
                deadline,
                permit,
                reply,
            }) => {
                let binding = opening
                    .binding
                    .clone()
                    .ok_or_else(|| error("native binding absent"))?;
                if paid.work.is_some() || !matches!(binding.has_paid_source(), Ok(false)) {
                    if reply
                        .send(Err(CoreError::rejected(
                            ErrorCode::Busy,
                            "native source already has a paid assessment",
                        )))
                        .is_err()
                    {
                        tracing::debug!("duplicate paid waiter disconnected");
                    }
                    continue;
                }
                if reply.is_closed() {
                    continue;
                }
                let completion_stop = paid.completion_stop.clone();
                let service = binding.authority().service().clone();
                paid.work = Some(Box::pin(async move {
                    let work = async move {
                        let outcome = crate::tenancy::learning::native::run(
                            binding,
                            checkpoint,
                            expected,
                            deadline,
                            permit,
                            completion_stop,
                        )
                        .await;
                        if reply.send(outcome).is_err() {
                            tracing::debug!("native paid work finished without its caller");
                        }
                    };
                    if AssertUnwindSafe(work).catch_unwind().await.is_err() {
                        // A panicked producer cannot be repolled. Keep its
                        // host and registry obligations quarantined instead.
                        service.host_cleanup_unknown();
                        pending::<()>().await;
                    }
                }));
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
            Command::Cohort { reply, .. } => {
                if reply.send(Err(error("native owner is closing"))).is_err() {
                    tracing::debug!("queued native cohort has no response receiver");
                }
            }
            Command::Assessment { reply, .. } => {
                if reply.send(Err(error("native owner is closing"))).is_err() {
                    tracing::debug!("queued native assessment has no response receiver");
                }
            }
            Command::Paid { reply, .. } => {
                if reply.send(Err(error("native owner is closing"))).is_err() {
                    tracing::debug!("queued native paid waiter disconnected");
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
    // Keep polling the same paid producer; dropping a waiter is not cleanup.
    paid.finish().await;
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
    let outcome = match outcome {
        Ok(()) => {
            if let Some(binding) = &opening.binding
                && binding
                    .has_paid_source()
                    .map_err(|code| core_error(code.into()))?
            {
                let proof = Arc::new(CleanupProof {
                    binding: binding.clone(),
                });
                crate::tenancy::learning::native::closed(proof)
                    .await
                    .map_err(|code| core_error(code.into()))?;
            }
            Ok(())
        }
        Err(error) => Err(error),
    };
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
        port.reconcile_owned(&self.store).await?;
        self.epoch_committed = true;
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
        if let Some(checked) = self.store.recovery().cloned() {
            if !checked.committed() {
                // Closed preparation has no dispatch, and confirmed non-commit
                // leaves the exact predecessor row in place. Never manufacture
                // a release or retry its atomic repair during failed-open cleanup.
                if !self.store.write_not_committed() {
                    return Err(error("native recovery disposition is unconfirmed"));
                }
                if let Some(resources) = &self.resources {
                    resources.shutdown().await.map_err(error)?;
                } else {
                    self.mcp.lock().await.shutdown().await.map_err(error)?;
                }
                // The reserved predecessor marker stays Active/Idle exactly as
                // found. No new native epoch was committed, so do not release it.
                return Ok(());
            }
            if !checked.retired() {
                let core = self
                    .core
                    .as_ref()
                    .ok_or_else(|| error("committed recovery lost Core custody"))?;
                let port = self
                    .port
                    .as_ref()
                    .ok_or_else(|| error("committed recovery lost port custody"))?;
                let durable = port.state.lock().await.journal.head.clone();
                core.reconcile_owned(&durable).await?;
                core.retire_stopped_native().await?;
                let binding = self
                    .binding
                    .as_ref()
                    .ok_or_else(|| error("native binding missing"))?;
                self.workspace
                    .as_ref()
                    .ok_or_else(|| error("native workspace missing"))?
                    .activate_stopped_native(&checked, binding.ownership_grant())
                    .map_err(|err| error(err.to_string()))?;
                self.store.retire_recovery()?;
                let mut state = port.state.lock().await;
                let next = state.journal.clone();
                port.persist(&mut state, next).await?;
                checked
                    .confirm_retired()
                    .map_err(|_| error("native retirement not acknowledged"))?;
            }
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

/// Minted only after actual resources, Core release, durable journal release and
/// workspace finish all returned success above. A caller/row cannot construct it.
pub(crate) struct CleanupProof {
    binding: Arc<Binding>,
}
impl CleanupProof {
    pub(crate) fn binding(&self) -> &Arc<Binding> {
        &self.binding
    }
}
