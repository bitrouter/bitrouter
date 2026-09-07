use super::super::tests::{claude_service, observation};
use super::*;
use crate::session_evidence::store::tasks::TaskReadProbe;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder};
use bitrouter_sdk::acp::client::{AcpClient, ClientOptions};
use bitrouter_sdk::acp::controller::tasks::TaskSelectionMode;
use bitrouter_sdk::acp::controller::{Controller, ControllerConfig};
use futures::StreamExt;

async fn session(directory: &Path) -> Result<EvidenceHandle> {
    let handle = claude_service(directory).await?;
    handle
        .service
        .observe(observation("new", "session/new", "request", json!({})))
        .await?;
    handle
        .service
        .observe(observation(
            "new",
            "session/new",
            "response",
            json!({"sessionId":"public"}),
        ))
        .await?;
    Ok(handle)
}

#[tokio::test]
async fn committed_selection_with_failed_status_is_unknown_and_replayable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut handle = session(directory.path()).await?;
    let service = handle.service.clone();
    service
        .observe(observation(
            "first",
            "session/prompt",
            "request",
            json!({"sessionId":"public","prompt":[]}),
        ))
        .await?;
    service
        .observe(observation(
            "first",
            "session/prompt",
            "response",
            json!({"stopReason":"end_turn"}),
        ))
        .await?;
    let before = service
        .task_status(TaskStatusRequest {
            session_id: "public".into(),
        })
        .await?;
    let request = TaskSelectRequest {
        session_id: "public".into(),
        request_id: "selection".into(),
        expected: before.current.context("task")?,
        mode: TaskSelectionMode::Retry,
    };
    let probe = Arc::new(TaskReadProbe {
        fail: true,
        ..Default::default()
    });
    *service.store.task_read_probe.lock().await = Some(probe.clone());
    let pending = {
        let service = service.clone();
        let request = request.clone();
        tokio::spawn(async move { service.task_select(request).await })
    };
    tokio::time::timeout(Duration::from_secs(10), probe.entered.notified()).await?;
    let key = AcpSessionKey {
        namespace: service.collector.root().namespace.clone(),
        harness: service.collector.root().harness,
        session_id: "public".into(),
    };
    assert_eq!(
        service
            .store
            .task_status(&key)
            .await?
            .pending
            .context("committed selection")?
            .request_id,
        request.request_id
    );
    probe.release.notify_one();
    let failure = pending.await?.err().context("post-commit read must fail")?;
    assert_eq!(
        failure.data.as_ref().context("error outcome")?["outcome"],
        "unknown"
    );
    let confirmed = service.task_select(request.clone()).await?;
    assert_eq!(
        confirmed.pending.context("same selection")?.request_id,
        request.request_id
    );
    assert_eq!(service.store.attempts(None, 8).await?.len(), 1);
    let mut stale = request.clone();
    stale.request_id = "stale".into();
    stale.expected.revision += 1;
    let failure = service
        .task_select(stale)
        .await
        .err()
        .context("stale selection")?;
    assert_eq!(
        failure.data.as_ref().context("error outcome")?["outcome"],
        "not_applied"
    );
    let mut changed = request;
    changed.mode = TaskSelectionMode::NewTask;
    let failure = service
        .task_select(changed)
        .await
        .err()
        .context("mismatched replay")?;
    assert_eq!(
        failure.data.as_ref().context("error outcome")?["outcome"],
        "unknown"
    );
    handle.shutdown().await?;
    Ok(())
}

struct Native {
    cancelled: Arc<Notify>,
}

impl ConnectTo<Client> for Native {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        Agent
            .builder()
            .on_receive_request(
                async |request: InitializeRequest,
                       responder: Responder<InitializeResponse>,
                       _connection| {
                    responder.respond(InitializeResponse::new(request.protocol_version))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |request: CancelNotification, connection: ConnectionTo<Client>| {
                    connection.send_notification(SessionNotification::new(
                        request.session_id,
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new("native update"),
                        ))),
                    ))?;
                    self.cancelled.notify_one();
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_to(client)
            .await
    }
}

#[tokio::test]
async fn a_paused_task_snapshot_keeps_native_observation_and_cancel_flowing() -> Result<()> {
    for selecting in [false, true] {
        let directory = tempfile::tempdir()?;
        let mut handle = session(directory.path()).await?;
        let service = handle.service.clone();
        service
            .observe(observation(
                "first",
                "session/prompt",
                "request",
                json!({"sessionId":"public","prompt":[]}),
            ))
            .await?;
        service
            .observe(observation(
                "first",
                "session/prompt",
                "response",
                json!({"stopReason":"end_turn"}),
            ))
            .await?;
        let expected = service
            .task_status(TaskStatusRequest {
                session_id: "public".into(),
            })
            .await?
            .current
            .context("initial task")?;
        let probe = Arc::new(TaskReadProbe::default());
        *service.store.task_read_probe.lock().await = Some(probe.clone());
        let cancelled = Arc::new(Notify::new());
        let controller = Controller::new(
            Native {
                cancelled: cancelled.clone(),
            },
            ControllerConfig::new(ControllerIdentity::new("fixture", "fixture", "test")),
        )
        .session_observer(service);
        let (manager, controller_side) = agent_client_protocol::Channel::duplex();
        let worker = tokio::spawn(controller.run(controller_side));
        let client = AcpClient::connect(manager, ClientOptions::default()).await?;
        let mut updates = client.subscribe_raw_updates();
        let query = {
            let client = client.clone();
            tokio::spawn(async move {
                if selecting {
                    client
                        .task_select(TaskSelectRequest {
                            session_id: "public".into(),
                            request_id: "select".into(),
                            expected,
                            mode: TaskSelectionMode::Retry,
                        })
                        .await
                } else {
                    client.task_status("public").await
                }
            })
        };
        tokio::time::timeout(Duration::from_secs(10), probe.entered.notified())
            .await
            .context("task reader reached database boundary")?;
        // The first cancel causes a native notification. Its real observer must
        // persist and forward it while the unrelated task snapshot stays paused.
        client.cancel("public").await?;
        tokio::time::timeout(Duration::from_secs(10), cancelled.notified())
            .await
            .context("first cancel forwarded")?;
        tokio::time::timeout(Duration::from_secs(10), updates.next())
            .await
            .context("native notification persisted during task read")?
            .context("forwarded native update")?;
        client.cancel("public").await?;
        tokio::time::timeout(Duration::from_secs(10), cancelled.notified())
            .await
            .context("second cancel forwarded")?;
        assert!(!query.is_finished());
        probe.release.notify_one();
        query.await??;
        client.shutdown().await?;
        worker.await??;
        handle.shutdown().await?;
    }
    Ok(())
}
