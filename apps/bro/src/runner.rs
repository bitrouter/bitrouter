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

pub(crate) const MAX_TOOL_CALLS: usize = 128;

pub(crate) trait Journal {
    fn save(&self, thread: &Thread) -> Result<()>;

    fn live(&self, thread_id: Uuid, turn_id: Uuid, event: TurnEventPayload) -> Result<()> {
        crate::emit(&json!({"type":"live","thread_id":thread_id,"turn_id":turn_id,"event":event}))
    }
}

impl Journal for Home {
    fn save(&self, thread: &Thread) -> Result<()> {
        Home::save(self, thread)
    }
}

pub(crate) fn cancel_turn(home: &dyn Journal, thread: &mut Thread) -> Result<()> {
    cancel_known(thread)?;
    home.save(thread)
}

fn cancel_known(thread: &mut Thread) -> Result<()> {
    if thread.current()?.outcome.is_some() {
        return Ok(());
    }
    for child in thread.current_mut()?.children.values_mut() {
        cancel_known(child)?;
    }
    crate::tree::collect_conclusions(thread)?;
    let unknown = thread.current()?.next() == Next::Blocked
        || thread.current()?.response().is_some_and(|response| {
            response.calls.iter().any(|call| {
                matches!(
                    call.state,
                    ToolState::Child
                        | ToolState::ToolIntent
                        | ToolState::ToolResult {
                            effect: EffectStatus::Unknown,
                            ..
                        }
                )
            })
        });
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
    let (status, detail) = if unknown {
        (
            TurnStatus::RecoveryRequired,
            "cancelled with an unresolved model request or tool effect",
        )
    } else {
        (TurnStatus::Cancelled, "cancelled")
    };
    thread.current_mut()?.outcome = Some(Outcome {
        status,
        detail: detail.into(),
        final_answer: thread.current()?.final_answer(),
    });
    Ok(())
}

pub(crate) fn settle(
    home: &dyn Journal,
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
    thread.verify_workspace()?;
    let pending = thread
        .current()?
        .pending()
        .context("turn has no pending approval")?;
    ensure!(
        pending.request_id == request_id,
        "pending approval request ID mismatch"
    );
    if let Some(check) = &mut thread.current_mut()?.check
        && check.approval.request.request_id == request_id
    {
        check.approval.approved = Some(approved);
        return home.save(thread);
    }
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

pub(crate) async fn cancellation(path: PathBuf, token: CancellationToken) {
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
    if matches!(
        thread.current()?.next(),
        Next::Verify | Next::Model | Next::Tool(_)
    ) {
        home.check_owner(thread)?;
    }
    let scheduler = thread
        .managed()
        .then(|| crate::tree::Scheduler::restore(thread))
        .transpose()?;
    let watch = tokio::spawn(cancellation(path, cancel.clone()));
    let result = drive(home, thread, client, &cancel, scheduler.as_ref(), 0).await;
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
        "verification":turn.verification(),"verification_evidence":turn.evidence(),"unknown_effect":status == TurnStatus::RecoveryRequired,
        "detail":turn.outcome.as_ref().map(|o| &o.detail),"final_answer":turn.outcome.as_ref().and_then(|o| o.final_answer.as_ref())}),
    )
}

pub(crate) async fn drive(
    home: &dyn Journal,
    thread: &mut Thread,
    client: &GatewayClient,
    cancel: &CancellationToken,
    scheduler: Option<&crate::tree::Scheduler>,
    depth: u32,
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
        let turn = thread.current()?;
        let next = turn.next();
        if let Some(tree) = scheduler {
            if next == Next::Blocked
                || turn
                    .outcome
                    .as_ref()
                    .is_some_and(|o| o.status == TurnStatus::RecoveryRequired)
            {
                tree.fenced.set(true);
            }
            if tree.fenced.get() && matches!(next, Next::Verify | Next::Tool(_) | Next::Model) {
                return Ok(());
            }
        }
        if matches!(
            next,
            Next::Verify | Next::Model | Next::Tool(_) | Next::Complete
        ) {
            thread.verify_workspace()?;
        }
        match next {
            Next::Settled | Next::Waiting => return Ok(()),
            Next::Blocked => {
                let answer = turn.final_answer();
                settle(
                    home,
                    thread,
                    TurnStatus::RecoveryRequired,
                    "unresolved model request or tool effect; refusing replay",
                    answer,
                )?;
            }
            Next::Complete => {
                let response = thread
                    .current()?
                    .response()
                    .context("missing final response")?;
                let final_answer = turn.final_answer();
                if response.finish_reason == "stop" {
                    let passed = matches!(
                        turn.verification(),
                        bitrouter_ai::harness::turn::VerificationStatus::Passed
                            | bitrouter_ai::harness::turn::VerificationStatus::NotRequested
                    );
                    settle(
                        home,
                        thread,
                        if passed {
                            TurnStatus::Completed
                        } else {
                            TurnStatus::Failed
                        },
                        if passed {
                            "completed"
                        } else {
                            "verification did not pass"
                        },
                        final_answer,
                    )?;
                } else {
                    let reason = format!("gateway finished with {}", response.finish_reason);
                    settle(home, thread, TurnStatus::Failed, &reason, final_answer)?;
                }
            }
            Next::Verify => {
                let _slot = match scheduler {
                    Some(tree) => Some(tree.tools.acquire().await?),
                    None => None,
                };
                if !cancel.is_cancelled() && !scheduler.is_some_and(|tree| tree.fenced.get()) {
                    crate::verification::run(home, thread, &tools, cancel).await?;
                }
            }
            Next::Tool(index) => {
                if let Some(tree) = scheduler
                    && crate::tree::is_spawn(thread, index)?
                {
                    tree.children(home, thread, client, cancel, depth, index)
                        .await?;
                } else {
                    let _slot = match scheduler {
                        Some(tree) => Some(tree.tools.acquire().await?),
                        None => None,
                    };
                    if !cancel.is_cancelled() && !scheduler.is_some_and(|tree| tree.fenced.get()) {
                        run_tool(home, thread, &tools, index, cancel).await?;
                    }
                }
            }
            Next::Model => {
                let _slot = match scheduler {
                    Some(tree) => Some(tree.models.acquire().await?),
                    None => None,
                };
                if !cancel.is_cancelled() && !scheduler.is_some_and(|tree| tree.fenced.get()) {
                    run_model(home, thread, client, &tools, cancel, scheduler).await?;
                }
            }
        }
    }
}

async fn run_model(
    home: &dyn Journal,
    thread: &mut Thread,
    client: &GatewayClient,
    tools: &WorkspaceTools,
    cancel: &CancellationToken,
    scheduler: Option<&crate::tree::Scheduler>,
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
    if scheduler.is_some_and(|tree| !tree.admit_model()) {
        return settle(
            home,
            thread,
            TurnStatus::Failed,
            "tree model attempt limit exceeded",
            None,
        );
    }
    let step_id = Uuid::new_v4();
    thread.current_mut()?.steps.push(Step {
        step_id,
        caller_request_id: Some(step_id),
        request_id: None,
        execution_id: None,
        partial: None,
        state: ModelState::ModelRequest,
    });
    home.save(thread)?;
    let reply = tokio::select! {
        biased;
        _ = cancel.cancelled() => return cancel_turn(home,thread),
        reply = client.chat_reply_with_correlation(&request, step_id) => reply,
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            if let bitrouter_sdk::error::Error::Service(failure) = &error {
                // A rejected or malformed reply can still identify a paid execution.
                let identity = |name: &str| {
                    let mut values = failure.headers.get_all(name).iter();
                    let value = values.next()?.to_str().ok()?;
                    (values.next().is_none()
                        && !value.is_empty()
                        && value.len() <= 256
                        && value.bytes().all(|b| b.is_ascii_graphic()))
                    .then(|| value.to_owned())
                };
                let step = thread.step_mut()?;
                step.request_id = identity("x-bitrouter-request-id");
                step.execution_id = identity("x-bitrouter-execution-id");
            }
            // HTTP denial can follow an upstream send and failed receipt publication.
            settle(
                home,
                thread,
                TurnStatus::RecoveryRequired,
                &error.to_string(),
                None,
            )?;
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
                StreamItem::Event(event) => model.push(event.data, |text| {
                    home.live(
                        thread.thread_id,
                        thread.current()?.turn_id,
                        TurnEventPayload::AssistantDelta {
                            item_id: step_id.to_string(),
                            text: text.into(),
                        },
                    )
                })?,
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
            let over_limit = thread.current()?.tool_calls() + response.calls.len() > MAX_TOOL_CALLS;
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

fn denied(reason: &str) -> ToolResultOutput {
    ToolResultOutput::ExecutionDenied {
        reason: Some(reason.into()),
    }
}

async fn run_tool(
    home: &dyn Journal,
    thread: &mut Thread,
    tools: &WorkspaceTools,
    index: usize,
    cancel: &CancellationToken,
) -> Result<()> {
    let thread_id = thread.thread_id;
    let turn_id = thread.current()?.turn_id;
    let response = thread.response_mut()?;
    let call = &response.message.tool_calls[index];
    let name = call.function.name.clone();
    let args = call.function.arguments.clone();
    let inv = &mut response.calls[index];
    let item_id = inv.item_id.to_string();
    let decision = inv.approval.as_ref().and_then(|a| a.approved);
    let (output, effect) = if let Err(reason) = tools.authorize(&name) {
        (denied(&reason), EffectStatus::NotExecuted)
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
                    home.live(thread_id, turn_id, event)?;
                },
                result = &mut execution => break result,
            }
        }
    };
    thread.response_mut()?.calls[index].state = ToolState::ToolResult { output, effect };
    home.save(thread)
}
