//! `bitrouter agent-proxy <agent_id>` — stdio bridge between a downstream
//! ACP consumer (e.g. an editor) and an upstream agent process configured
//! under `agents:` in `bitrouter.yaml`.
//!
//! Wire shape:
//!
//! ```text
//!   editor stdio  <-->  agent-proxy  <-->  upstream agent process (via [`AcpStdioExecutor`])
//! ```
//!
//! For each line received on stdin (one JSON-RPC envelope per the ACP
//! transport spec
//! <https://agentclientprotocol.com/protocol/transports>):
//! - **Request** (has `id` + `method`): route through the configured
//!   `acp::Pipeline`, write the response (with the inbound id) to stdout.
//! - **Notification** (no `id`, has `method`): forward to the upstream by
//!   crafting a one-way send via the executor's connection.
//! - **Response** (has `id`, no `method`): for v1.0 we drop these — the
//!   executor's broadcast surface fans server→client requests, but the
//!   bridge doesn't yet originate matching requests. Logged for debugging.
//!
//! Server-originated notifications and requests flow the other direction:
//! the bridge subscribes to the executor's broadcast for `agent` and
//! writes each value out as a JSON-RPC envelope on stdout.
//!
//! This is a pure pass-through router. There is no policy or settlement on
//! the ACP path in v1.0; the pipeline's hook traits exist for future use.

use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter_sdk::acp::{
    AcpRequest, AcpStdioExecutor, AcpTarget, ConfigAcpRoutingTable, Pipeline, PipelineBuilder,
};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::config::Config;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Run the stdio bridge for `agent_id` against the agents declared in
/// `config`. Blocks until stdin closes or an unrecoverable I/O error.
pub async fn run(config: Config, agent_id: &str) -> Result<()> {
    let agent_cfg = config
        .agents
        .get(agent_id)
        .cloned()
        .with_context(|| format!("no acp agent '{agent_id}' in bitrouter.yaml `agents:`"))?;
    let agent_name = agent_cfg.name.clone();
    let agents = vec![(agent_id.to_string(), agent_cfg)];
    let table = Arc::new(
        ConfigAcpRoutingTable::from_configs(agents)
            .context("building the ACP routing table for the agent-proxy bridge")?,
    );
    let executor = Arc::new(AcpStdioExecutor::new());

    let target = AcpTarget {
        agent_name: agent_name.clone(),
        transport: table
            .lookup(agent_id)
            .cloned()
            .expect("entry just built; lookup cannot miss"),
    };
    // Spin up the upstream subprocess + subscribe before forwarding any
    // editor traffic, so we don't lose early server→client notifications.
    executor
        .ensure_connected(&target)
        .await
        .context("spawning upstream agent process")?;
    let upstream_messages = executor
        .subscribe(agent_id)
        .await
        .expect("subscribe after ensure_connected");

    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(table.clone())
        .executor(executor.clone());
    let pipeline = Arc::new(
        builder
            .build()
            .context("building the agent-proxy pipeline")?,
    );

    // Single stdout writer guarded by a mutex — both the inbound→pipeline
    // task and the upstream-notification task write to the same stdout, and
    // an interleaved write would corrupt the line framing.
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));

    let stdout_for_notify = Arc::clone(&stdout);
    let agent_for_notify = agent_id.to_string();
    let mut notify_rx = upstream_messages;
    let notify_task = tokio::spawn(async move {
        loop {
            match notify_rx.recv().await {
                Ok(value) => {
                    let mut line = match serde_json::to_string(&value) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(
                                agent = %agent_for_notify,
                                %e,
                                "agent-proxy: dropping unrenderable upstream notification"
                            );
                            continue;
                        }
                    };
                    line.push('\n');
                    let mut guard = stdout_for_notify.lock().await;
                    if let Err(e) = guard.write_all(line.as_bytes()).await {
                        tracing::warn!(agent = %agent_for_notify, %e, "agent-proxy: stdout write failed");
                        return;
                    }
                    if let Err(e) = guard.flush().await {
                        tracing::warn!(agent = %agent_for_notify, %e, "agent-proxy: stdout flush failed");
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        agent = %agent_for_notify,
                        skipped = n,
                        "agent-proxy: subscriber lagged; some upstream notifications were dropped"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    let inbound_task = tokio::spawn(forward_inbound_stdin(
        pipeline,
        target,
        agent_name,
        stdout.clone(),
    ));

    // The bridge runs until stdin closes. The notification task lives as
    // long as the executor's broadcast does; we abort it after stdin closes
    // so the binary can exit.
    let inbound_result = inbound_task.await.context("inbound task panicked")?;
    notify_task.abort();
    inbound_result
}

async fn forward_inbound_stdin(
    pipeline: Arc<Pipeline>,
    target: AcpTarget,
    agent_name: String,
    stdout: Arc<Mutex<tokio::io::Stdout>>,
) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    while let Some(line) = reader
        .next_line()
        .await
        .context("agent-proxy: reading stdin")?
    {
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%e, "agent-proxy: unparseable inbound line; dropping");
                continue;
            }
        };
        let method = value
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let id = value.get("id").cloned();
        let params = value
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match (id.is_some(), method.is_empty()) {
            // Request from the editor — route through the pipeline.
            (true, false) => {
                let request = AcpRequest::new(
                    target.agent_name.clone(),
                    method.clone(),
                    params,
                    CallerContext::local(),
                );
                let inbound_id = id.unwrap_or(serde_json::Value::Null);
                let result = pipeline.execute(request).await;
                let envelope = match result {
                    Ok(resp) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": inbound_id,
                        "result": resp.result,
                    }),
                    Err(e) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": inbound_id,
                        "error": {
                            "code": -32000,
                            "message": e.to_string(),
                        },
                    }),
                };
                write_envelope(&stdout, &envelope, &agent_name).await;
            }
            // Notification from the editor — fire-and-forget. The pipeline
            // is request-response shaped; for v1.0 we surface notifications
            // as "uninteresting" diagnostic output. A future enhancement can
            // expose a notification-send path on `AcpStdioExecutor`.
            (false, false) => {
                tracing::debug!(
                    %method,
                    "agent-proxy: dropping inbound notification (v1.0 does not relay editor → agent notifications)"
                );
            }
            // Response from the editor (e.g. answering a server-initiated
            // request the proxy forwarded). v1.0 doesn't yet wire this
            // direction; the upstream will time-out waiting on its end.
            (true, true) => {
                tracing::debug!(
                    "agent-proxy: dropping inbound response (v1.0 does not yet correlate server-initiated requests)"
                );
            }
            (false, true) => {
                tracing::warn!("agent-proxy: malformed inbound envelope (neither id nor method)");
            }
        }
    }
    Ok(())
}

async fn write_envelope(
    stdout: &Arc<Mutex<tokio::io::Stdout>>,
    envelope: &serde_json::Value,
    agent_name: &str,
) {
    let mut line = match serde_json::to_string(envelope) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%e, agent = %agent_name, "agent-proxy: failed to serialise response");
            return;
        }
    };
    line.push('\n');
    let mut guard = stdout.lock().await;
    if let Err(e) = guard.write_all(line.as_bytes()).await {
        tracing::warn!(%e, agent = %agent_name, "agent-proxy: stdout write failed");
        return;
    }
    if let Err(e) = guard.flush().await {
        tracing::warn!(%e, agent = %agent_name, "agent-proxy: stdout flush failed");
    }
}
