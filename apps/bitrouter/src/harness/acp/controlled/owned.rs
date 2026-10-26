use super::{CheckpointSummary, ControlledSession, LocalControllerLaunch, factory};
use crate::{
    gateway::assembly::App,
    harness::acp::{
        client::AcpClient,
        controller::{ControllerConfig, ControllerIdentity},
        up::owned::Control,
    },
    tenancy::capture::{Binding, port::Port},
};
use anyhow::{Context, Result, ensure};
use futures::{FutureExt, Stream};
use std::{collections::BTreeSet, path::PathBuf, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

const WORK: Duration = Duration::from_secs(60);
const CLEANUP: Duration = Duration::from_secs(30);

enum Input {
    New(PathBuf),
    Prompt(String, String),
    Checkpoint(String),
}
enum Reply {
    New(String),
    Prompt,
    Checkpoint(Option<CheckpointSummary>),
}
struct Command {
    input: Input,
    reply: oneshot::Sender<Result<Reply>>,
    _capacity: OwnedSemaphorePermit,
}

pub(super) struct Handle {
    commands: mpsc::Sender<Command>,
    capacity: Arc<Semaphore>,
    stop: CancellationToken,
    finished: watch::Receiver<bool>,
    client: AcpClient,
    #[cfg(all(test, unix))]
    pub(super) ingress: Arc<crate::harness::acp::up::owned::ingress::Statistics>,
}
impl Handle {
    pub(super) fn stop(&self) {
        self.stop.cancel();
    }
    async fn request(&self, input: Input) -> Result<Reply> {
        ensure!(!self.stop.is_cancelled(), "controlled session is closing");
        let capacity = self
            .capacity
            .clone()
            .try_acquire_owned()
            .context("controlled session capacity exhausted")?;
        let cancel = self.stop.clone().drop_guard();
        let (reply, receive) = oneshot::channel();
        self.commands
            .try_send(Command {
                input,
                reply,
                _capacity: capacity,
            })
            .map_err(|_| anyhow::anyhow!("controlled session queue unavailable"))?;
        let result = receive
            .await
            .context("controlled session reply unavailable")??;
        cancel.disarm();
        Ok(result)
    }
    pub(super) async fn new_session(&self, cwd: PathBuf) -> Result<String> {
        ensure!(
            cwd.is_absolute() && cwd.as_os_str().len() <= 4096,
            "controlled workspace must be a bounded absolute path"
        );
        match self.request(Input::New(cwd)).await? {
            Reply::New(id) => Ok(id),
            _ => anyhow::bail!("controlled session reply mismatch"),
        }
    }
    pub(super) async fn prompt(&self, native: &str, text: &str) -> Result<()> {
        ensure!(
            !native.is_empty() && native.len() <= 256 && text.len() <= 128 * 1024,
            "controlled prompt exceeds bound"
        );
        match self
            .request(Input::Prompt(native.to_owned(), text.to_owned()))
            .await?
        {
            Reply::Prompt => Ok(()),
            _ => anyhow::bail!("controlled prompt reply mismatch"),
        }
    }
    pub(super) async fn checkpoint(&self, native: &str) -> Result<Option<CheckpointSummary>> {
        ensure!(
            !native.is_empty() && native.len() <= 256,
            "controlled native ID exceeds bound"
        );
        match self.request(Input::Checkpoint(native.to_owned())).await? {
            Reply::Checkpoint(value) => Ok(value),
            _ => anyhow::bail!("controlled checkpoint reply mismatch"),
        }
    }
    pub(super) async fn shutdown(&self) -> Result<()> {
        self.stop.cancel();
        let mut finished = self.finished.clone();
        tokio::time::timeout(CLEANUP, async {
            while !*finished.borrow_and_update() {
                finished
                    .changed()
                    .await
                    .context("controlled owner completion lost")?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("controlled cleanup is unconfirmed")?
    }
    pub(super) fn updates(
        &self,
    ) -> Pin<Box<dyn Stream<Item = crate::harness::acp::translate::SessionUpdateKind> + Send>> {
        self.client.subscribe_updates()
    }
    pub(super) fn permissions(
        &self,
    ) -> Pin<Box<dyn Stream<Item = crate::harness::acp::client::PendingPermission> + Send>> {
        self.client.subscribe_permissions()
    }
}

struct Custody {
    binding: Arc<Binding>,
    port: Arc<Port>,
    process: Arc<Control>,
    connection: Option<factory::Connection>,
    client: Option<AcpClient>,
}

pub(super) async fn open(
    app: Arc<App>,
    headers: http::HeaderMap,
    mut launch: LocalControllerLaunch,
    cancel: CancellationToken,
) -> Result<ControlledSession> {
    ensure!(!cancel.is_cancelled(), "controlled opener cancelled");
    ensure!(cfg!(unix), "owned controller requires Unix process groups");
    ensure!(
        !launch.options.terminal_auth,
        "owned controller has no terminal login"
    );
    ensure!(
        launch
            .options
            .turn_timeout
            .is_none_or(|timeout| timeout <= WORK),
        "controlled turn timeout exceeds bound"
    );
    launch.options.turn_timeout = Some(launch.options.turn_timeout.unwrap_or(WORK));
    launch.agent.validate()?;
    // Trusted launch descriptors may include explicit tool environment; bound
    // them before cloning or creating any child. They are never capture payloads.
    crate::harness::trajectory::owned::io::json(&launch.agent, 128 * 1024)?;
    crate::harness::trajectory::owned::io::json(&launch.options, 128 * 1024)?;
    let stop = cancel.child_token();
    let service = app.capture_service()?;
    let prepared = service
        .prepare_capture(
            &headers,
            &launch.source,
            &launch.listener_origin,
            stop.clone(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("controlled host authentication rejected"))?;
    let slot = service
        .host_slot(prepared.binding.authority())
        .map_err(|_| anyhow::anyhow!("controlled host slot unavailable"))?;
    let finished = slot.finished();
    let waiting = stop.clone().drop_guard();
    let (commands, receiver) = mpsc::channel(1);
    let (reply, receive) = oneshot::channel();
    let port = Port::new(prepared.binding.clone());
    let process = Control::new(prepared.binding.clone(), CancellationToken::new());
    let custody = Custody {
        binding: prepared.binding,
        port,
        process,
        connection: None,
        client: None,
    };
    service.own_host(
        slot,
        owner(
            custody,
            OwnerInput {
                launch,
                endpoint: prepared.endpoint,
                stop,
                finished,
                commands,
                receiver,
                reply,
            },
        ),
    );
    let session = receive
        .await
        .context("controlled opener response unavailable")??;
    waiting.disarm();
    Ok(session)
}

struct OwnerInput {
    launch: LocalControllerLaunch,
    endpoint: crate::harness::acp::controller::ProviderEndpointPlan,
    stop: CancellationToken,
    finished: watch::Receiver<bool>,
    commands: mpsc::Sender<Command>,
    receiver: mpsc::Receiver<Command>,
    reply: oneshot::Sender<Result<ControlledSession>>,
}

async fn owner(mut custody: Custody, input: OwnerInput) {
    let stop = input.stop.clone();
    let service = custody.binding.service().clone();
    let service_stop = service.host_stop();
    let process = custody.process.clone();
    let result = {
        let work = std::panic::AssertUnwindSafe(serve(&mut custody, input)).catch_unwind();
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => result,
            () = async { tokio::select! { () = stop.cancelled() => {}, () = service_stop.cancelled() => {} } } => {
                stop.cancel();
                match tokio::time::timeout(CLEANUP, &mut work).await {
                    Ok(result) => result,
                    Err(_) => { service.host_cleanup_unknown(); process.stop.cancel(); work.await }
                }
            }
        }
    };
    if !matches!(result, Ok(Ok(()))) {
        process.stop.cancel();
        service.host_cleanup_unknown();
        // Custody remains outside the caught inner work. Unknown SQL or child
        // cleanup cannot discard the port, pending bytes, reaper or client.
        std::future::pending::<()>().await;
    }
    drop(custody);
}

async fn serve(custody: &mut Custody, input: OwnerInput) -> Result<()> {
    let OwnerInput {
        launch,
        endpoint,
        stop,
        finished,
        commands,
        mut receiver,
        reply,
    } = input;
    let opened = async {
        custody.binding.check().map_err(|_| anyhow::anyhow!("controlled authority unavailable"))?;
        ensure!(!stop.is_cancelled(), "controlled opener cancelled");
        custody.port.create().await?;
        custody.binding.check().map_err(|_| anyhow::anyhow!("controlled authority unavailable"))?;
        ensure!(!stop.is_cancelled(), "controlled opener cancelled");
        let config = ControllerConfig::new(ControllerIdentity::new(&launch.source, "configured-acp-adapter", "configured")).endpoint(endpoint);
        custody.connection = Some(factory::start(&launch.agent, config, &launch.options, factory::Attachments { owned: Some((custody.process.clone(), custody.port.clone())), ..Default::default() })?);
        let connection = custody.connection.as_mut().context("controlled connection absent")?;
        let connected = tokio::select! {
            biased;
            () = stop.cancelled() => { custody.process.stop.cancel(); (&mut connection.connect).await },
            result = tokio::time::timeout(WORK, &mut connection.connect) => match result {
                Ok(value) => value,
                Err(_) => { stop.cancel(); custody.process.stop.cancel(); (&mut connection.connect).await },
            },
        }?;
        custody.client = Some(connected.clone());
        custody.port.initialized().await?;
        custody.binding.check().map_err(|_| anyhow::anyhow!("controlled authority unavailable"))?;
        ensure!(!stop.is_cancelled(), "controlled opener cancelled");
        Ok::<_, anyhow::Error>(ControlledSession { handle: Handle { commands, capacity: Arc::new(Semaphore::new(1)), stop: stop.clone(), finished, client: connected, #[cfg(all(test, unix))] ingress: custody.process.ingress.clone() } })
    }.await;
    let ready = opened.is_ok();
    if reply.send(opened).is_err() {
        stop.cancel();
    }
    if ready {
        let mut sessions = BTreeSet::new();
        loop {
            let command = tokio::select! { biased; () = stop.cancelled() => break, command = receiver.recv() => command };
            let Some(command) = command else {
                break;
            };
            let Command {
                input,
                reply,
                _capacity,
            } = command;
            let work = operation(custody, &launch, input, &mut sessions);
            tokio::pin!(work);
            let result = tokio::select! {
                biased;
                () = stop.cancelled() => Err(anyhow::anyhow!("controlled operation cancelled")),
                result = tokio::time::timeout(WORK, &mut work) => result.unwrap_or_else(|_| Err(anyhow::anyhow!("controlled operation deadline"))),
            };
            let failed = result.is_err();
            if reply.send(result).is_err() {
                stop.cancel();
            }
            drop(_capacity);
            if failed {
                stop.cancel();
                break;
            }
        }
    }
    receiver.close();
    while let Ok(command) = receiver.try_recv() {
        let _sent = command
            .reply
            .send(Err(anyhow::anyhow!("controlled owner closing")));
    }
    custody
        .binding
        .close()
        .map_err(|_| anyhow::anyhow!("controlled close authority unavailable"))?;
    // First ask the client to end the duplex. If the child fails to cooperate,
    // signal its existing process owner; never abort the controller task.
    if let Some(client) = &custody.client {
        let close = client.shutdown();
        tokio::pin!(close);
        // Client protocol failure still requires the same controller/reaper
        // join below; it cannot short-circuit process or append cleanup.
        let _protocol_close = match tokio::time::timeout(Duration::from_secs(5), &mut close).await {
            Ok(result) => result,
            Err(_) => {
                custody.process.stop.cancel();
                close.await
            }
        };
    }
    custody.process.stop.cancel();
    if let Some(connection) = &mut custody.connection {
        // A controller error is a captured interrupted connection, not evidence
        // of a lost owner. An unwound/detached task requires quarantine.
        let _protocol_result = (&mut connection.controller)
            .await
            .context("controlled task join unconfirmed")?;
        if custody.process.started() {
            connection
                .reaped
                .clone()
                .await
                .context("controlled child cleanup unconfirmed")?;
        }
        if let Some(driver) = &mut connection.client_driver {
            driver
                .await
                .context("controlled client driver join unconfirmed")?;
        }
    }
    custody.port.finish().await?;
    Ok(())
}

async fn operation(
    custody: &mut Custody,
    launch: &LocalControllerLaunch,
    input: Input,
    sessions: &mut BTreeSet<String>,
) -> Result<Reply> {
    custody
        .binding
        .check()
        .map_err(|_| anyhow::anyhow!("controlled authority rejected"))?;
    let client = custody
        .client
        .as_ref()
        .context("controlled client unavailable")?;
    let result = match input {
        Input::New(cwd) => {
            ensure!(sessions.len() < 16, "controlled session limit reached");
            let ids = client
                .new_session(cwd, launch.options.mcp_servers.clone())
                .await?;
            custody
                .binding
                .check()
                .map_err(|_| anyhow::anyhow!("controlled authority rejected"))?;
            let options = ids
                .initial_settings
                .config_options
                .context("harness lacks model selection")?;
            // ACP option metadata is inspected before sending a fixed model value.
            // https://agentclientprotocol.com/protocol/session-config-options
            let json = serde_json::to_value(options)?;
            let has_model = json.as_array().is_some_and(|options| {
                options.iter().any(|option| {
                    option.get("id").and_then(serde_json::Value::as_str) == Some("model")
                        && option.get("type").and_then(serde_json::Value::as_str) == Some("select")
                        && option
                            .get("options")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|options| {
                                options.iter().any(|option| {
                                    option.get("value").and_then(serde_json::Value::as_str)
                                        == Some(custody.binding.model())
                                })
                            })
                })
            });
            ensure!(
                has_model,
                "harness does not advertise the fixed retained model"
            );
            client
                .set_session_config_option(&ids.acp_session_id, "model", custody.binding.model())
                .await?;
            ensure!(
                sessions.insert(ids.acp_session_id.clone()),
                "harness reused a native ID"
            );
            Reply::New(ids.acp_session_id)
        }
        Input::Prompt(native, text) => {
            ensure!(
                sessions.contains(&native),
                "native session is not associated with this controller"
            );
            client.prompt(&native, &text).await?;
            Reply::Prompt
        }
        Input::Checkpoint(native) => {
            ensure!(
                sessions.contains(&native),
                "native session is not associated with this controller"
            );
            Reply::Checkpoint(custody.port.checkpoint(&native).await?.map(|value| {
                CheckpointSummary {
                    checkpoint_id: value.checkpoint_id,
                    watermark: value.watermark,
                    prefix_digest: value.prefix_digest,
                    observations: value.observations,
                }
            }))
        }
    };
    custody
        .binding
        .check()
        .map_err(|_| anyhow::anyhow!("controlled authority rejected"))?;
    Ok(result)
}
