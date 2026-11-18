//! Single foreground execution owner. Each durable intent precedes its I/O.
//! Resume drives settled tool results forward and refuses ambiguous dispatch.
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use bitrouter_ai::harness::runtime::EffectStatus;
use bitrouter_ai::harness::turn::{InputRequest, TurnEventPayload, TurnStatus};
use bitrouter_ai::stream::{ProtocolEnd, StreamItem};
use bitrouter_ai::types::ToolResultOutput;
use bitrouter_sdk::gateway::{GatewayClient, reply::Body};
use futures::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::checkpoint::{Approval, Home, ModelState, Next, Outcome, Step, Thread, ToolState};
use crate::gateway::{Accumulator, request};
use crate::tools::WorkspaceTools;

pub(crate) fn cancel_turn(home: &Home, thread: &mut Thread) -> Result<()> {
    if thread.current()?.outcome.is_some() {
        return Ok(());
    }
    if thread.current()?.next() == Next::Blocked {
        settle(
            home,
            thread,
            TurnStatus::RecoveryRequired,
            "cancelled with an unresolved model request or tool effect",
            None,
        )
    } else {
        if let Ok(response) = thread.response_mut() {
            for call in &mut response.calls {
                if matches!(call.state, ToolState::Proposed) {
                    call.state = ToolState::ToolResult {
                        output: denied("cancelled before dispatch"),
                        effect: EffectStatus::NotExecuted,
                    };
                }
            }
        }
        settle(home, thread, TurnStatus::Cancelled, "cancelled", None)
    }
}

fn settle(
    home: &Home,
    thread: &mut Thread,
    status: TurnStatus,
    detail: &str,
    final_answer: Option<String>,
) -> Result<()> {
    thread.current_mut()?.outcome = Some(Outcome {
        status,
        detail: detail.into(),
        final_answer,
    });
    home.save(thread)
}

pub(crate) fn answer(
    home: &Home,
    thread: &mut Thread,
    request_id: &str,
    approved: bool,
) -> Result<()> {
    let pending = thread
        .current()?
        .pending()
        .context("turn has no pending approval")?;
    ensure!(
        pending.request_id == request_id,
        "pending approval request ID mismatch"
    );
    let approval = thread
        .response_mut()?
        .calls
        .iter_mut()
        .filter_map(|c| c.approval.as_mut())
        .find(|a| a.request.request_id == request_id)
        .context("pending approval is missing")?;
    approval.approved = Some(approved);
    home.save(thread)
}

async fn cancellation(path: PathBuf, token: CancellationToken) {
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    loop {
        if path.try_exists().unwrap_or(true) {
            break;
        }
        tokio::select! {
            _ = &mut signal => break,
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    token.cancel();
}

pub(crate) async fn run(home: &Home, thread: &mut Thread, client: &GatewayClient) -> Result<()> {
    let turn = thread.current()?.turn_id;
    let path = home.cancel_path(turn);
    let cancel = CancellationToken::new();
    if path.try_exists()? {
        cancel.cancel();
    }
    let watch = tokio::spawn(cancellation(path, cancel.clone()));
    let result = drive(home, thread, client, &cancel).await;
    watch.abort();
    result?;
    report(thread)?;
    match thread.current()?.status(false) {
        TurnStatus::Completed | TurnStatus::WaitingForInput => Ok(()),
        status => bail!("BRO turn ended with {status:?}"),
    }
}

pub(crate) fn report(thread: &Thread) -> Result<()> {
    let turn = thread.current()?;
    let status = turn.status(false);
    let kind = match status {
        TurnStatus::WaitingForInput => "input_requested",
        TurnStatus::RecoveryRequired => "blocked",
        _ => "terminal",
    };
    crate::emit(
        &json!({"type":kind,"thread_id":thread.thread_id,"turn_id":turn.turn_id,
        "status":status,"workspace":thread.workspace,"pending_input":turn.pending(),
        "detail":turn.outcome.as_ref().map(|o| &o.detail),"final_answer":turn.outcome.as_ref().and_then(|o| o.final_answer.as_ref())}),
    )
}

async fn drive(
    home: &Home,
    thread: &mut Thread,
    client: &GatewayClient,
    cancel: &CancellationToken,
) -> Result<()> {
    let tools = WorkspaceTools::new(&thread.workspace, thread.tool_mode)?;
    ensure!(
        tools.root() == thread.workspace,
        "workspace moved since admission"
    );
    loop {
        if cancel.is_cancelled() {
            return cancel_turn(home, thread);
        }
        match thread.current()?.next() {
            Next::Settled | Next::Waiting => return Ok(()),
            Next::Blocked => {
                settle(
                    home,
                    thread,
                    TurnStatus::RecoveryRequired,
                    "unresolved model request or tool effect; refusing replay",
                    None,
                )?;
            }
            Next::Complete => {
                let response = thread
                    .current()?
                    .response()
                    .context("missing final response")?;
                let final_answer = response
                    .message
                    .content
                    .as_ref()
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                if response.finish_reason == "stop" {
                    settle(
                        home,
                        thread,
                        TurnStatus::Completed,
                        "completed",
                        final_answer,
                    )?;
                } else {
                    let reason = format!("gateway finished with {}", response.finish_reason);
                    settle(home, thread, TurnStatus::Failed, &reason, final_answer)?;
                }
            }
            Next::Tool(index) => run_tool(home, thread, &tools, index, cancel).await?,
            Next::Model => run_model(home, thread, client, &tools, cancel).await?,
        }
    }
}

async fn run_model(
    home: &Home,
    thread: &mut Thread,
    client: &GatewayClient,
    tools: &WorkspaceTools,
    cancel: &CancellationToken,
) -> Result<()> {
    if thread.current()?.steps.len() >= 32 {
        settle(
            home,
            thread,
            TurnStatus::Failed,
            "model step limit exceeded",
            None,
        )?;
        return Ok(());
    }
    let request = match request(thread, tools) {
        Ok(request) => request,
        Err(error) => {
            settle(home, thread, TurnStatus::Failed, &error.to_string(), None)?;
            return Ok(());
        }
    };
    let step_id = Uuid::new_v4();
    thread.current_mut()?.steps.push(Step {
        step_id,
        request_id: None,
        execution_id: None,
        partial: None,
        state: ModelState::ModelRequest,
    });
    home.save(thread)?;
    let reply = tokio::select! {
        biased;
        _ = cancel.cancelled() => return cancel_turn(home,thread),
        reply = client.chat_reply(&request) => reply,
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            let status = if gateway_denied(&error) {
                thread.step_mut()?.state = ModelState::GatewayDenied;
                TurnStatus::Failed
            } else {
                TurnStatus::RecoveryRequired
            };
            settle(home, thread, status, &error.to_string(), None)?;
            return Ok(());
        }
    };
    let step = thread.step_mut()?;
    step.request_id = reply.metadata.request_id;
    step.execution_id = reply.metadata.execution_id;
    home.save(thread)?;
    let Body::Stream(mut stream) = reply.body else {
        bail!("gateway returned a nonstreaming reply")
    };
    let mut model = Accumulator::default();
    let complete = async {
        while let Some(item) = stream.next().await {
            match item? {
            StreamItem::Event(event) => model.push(event.data, |text| crate::emit(&json!({"type":"live",
                "thread_id":thread.thread_id,"turn_id":thread.current()?.turn_id,
                "event":TurnEventPayload::AssistantDelta {item_id:step_id.to_string(),text:text.into()}})))?,
            StreamItem::End(ProtocolEnd::ChatDone) => return Ok(()),
            _ => bail!("unexpected gateway stream terminal"),
        }
        }
        bail!("gateway stream missing Chat terminal")
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow::anyhow!("cancelled during inference; gateway completion is unknown")),
        result = complete => result,
    };
    let partial = model.partial();
    let result = result.and_then(|()| model.finish());
    if result.is_err() {
        thread.step_mut()?.partial = partial;
    }
    match result {
        Ok(response) => {
            let count: usize = thread
                .current()?
                .steps
                .iter()
                .filter_map(|s| match &s.state {
                    ModelState::ModelResponse { response } => Some(response.calls.len()),
                    _ => None,
                })
                .sum();
            let over_limit = count + response.calls.len() > 128;
            thread.step_mut()?.state = ModelState::ModelResponse {
                response: Box::new(response),
            };
            if over_limit {
                for call in &mut thread.response_mut()?.calls {
                    call.state = ToolState::ToolResult {
                        output: denied("tool call limit exceeded"),
                        effect: EffectStatus::NotExecuted,
                    };
                }
                settle(
                    home,
                    thread,
                    TurnStatus::Failed,
                    "tool call limit exceeded",
                    None,
                )?;
            } else {
                home.save(thread)?;
            }
        }
        Err(error) => settle(
            home,
            thread,
            TurnStatus::RecoveryRequired,
            &error.to_string(),
            None,
        )?,
    }
    Ok(())
}

// A provider can return 403 after dispatch. Only a gateway authentication
// or permission rejection settles a paid intent as never started.
fn gateway_denied(error: &bitrouter_sdk::error::Error) -> bool {
    use bitrouter_ai::gateway::GatewayErrorValue;
    let bitrouter_sdk::error::Error::Service(failure) = error else {
        return false;
    };
    let Some(GatewayErrorValue::Detail(detail)) = failure.body.as_ref().map(|body| &body.error)
    else {
        return false;
    };
    let code = detail
        .code
        .as_ref()
        .and_then(Option::as_ref)
        .and_then(serde_json::Value::as_str);
    matches!(
        (failure.status, code),
        (401, Some("authentication_error")) | (403, Some("permission_denied"))
    )
}

fn denied(reason: &str) -> ToolResultOutput {
    ToolResultOutput::ExecutionDenied {
        reason: Some(reason.into()),
    }
}

async fn run_tool(
    home: &Home,
    thread: &mut Thread,
    tools: &WorkspaceTools,
    index: usize,
    cancel: &CancellationToken,
) -> Result<()> {
    let mode = thread.tool_mode;
    let turn_id = thread.current()?.turn_id;
    let thread_id = thread.thread_id;
    let response = thread.response_mut()?;
    let call = &response.message.tool_calls[index];
    let name = call.function.name.clone();
    let args = call.function.arguments.clone();
    let inv = &mut response.calls[index];
    let item_id = inv.item_id.to_string();
    let decision = inv.approval.as_ref().and_then(|a| a.approved);
    let (output, effect) = if !WorkspaceTools::allowed(mode, &name) {
        (
            denied("tool unavailable in this workspace mode"),
            EffectStatus::NotExecuted,
        )
    } else if let Err(error) = WorkspaceTools::validate(&name, &args) {
        (
            ToolResultOutput::ErrorText { value: error },
            EffectStatus::NotExecuted,
        )
    } else if decision == Some(false) {
        (
            denied("native tool approval denied"),
            EffectStatus::NotExecuted,
        )
    } else if !WorkspaceTools::read_only(&name) && decision.is_none() {
        inv.approval = Some(Approval {
            request: InputRequest {
                request_id: Uuid::new_v4().to_string(),
                tool_id: item_id,
                tool_name: name,
                arguments: args,
            },
            approved: None,
        });
        return home.save(thread);
    } else {
        inv.state = ToolState::ToolIntent;
        home.save(thread)?;
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let execution = tools.execute_with_effect(&name, &args, cancel, &item_id, Some(&tx));
        tokio::pin!(execution);
        loop {
            tokio::select! {
                event = rx.recv() => if let Some(event) = event {
                    crate::emit(&json!({"type":"live","thread_id":thread_id,"turn_id":turn_id,"event":event}))?;
                },
                result = &mut execution => break result,
            }
        }
    };
    thread.response_mut()?.calls[index].state = ToolState::ToolResult { output, effect };
    home.save(thread)
}
