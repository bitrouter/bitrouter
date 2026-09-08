//! Application-owned task selection over the controller's ACP connection.
//! These identifiers describe evaluation attempts, not native harness sessions.
//! Custom methods and advertised capabilities follow
//! <https://agentclientprotocol.com/protocol/v1/extensibility>.

/// The application-confirmed position used to reject stale task selections.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskCursor {
    /// Stable logical task identity, retained by a retry.
    pub task_id: String,
    /// Current execution attempt within that task.
    pub attempt_id: String,
    /// Revision of prompt membership and active-attempt selection. Evaluation
    /// revisions do not change this cursor.
    pub revision: u64,
}

/// How the next new prompt changes the application task identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSelectionMode {
    /// Start a separate task and its first attempt.
    NewTask,
    /// Start another attempt for the current task.
    Retry,
}

/// Reserve an application task transition without changing native context.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    agent_client_protocol::JsonRpcRequest,
)]
#[request(method = "_bitrouter/task/select", response = TaskStatusResponse)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSelectRequest {
    /// The adapter's ACP conversation id.
    pub session_id: String,
    /// An idempotency key retained when retrying an uncertain response.
    pub request_id: String,
    /// State previously returned by task status.
    pub expected: TaskCursor,
    /// Whether to retain or replace the logical task identity.
    pub mode: TaskSelectionMode,
}

/// Query the application task associated with one ACP conversation.
#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[request(method = "_bitrouter/task/status", response = TaskStatusResponse)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskStatusRequest {
    /// The adapter's ACP conversation id.
    pub session_id: String,
}

/// A committed selection that has not yet consumed a new prompt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingTaskSelection {
    /// The idempotency key originally submitted by the manager.
    pub request_id: String,
    /// Identity change to apply at the next prompt.
    pub mode: TaskSelectionMode,
}

/// Application status; this response is not an evaluation evidence package.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    agent_client_protocol::JsonRpcResponse,
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskStatusResponse {
    /// Absent until the first confirmed prompt creates an attempt.
    pub current: Option<TaskCursor>,
    /// Current application collection or evaluation phase, when a task exists.
    pub phase: Option<String>,
    /// A selection takes effect with the next new prompt operation.
    pub pending: Option<PendingTaskSelection>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::controller::{
        Controller, ControllerConfig, ControllerIdentity, SessionObservation, SessionObserver,
    };
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::schema::v1::{InitializeRequest, InitializeResponse};
    use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Observer {
        enabled: bool,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl SessionObserver for Observer {
        fn task_control_enabled(&self) -> bool {
            self.enabled
        }
        async fn observe(&self, _: SessionObservation) -> Result<(), agent_client_protocol::Error> {
            Ok(())
        }
        async fn task_status(
            &self,
            request: TaskStatusRequest,
        ) -> Result<TaskStatusResponse, agent_client_protocol::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.session_id, "public");
            Ok(status())
        }
        async fn task_select(
            &self,
            request: TaskSelectRequest,
        ) -> Result<TaskStatusResponse, agent_client_protocol::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.request_id, "selection");
            assert_eq!(request.mode, TaskSelectionMode::Retry);
            assert_eq!(Some(request.expected), status().current);
            Ok(TaskStatusResponse {
                pending: Some(PendingTaskSelection {
                    request_id: request.request_id,
                    mode: request.mode,
                }),
                ..status()
            })
        }
    }

    fn status() -> TaskStatusResponse {
        TaskStatusResponse {
            current: Some(TaskCursor {
                task_id: "task".into(),
                attempt_id: "attempt".into(),
                revision: 2,
            }),
            phase: Some("settling".into()),
            pending: None,
        }
    }

    struct Canary {
        forwarded: Arc<AtomicUsize>,
        initializing: Arc<tokio::sync::Notify>,
        finish_initialize: Arc<tokio::sync::Notify>,
    }

    struct BlockedObserver {
        entered: Arc<tokio::sync::Notify>,
        released: Arc<tokio::sync::Notify>,
    }

    struct ReleaseOnDrop(Arc<tokio::sync::Notify>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    #[async_trait::async_trait]
    impl SessionObserver for BlockedObserver {
        fn task_control_enabled(&self) -> bool {
            true
        }
        async fn observe(&self, _: SessionObservation) -> Result<(), agent_client_protocol::Error> {
            Ok(())
        }
        async fn task_status(
            &self,
            _: TaskStatusRequest,
        ) -> Result<TaskStatusResponse, agent_client_protocol::Error> {
            let _release = ReleaseOnDrop(self.released.clone());
            self.entered.notify_one();
            std::future::pending().await
        }
        async fn task_select(
            &self,
            _: TaskSelectRequest,
        ) -> Result<TaskStatusResponse, agent_client_protocol::Error> {
            self.task_status(TaskStatusRequest {
                session_id: "public".into(),
            })
            .await
        }
    }

    #[tokio::test]
    async fn abandoned_task_calls_cancel_controller_work_before_another_request()
    -> anyhow::Result<()> {
        use crate::acp::client::{AcpClient, ClientOptions};
        let observer = Arc::new(BlockedObserver {
            entered: Arc::new(tokio::sync::Notify::new()),
            released: Arc::new(tokio::sync::Notify::new()),
        });
        let finish_initialize = Arc::new(tokio::sync::Notify::new());
        finish_initialize.notify_one();
        let forwarded = Arc::new(AtomicUsize::new(0));
        let controller = Controller::new(
            Canary {
                forwarded: forwarded.clone(),
                initializing: Arc::new(tokio::sync::Notify::new()),
                finish_initialize,
            },
            ControllerConfig::new(ControllerIdentity::new("fixture", "fixture", "test")),
        )
        .session_observer(observer.clone());
        let (manager, transport) = agent_client_protocol::Channel::duplex();
        let worker = tokio::spawn(controller.run(transport));
        let client = AcpClient::connect(manager, ClientOptions::default()).await?;
        for selection in [false, true, false, true] {
            let mut call = Box::pin(async {
                if selection {
                    client
                        .task_select(TaskSelectRequest {
                            session_id: "public".into(),
                            request_id: "selection".into(),
                            expected: status().current.ok_or_else(|| {
                                crate::acp::client::tasks::TaskControlError::Unknown(
                                    anyhow::anyhow!("fixture cursor"),
                                )
                            })?,
                            mode: TaskSelectionMode::Retry,
                        })
                        .await
                } else {
                    client.task_status("public").await
                }
            });
            tokio::select! {
                _ = observer.entered.notified() => {}
                result = &mut call => { result?; anyhow::bail!("blocked observer returned early"); }
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => anyhow::bail!("observer did not start"),
            }
            assert!(
                tokio::time::timeout(std::time::Duration::ZERO, call)
                    .await
                    .is_err()
            );
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                observer.released.notified(),
            )
            .await?;
        }
        client.shutdown().await?;
        worker.await??;
        assert_eq!(forwarded.load(Ordering::SeqCst), 0);
        Ok(())
    }

    impl ConnectTo<Client> for Canary {
        async fn connect_to(
            self,
            client: impl ConnectTo<Agent>,
        ) -> Result<(), agent_client_protocol::Error> {
            let status_calls = self.forwarded.clone();
            Agent
                .builder()
                .on_receive_request(
                    async move |request: InitializeRequest,
                                responder: Responder<InitializeResponse>,
                                connection: ConnectionTo<Client>| {
                        let initializing = self.initializing.clone();
                        let finish_initialize = self.finish_initialize.clone();
                        connection.spawn(async move {
                            initializing.notify_one();
                            finish_initialize.notified().await;
                            responder.respond(InitializeResponse::new(request.protocol_version))
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_: TaskStatusRequest,
                                responder: Responder<TaskStatusResponse>,
                                _connection| {
                        status_calls.fetch_add(1, Ordering::SeqCst);
                        responder.respond(status())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_: TaskSelectRequest,
                                responder: Responder<TaskStatusResponse>,
                                _connection| {
                        self.forwarded.fetch_add(1, Ordering::SeqCst);
                        responder.respond(status())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(client)
                .await
        }
    }

    #[tokio::test]
    async fn task_client_negotiates_methods_without_sending_unadvertised_calls()
    -> anyhow::Result<()> {
        use crate::acp::client::{
            AcpClient, ClientOptions,
            tasks::{TaskControlError, TaskMethod},
        };
        for enabled in [false, true] {
            let observer = Arc::new(Observer {
                enabled,
                calls: AtomicUsize::new(0),
            });
            let forwarded = Arc::new(AtomicUsize::new(0));
            let finish_initialize = Arc::new(tokio::sync::Notify::new());
            finish_initialize.notify_one();
            let native = Canary {
                forwarded: forwarded.clone(),
                initializing: Arc::new(tokio::sync::Notify::new()),
                finish_initialize,
            };
            let mut worker = None;
            let client = if enabled {
                let controller = Controller::new(
                    native,
                    ControllerConfig::new(ControllerIdentity::new("fixture", "adapter", "test")),
                )
                .session_observer(observer.clone());
                let (manager, transport) = agent_client_protocol::Channel::duplex();
                worker = Some(tokio::spawn(controller.run(transport)));
                AcpClient::connect(manager, ClientOptions::default()).await?
            } else {
                // This native peer would accept both methods if the client
                // sent them, so its counter detects a missing local gate.
                AcpClient::connect(native, ClientOptions::default()).await?
            };
            assert_eq!(client.task_control().allows(TaskMethod::Status), enabled);
            assert_eq!(client.task_control().allows(TaskMethod::Select), enabled);
            let read = client.task_status("public").await;
            let selected = client
                .task_select(TaskSelectRequest {
                    session_id: "public".into(),
                    request_id: "selection".into(),
                    expected: status()
                        .current
                        .ok_or_else(|| anyhow::anyhow!("fixture cursor"))?,
                    mode: TaskSelectionMode::Retry,
                })
                .await;
            if enabled {
                read?;
                assert_eq!(
                    selected?
                        .pending
                        .ok_or_else(|| anyhow::anyhow!("reservation"))?
                        .request_id,
                    "selection"
                );
            } else {
                assert!(matches!(read, Err(TaskControlError::Unavailable(_))));
                assert!(matches!(selected, Err(TaskControlError::Unavailable(_))));
            }
            client.shutdown().await?;
            if let Some(worker) = worker {
                worker.await??;
            }
            assert_eq!(forwarded.load(Ordering::SeqCst), 0);
            assert_eq!(
                observer.calls.load(Ordering::SeqCst),
                if enabled { 2 } else { 0 }
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn task_extensions_are_advertised_gated_and_never_forwarded_to_the_harness()
    -> anyhow::Result<()> {
        for enabled in [false, true] {
            let observer = Arc::new(Observer {
                enabled,
                calls: AtomicUsize::new(0),
            });
            let forwarded = Arc::new(AtomicUsize::new(0));
            let initializing = Arc::new(tokio::sync::Notify::new());
            let finish_initialize = Arc::new(tokio::sync::Notify::new());
            let controller = Controller::new(
                Canary {
                    forwarded: forwarded.clone(),
                    initializing: initializing.clone(),
                    finish_initialize: finish_initialize.clone(),
                },
                ControllerConfig::new(ControllerIdentity::new("fixture", "adapter", "test")),
            )
            .session_observer(observer.clone());
            let (manager, transport) = agent_client_protocol::Channel::duplex();
            let worker = tokio::spawn(controller.run(transport));
            Client
                .builder()
                .connect_with(manager, async |connection: ConnectionTo<Agent>| {
                    // Conductor requires initialize as the first message. Hold
                    // the native response to exercise the controller's gate.
                    let mut initialize = Box::pin(connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task());
                    tokio::select! {
                        result = &mut initialize => {
                            result?;
                            return Err(agent_client_protocol::Error::internal_error());
                        }
                        result = tokio::time::timeout(std::time::Duration::from_secs(10), initializing.notified()) => {
                            result.map_err(|_| agent_client_protocol::Error::internal_error())?;
                        }
                    }
                    let early = connection
                        .send_request(TaskStatusRequest { session_id: "public".into() })
                        .block_task().await
                        .err().ok_or_else(agent_client_protocol::Error::internal_error)?;
                    assert_eq!(i32::from(early.code), -32600);
                    let early = connection.send_request(TaskSelectRequest {
                        session_id: "public".into(), request_id: "selection".into(),
                        expected: TaskCursor { task_id: "task".into(), attempt_id: "attempt".into(), revision: 2 },
                        mode: TaskSelectionMode::Retry,
                    }).block_task().await
                        .err().ok_or_else(agent_client_protocol::Error::internal_error)?;
                    assert_eq!(i32::from(early.code), -32600);
                    assert_eq!(observer.calls.load(Ordering::SeqCst), 0);
                    finish_initialize.notify_one();
                    let init = initialize.await?;
                    let metadata = serde_json::to_value(&init)
                        .map_err(|_| agent_client_protocol::Error::internal_error())?;
                    assert_eq!(
                        metadata
                            .pointer("/_meta/bitrouter.dev~1controller/taskControl/version")
                            .and_then(serde_json::Value::as_str),
                        enabled.then_some("1")
                    );
                    let result = connection
                        .send_request(TaskStatusRequest {
                            session_id: "public".into(),
                        })
                        .block_task()
                        .await;
                    assert_eq!(result.is_ok(), enabled);
                    let selected = connection
                        .send_request(TaskSelectRequest {
                            session_id: "public".into(),
                            request_id: "selection".into(),
                            expected: TaskCursor {
                                task_id: "task".into(),
                                attempt_id: "attempt".into(),
                                revision: 2,
                            },
                            mode: TaskSelectionMode::Retry,
                        })
                        .block_task()
                        .await;
                    assert_eq!(selected.is_ok(), enabled);
                    assert_eq!(
                        observer.calls.load(Ordering::SeqCst),
                        if enabled { 2 } else { 0 }
                    );
                    Ok(())
                })
                .await?;
            worker.await??;
            assert_eq!(forwarded.load(Ordering::SeqCst), 0);
        }
        Ok(())
    }
}
