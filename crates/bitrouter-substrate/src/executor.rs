//! Substrate executor — bridges the SDK's typed `acp::Pipeline` to a single
//! upstream ACP connection.
//!
//! [`SessionExecutor`] implements [`bitrouter_sdk::acp::Executor`] over one
//! [`UpstreamConnection`] (one session per process, no connection pool). Streaming
//! updates and permission requests ride the callback plane exposed by `up.rs`,
//! not the `AcpResponse` return value.

use std::sync::Arc;

use agent_client_protocol_schema::v1::{PromptResponse, StopReason};
use async_trait::async_trait;
use bitrouter_sdk::acp::{AcpRequest, AcpRequestPayload, AcpResponse, AcpTarget, Executor};
use bitrouter_sdk::error::{BitrouterError, Result};

use crate::up::UpstreamConnection;

/// Drives a single upstream ACP connection for the `acp::Pipeline`.
///
/// Owns one [`UpstreamConnection`] shared via `Arc` so the turn-queue (engine)
/// and the executor can both reference the same session. Streaming updates and
/// permission requests arrive on `UpstreamConnection`'s broadcast / mpsc planes;
/// the `AcpResponse` carries only the final typed result.
pub struct SessionExecutor {
    conn: Arc<UpstreamConnection>,
}

impl SessionExecutor {
    /// Wrap an existing upstream connection.
    pub fn new(conn: Arc<UpstreamConnection>) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl Executor for SessionExecutor {
    async fn execute(&self, _target: &AcpTarget, request: &AcpRequest) -> Result<AcpResponse> {
        match &request.payload {
            AcpRequestPayload::Prompt(p) => {
                let result = self.conn.prompt_typed(p.clone()).await.map_err(|e| {
                    BitrouterError::Upstream {
                        status: 502,
                        message: e.to_string(),
                    }
                })?;
                Ok(AcpResponse {
                    request_id: request.request_id.clone(),
                    result,
                })
            }
            AcpRequestPayload::Cancel { session_id } => {
                self.conn
                    .cancel(session_id)
                    .await
                    .map_err(|e| BitrouterError::Upstream {
                        status: 502,
                        message: e.to_string(),
                    })?;
                // `session/cancel` has no prompt body; return a minimal
                // PromptResponse confirming cancellation.
                Ok(AcpResponse {
                    request_id: request.request_id.clone(),
                    result: cancelled_response(),
                })
            }
        }
    }
}

/// Minimal `PromptResponse` for a cancel acknowledgement. ACP v1 spec §4.3
/// states the agent MUST eventually reply with `StopReason::Cancelled` after a
/// `session/cancel` notification; this synthesises that acknowledgement on the
/// client side when we have already enqueued the cancel notification.
fn cancelled_response() -> PromptResponse {
    PromptResponse::new(StopReason::Cancelled)
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use agent_client_protocol_schema::v1::{ContentBlock, SessionId, StopReason, TextContent};
    use async_trait::async_trait;
    use bitrouter_sdk::acp::{
        AcpRequest, AcpRequestPayload, AcpTarget, AcpTransport, Executor, PipelineBuilder,
        RoutingTable,
    };
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::error::{BitrouterError, Result};

    use super::SessionExecutor;
    use crate::up::UpstreamConnection;

    /// Minimal routing table that accepts a single well-known agent name.
    struct StaticTable;

    #[async_trait]
    impl RoutingTable for StaticTable {
        async fn resolve(&self, agent: &str, _caller: &CallerContext) -> Result<AcpTarget> {
            if agent == "test-agent" {
                Ok(AcpTarget {
                    agent_name: agent.to_string(),
                    transport: AcpTransport::Stdio {
                        command: "/bin/true".into(),
                        args: vec![],
                        env: Default::default(),
                    },
                })
            } else {
                Err(BitrouterError::NotFound(format!("no agent '{agent}'")))
            }
        }
    }

    /// Bash stub that implements the ACP handshake + prompt.
    ///
    /// Returns `stopReason: "end_turn"` on a `session/prompt`.
    const BASH_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"s1"}}\n' "$id";;
            *session/prompt*) printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
          esac
        done
    "#;

    async fn make_conn() -> (Arc<UpstreamConnection>, String) {
        let conn = Arc::new(
            UpstreamConnection::spawn(
                "bash",
                &["-c".to_string(), BASH_STUB.to_string()],
                &HashMap::new(),
            )
            .await
            .expect("spawn upstream"),
        );
        let session_id = conn
            .new_session(std::path::PathBuf::from("/"), vec![])
            .await
            .expect("session/new")
            .acp_session_id;
        (conn, session_id)
    }

    fn prompt_request(session_id: &str) -> AcpRequest {
        AcpRequest::new(
            "test-agent",
            AcpRequestPayload::Prompt(agent_client_protocol_schema::v1::PromptRequest::new(
                SessionId::new(session_id),
                vec![ContentBlock::Text(TextContent::new("hello".to_string()))],
            )),
            CallerContext::new("k", "u"),
        )
    }

    // ── Step 1 (TDD): this test was written first and drove the implementation ──

    #[tokio::test]
    async fn session_executor_routes_prompt_via_pipeline() {
        let (conn, session_id) = make_conn().await;
        let executor = Arc::new(SessionExecutor::new(Arc::clone(&conn)));

        let mut b = PipelineBuilder::new();
        b.routing_table(Arc::new(StaticTable)).executor(executor);
        let pipeline = b.build().expect("build pipeline");

        let req = prompt_request(&session_id);
        let resp = pipeline.execute(req).await.expect("pipeline execute");
        assert_eq!(resp.result.stop_reason, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn session_executor_cancel_returns_cancelled_response() {
        let (conn, session_id) = make_conn().await;
        let executor = SessionExecutor::new(Arc::clone(&conn));

        // Build a dummy target (not actually used for cancel routing).
        let target = AcpTarget {
            agent_name: "test-agent".to_string(),
            transport: AcpTransport::Stdio {
                command: "/bin/true".into(),
                args: vec![],
                env: Default::default(),
            },
        };
        let req = AcpRequest::new(
            "test-agent",
            AcpRequestPayload::Cancel {
                session_id: session_id.clone(),
            },
            CallerContext::new("k", "u"),
        );
        let resp = executor.execute(&target, &req).await.expect("cancel");
        assert_eq!(resp.result.stop_reason, StopReason::Cancelled);
    }
}
