use super::*;
use agent_client_protocol::schema::v1::{InitializeRequest, InitializeResponse};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder};
use anyhow::{Context, Result};
use bitrouter_sdk::acp::client::ClientOptions;
use bitrouter_sdk::acp::controller::tasks::{PendingTaskSelection, TaskCursor, TaskStatusRequest};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

#[derive(Clone)]
struct Server {
    state: Arc<Mutex<TaskStatusResponse>>,
    calls: Arc<Mutex<Vec<TaskSelectRequest>>>,
    unknown: Arc<AtomicBool>,
    reject: Arc<AtomicBool>,
    next_error: Arc<Mutex<Option<agent_client_protocol::Error>>>,
    delay_status: Arc<AtomicBool>,
    status_started: Arc<Notify>,
    release_status: Arc<Notify>,
    status_sent: Arc<Notify>,
}

impl Server {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TaskStatusResponse {
                current: Some(TaskCursor {
                    task_id: "task".into(),
                    attempt_id: "attempt".into(),
                    revision: 2,
                }),
                phase: Some("settling".into()),
                pending: None,
            })),
            calls: Default::default(),
            unknown: Default::default(),
            reject: Default::default(),
            next_error: Default::default(),
            delay_status: Default::default(),
            status_started: Default::default(),
            release_status: Default::default(),
            status_sent: Default::default(),
        }
    }
}

impl ConnectTo<Client> for Server {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let status = self.clone();
        Agent.builder()
            .on_receive_request(async |request: InitializeRequest, responder: Responder<InitializeResponse>, _connection| {
                let mut response = InitializeResponse::new(request.protocol_version);
                response.meta = Some([("bitrouter.dev/controller".into(), json!({"taskControl":{
                    "version":"1","scope":"session","methods":["_bitrouter/task/status","_bitrouter/task/select"]
                }}))].into_iter().collect());
                responder.respond(response)
            }, agent_client_protocol::on_receive_request!())
            .on_receive_request(async move |_: TaskStatusRequest, responder: Responder<TaskStatusResponse>, connection: ConnectionTo<Client>| {
                let response = status.state.lock().map_err(|_| agent_client_protocol::Error::internal_error())?.clone();
                if status.delay_status.swap(false, Ordering::SeqCst) {
                    let status = status.clone();
                    connection.spawn(async move {
                        status.status_started.notify_one();
                        status.release_status.notified().await;
                        let result = responder.respond(response);
                        status.status_sent.notify_one();
                        result
                    })?;
                    Ok(())
                } else { responder.respond(response) }
            }, agent_client_protocol::on_receive_request!())
            .on_receive_request(async move |request: TaskSelectRequest, responder: Responder<TaskStatusResponse>, _connection| {
                let mut calls = self.calls.lock().map_err(|_| agent_client_protocol::Error::internal_error())?;
                let seen = calls.iter().any(|old| old == &request);
                calls.push(request.clone());
                if let Some(error) = self.next_error.lock().map_err(|_| agent_client_protocol::Error::internal_error())?.take() {
                    return responder.respond_with_error(error);
                }
                if self.reject.swap(false, Ordering::SeqCst) {
                    return responder.respond_with_error(agent_client_protocol::Error::invalid_request().data(json!({
                        "code":"task_control_conflict","outcome":"not_applied","message":"Task changed."
                    })));
                }
                let mut state = self.state.lock().map_err(|_| agent_client_protocol::Error::internal_error())?;
                if !seen {
                    state.pending = Some(PendingTaskSelection { request_id: request.request_id, mode: request.mode });
                }
                if self.unknown.swap(false, Ordering::SeqCst) {
                    return responder.respond_with_error(agent_client_protocol::Error::invalid_request().data(json!({"code":"task_control_conflict"})));
                }
                responder.respond(state.clone())
            }, agent_client_protocol::on_receive_request!())
            .connect_to(client).await
    }
}

async fn complete(driver: &mut TaskDriver) -> Result<()> {
    let outcome = tokio::time::timeout(CONTROL_TIMEOUT, driver.result()).await?;
    driver.complete(outcome);
    Ok(())
}

/// A real storage boundary supplies its RPC error and confirmed status here.
/// Exercise decoding and recovery through the actual client/driver channel.
pub(crate) async fn rejected_selection_keeps_the_current_task_usable(
    error: agent_client_protocol::Error,
    status: TaskStatusResponse,
) -> Result<()> {
    let server = Server::new();
    *server
        .state
        .lock()
        .map_err(|error| anyhow::anyhow!("{error}"))? = status.clone();
    *server
        .next_error
        .lock()
        .map_err(|error| anyhow::anyhow!("{error}"))? = Some(error);
    let client = AcpClient::connect(server, ClientOptions::default()).await?;
    let mut driver = TaskDriver::default();
    driver.refresh(&client, "public");
    complete(&mut driver).await?;
    driver
        .select(&client, "public", TaskSelectionMode::Retry, false)
        .map_err(anyhow::Error::msg)?;
    complete(&mut driver).await?;
    assert!(driver.selection.is_none());
    assert!(!driver.can_prompt(&client));
    driver.refresh(&client, "public");
    complete(&mut driver).await?;
    assert!(driver.can_prompt(&client));
    assert_eq!(
        driver.confirmed.as_ref().context("refreshed task")?.current,
        status.current
    );
    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn uncertain_selection_retries_the_original_intent_after_consumption() -> Result<()> {
    let server = Server::new();
    let client = AcpClient::connect(server.clone(), ClientOptions::default()).await?;
    let mut driver = TaskDriver::default();
    driver.refresh(&client, "public");
    assert!(!driver.can_prompt(&client));
    complete(&mut driver).await?;
    server.unknown.store(true, Ordering::SeqCst);
    driver
        .select(&client, "public", TaskSelectionMode::Retry, false)
        .map_err(anyhow::Error::msg)?;
    assert!(!driver.can_prompt(&client));
    complete(&mut driver).await?;
    assert!(!driver.can_prompt(&client));
    assert!(
        driver
            .view(&client)
            .context("task view")?
            .status
            .contains("unconfirmed")
    );
    driver.refresh(&client, "public");
    assert!(driver.pending.is_none());
    assert!(
        driver
            .select(&client, "public", TaskSelectionMode::NewTask, false)
            .is_err()
    );
    {
        let mut state = server
            .state
            .lock()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        state.pending = None;
        state.current.as_mut().context("current task")?.attempt_id = "later-attempt".into();
    }
    driver
        .select(&client, "public", TaskSelectionMode::Retry, false)
        .map_err(anyhow::Error::msg)?;
    complete(&mut driver).await?;
    assert!(driver.can_prompt(&client));
    assert!(driver.selection.is_none());
    assert_eq!(
        driver
            .view(&client)
            .context("task view")?
            .attempt_id
            .as_deref(),
        Some("later-attempt")
    );
    {
        let calls = server
            .calls
            .lock()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
    }
    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn a_late_status_cannot_erase_a_confirmed_selection_with_the_same_revision() -> Result<()> {
    let server = Server::new();
    let client = AcpClient::connect(server.clone(), ClientOptions::default()).await?;
    let mut driver = TaskDriver::default();
    driver.refresh(&client, "public");
    complete(&mut driver).await?;
    let original = driver.confirmed.as_ref().context("status")?.current.clone();
    server.delay_status.store(true, Ordering::SeqCst);
    driver.refresh(&client, "public");
    tokio::time::timeout(CONTROL_TIMEOUT, server.status_started.notified()).await?;
    driver
        .select(&client, "public", TaskSelectionMode::NewTask, false)
        .map_err(anyhow::Error::msg)?;
    complete(&mut driver).await?;
    server.release_status.notify_one();
    tokio::time::timeout(CONTROL_TIMEOUT, server.status_sent.notified()).await?;
    let _ = client.task_status("public").await?;
    assert_eq!(
        driver.confirmed.as_ref().context("status")?.current,
        original
    );
    assert!(
        driver
            .confirmed
            .as_ref()
            .context("status")?
            .pending
            .is_some()
    );
    assert!(driver.can_prompt(&client));
    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn a_certified_conflict_requires_refresh_and_another_user_action() -> Result<()> {
    let server = Server::new();
    let client = AcpClient::connect(server.clone(), ClientOptions::default()).await?;
    let mut driver = TaskDriver::default();
    driver.refresh(&client, "public");
    complete(&mut driver).await?;
    server.reject.store(true, Ordering::SeqCst);
    driver
        .select(&client, "public", TaskSelectionMode::Retry, false)
        .map_err(anyhow::Error::msg)?;
    complete(&mut driver).await?;
    assert!(driver.selection.is_none());
    assert!(!driver.can_prompt(&client));
    driver.refresh(&client, "public");
    complete(&mut driver).await?;
    assert!(driver.can_prompt(&client));
    assert_eq!(
        server
            .calls
            .lock()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .len(),
        1
    );
    assert_eq!(
        driver.view(&client).context("refreshed task")?.status,
        "Reconciling task evidence; evaluation pending"
    );
    driver
        .select(&client, "public", TaskSelectionMode::NewTask, false)
        .map_err(anyhow::Error::msg)?;
    complete(&mut driver).await?;
    {
        let calls = server
            .calls
            .lock()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        assert_eq!(calls.len(), 2);
        assert_ne!(calls[0].request_id, calls[1].request_id);
    }
    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn replacing_a_driver_discards_the_previous_sessions_late_response() -> Result<()> {
    let old = Server::new();
    let old_client = AcpClient::connect(old.clone(), ClientOptions::default()).await?;
    let mut driver = TaskDriver::default();
    old.delay_status.store(true, Ordering::SeqCst);
    driver.refresh(&old_client, "old");
    tokio::time::timeout(CONTROL_TIMEOUT, old.status_started.notified()).await?;
    driver = TaskDriver::default();
    let new = Server::new();
    new.state
        .lock()
        .map_err(|error| anyhow::anyhow!("{error}"))?
        .current
        .as_mut()
        .context("new task")?
        .task_id = "new-task".into();
    let new_client = AcpClient::connect(new, ClientOptions::default()).await?;
    driver.refresh(&new_client, "new");
    complete(&mut driver).await?;
    old.release_status.notify_one();
    tokio::time::timeout(CONTROL_TIMEOUT, old.status_sent.notified()).await?;
    let _ = old_client.task_status("old").await?;
    assert_eq!(
        driver
            .view(&new_client)
            .context("new view")?
            .task_id
            .as_deref(),
        Some("new-task")
    );
    old_client.shutdown().await?;
    new_client.shutdown().await?;
    Ok(())
}
