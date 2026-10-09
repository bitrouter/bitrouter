use std::sync::Arc;

use bitrouter_ai::types::ToolResultOutput;
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, ApprovalRequest, RunEvent};
use crate::core::checkpoint::ToolStartFence;
use crate::core::protocol::{ToolExecute, ToolObservation, ToolOutcome, ToolResult, ToolStatus};
use crate::core::session::CoreSession;
use crate::store::EffectStatus;

use super::port::Port;

pub(super) async fn execute(
    agent: Agent,
    core: CoreSession,
    port: Arc<Port>,
    (dispatch, skip): (ToolExecute, bool),
    cancel: CancellationToken,
    approvals: Option<mpsc::Sender<ApprovalRequest>>,
    effects: Arc<RwLock<()>>,
) -> Result<bool, String> {
    let observation = |status| ToolObservation {
        invocation_id: dispatch.invocation_id.clone(),
        attempt_id: dispatch.attempt_id.clone(),
        status,
        evidence: Vec::new(),
    };
    let snapshot = core.snapshot().await;
    let call = snapshot
        .agents
        .get(&dispatch.agent_id)
        .and_then(|agent| agent.turn.as_ref())
        .and_then(|turn| {
            turn.invocations
                .iter()
                .find(|call| call.dispatch == dispatch)
        })
        .ok_or("native tool dispatch has no committed invocation")?;
    let item_id = call.public_call_id.clone();
    let approved = if skip {
        false
    } else if !read_only(&dispatch.tool)
        && let Some(approvals) = approvals
    {
        core.tool_status(
            &uuid::Uuid::new_v4().to_string(),
            observation(ToolStatus::WaitingApproval),
        )
        .await
        .map_err(|error| error.message)?;
        let (response, receive) = oneshot::channel();
        let request = ApprovalRequest {
            id: uuid::Uuid::new_v4().to_string(),
            tool_id: item_id.clone(),
            tool_name: dispatch.tool.clone(),
            arguments: dispatch.arguments.to_string(),
            response,
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            _ = port.stopped.cancelled() => false,
            outcome = async { approvals.send(request).await.map_err(|_| ())?; receive.await.map_err(|_| ()) } => outcome.unwrap_or(false),
        }
    } else {
        true
    };
    let result = if !approved {
        (
            crate::agent::not_executed("native tool approval denied or cancelled"),
            EffectStatus::NotExecuted,
        )
    } else if dispatch.tool != super::artifact::NAME
        && let Err(error) = agent.validate_call(&dispatch.tool, &dispatch.arguments.to_string())
    {
        (
            crate::agent::not_executed(&error),
            EffectStatus::NotExecuted,
        )
    } else {
        run_tool(&agent, &core, &port, &dispatch, &cancel, &item_id, &effects).await?
    };
    let failed = matches!(
        result.0,
        ToolResultOutput::ErrorText { .. }
            | ToolResultOutput::ErrorJson { .. }
            | ToolResultOutput::ExecutionDenied { .. }
    );
    let mut result = ToolResult {
        invocation_id: dispatch.invocation_id.clone(),
        attempt_id: dispatch.attempt_id.clone(),
        status: match result.1 {
            EffectStatus::Unknown => ToolOutcome::EffectUnknown,
            EffectStatus::NotExecuted => ToolOutcome::NotExecuted,
            EffectStatus::Completed if failed => ToolOutcome::Failed,
            EffectStatus::Completed => ToolOutcome::Succeeded,
        },
        output: serde_json::to_string(&result.0).map_err(|error| error.to_string())?,
        evidence: Vec::new(),
        workspace_revision: None,
    };
    super::artifact::offload(&port, &dispatch, &mut result, failed).await?;
    core.tool_result(&uuid::Uuid::new_v4().to_string(), result)
        .await
        .map_err(|error| error.message)?;
    Ok(failed)
}

async fn run_tool(
    agent: &Agent,
    core: &CoreSession,
    port: &Arc<Port>,
    dispatch: &ToolExecute,
    cancel: &CancellationToken,
    item_id: &str,
    effects: &Arc<RwLock<()>>,
) -> Result<(ToolResultOutput, EffectStatus), String> {
    let permit = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok((crate::agent::not_executed("cancelled before tool execution"), EffectStatus::NotExecuted)),
        _ = port.stopped.cancelled() => return Err("native tool authority stopped".into()),
        permit = agent.workers.clone().acquire_owned() => permit.map_err(|error| error.to_string())?,
    };
    let read_only = read_only(&dispatch.tool);
    let (read, write) = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok((crate::agent::not_executed("cancelled waiting for workspace"), EffectStatus::NotExecuted)),
        _ = port.stopped.cancelled() => return Err("native tool authority stopped".into()),
        guards = async {
            if read_only { (Some(effects.clone().read_owned().await), None) }
            else { (None, Some(effects.clone().write_owned().await)) }
        } => guards,
    };
    core.tool_status(
        &uuid::Uuid::new_v4().to_string(),
        ToolObservation {
            invocation_id: dispatch.invocation_id.clone(),
            attempt_id: dispatch.attempt_id.clone(),
            status: ToolStatus::Running,
            evidence: Vec::new(),
        },
    )
    .await
    .map_err(|error| error.message)?;
    let worker_agent = agent.clone();
    let token = cancel.clone();
    let name = dispatch.tool.clone();
    let arguments = dispatch.arguments.to_string();
    let worker_item = item_id.to_string();
    let events = port
        .root_agent
        .get()
        .filter(|root| *root == &dispatch.agent_id)
        .and(port.events.clone());
    let recording = Arc::clone(port);
    let artifact_core = core.clone();
    let artifact_dispatch = dispatch.clone();
    let root = port.root_agent.get() == Some(&dispatch.agent_id);
    let worker = port
        .start_tool(
            ToolStartFence {
                invocation_id: dispatch.invocation_id.clone(),
                attempt_id: dispatch.attempt_id.clone(),
            },
            || {
                tokio::spawn(async move {
                    let _guards = (permit, read, write);
                    if token.is_cancelled() {
                        return (
                            crate::agent::not_executed("cancelled at dispatch"),
                            EffectStatus::NotExecuted,
                        );
                    }
                    if root {
                        recording.recorded.lock().await.push(RunEvent::ToolStarted {
                            id: worker_item.clone(),
                            name: name.clone(),
                        });
                    }
                    if let Some(events) = &events {
                        let _ = events
                            .send(RunEvent::ToolStarted {
                                id: worker_item.clone(),
                                name: name.clone(),
                            })
                            .await;
                    }
                    if name == super::artifact::NAME {
                        return match super::artifact::read(
                            &artifact_core,
                            &recording,
                            &artifact_dispatch,
                        )
                        .await
                        {
                            Ok(output) => (output, EffectStatus::Completed),
                            Err(error) => (
                                crate::agent::not_executed(&error),
                                EffectStatus::NotExecuted,
                            ),
                        };
                    }
                    if let Some(resources) = &worker_agent.resources
                        && resources.contains(&name)
                    {
                        resources.execute(&name, &arguments, &token).await
                    } else {
                        worker_agent
                            .tools
                            .execute_with_effect(
                                &name,
                                &arguments,
                                &token,
                                &worker_item,
                                events.as_ref(),
                            )
                            .await
                    }
                })
            },
        )
        .await;
    match worker {
        Some(worker) => Ok(worker.await.unwrap_or_else(|_| {
            (
                ToolResultOutput::ErrorText {
                    value: "native tool worker lost".into(),
                },
                EffectStatus::Unknown,
            )
        })),
        None => Ok((
            crate::agent::not_executed(if port.launch.pending() {
                "not_executed_due_to_steer"
            } else {
                "tool dispatch fenced"
            }),
            EffectStatus::NotExecuted,
        )),
    }
}

pub(super) fn read_only(name: &str) -> bool {
    name == super::artifact::NAME || crate::tools::WorkspaceTools::read_only(name)
}
