//! Operator verification is a separate local shell invocation. Only a durable
//! result can discharge its intent, including after the foreground worker exits.
use anyhow::{Context, Result, ensure};
use bitrouter_ai::harness::agent::ToolMode;
use bitrouter_ai::harness::runtime::EffectStatus;
use bitrouter_ai::harness::turn::{
    InputRequest, TurnStatus, VerificationEvidence, VerificationStatus,
};
use bitrouter_ai::types::ToolResultOutput;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::checkpoint::{Approval, Next, Thread, Turn};
use crate::runner::Journal;
use crate::tools::WorkspaceTools;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Check {
    pub approval: Approval,
    pub state: State,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub(crate) enum State {
    Ready,
    Intent,
    Result {
        effect: EffectStatus,
        evidence: Box<VerificationEvidence>,
    },
}

fn shell_arguments(command: &str) -> String {
    json!({"command":command}).to_string()
}

impl Check {
    pub fn new(command: &str) -> Result<Self> {
        ensure!(
            !command.trim().is_empty()
                && !command.contains('\0')
                && command.len() <= crate::gateway::MAX_CONTEXT_BYTES,
            "invalid verification command"
        );
        let arguments = shell_arguments(command);
        WorkspaceTools::validate("shell", &arguments).map_err(anyhow::Error::msg)?;
        Ok(Self {
            approval: Approval {
                request: InputRequest {
                    request_id: Uuid::new_v4().to_string(),
                    tool_id: Uuid::new_v4().to_string(),
                    tool_name: "shell".into(),
                    arguments,
                },
                approved: None,
            },
            state: State::Ready,
        })
    }

    pub fn next(&self) -> Next {
        match self.state {
            State::Ready if self.approval.approved.is_none() => Next::Waiting,
            State::Ready => Next::Verify,
            State::Intent
            | State::Result {
                effect: EffectStatus::Unknown,
                ..
            } => Next::Blocked,
            State::Result { .. } => Next::Complete,
        }
    }

    fn verdict(&self) -> VerificationStatus {
        match &self.state {
            State::Result {
                effect: EffectStatus::NotExecuted,
                ..
            } if self.approval.approved == Some(false) => VerificationStatus::Denied,
            State::Ready
            | State::Intent
            | State::Result {
                effect: EffectStatus::NotExecuted,
                ..
            } => VerificationStatus::Unavailable,
            State::Result {
                effect: EffectStatus::Completed,
                evidence,
            } if evidence.exit_status == Some(0)
                && !evidence.timed_out
                && evidence.error.is_none() =>
            {
                VerificationStatus::Passed
            }
            State::Result { .. } => VerificationStatus::Failed,
        }
    }
}

impl Turn {
    pub fn verification(&self) -> VerificationStatus {
        if let Some(check) = &self.check {
            check.verdict()
        } else if self
            .outcome
            .as_ref()
            .is_some_and(|o| o.status != TurnStatus::Completed)
        {
            VerificationStatus::Unavailable
        } else {
            VerificationStatus::NotRequested
        }
    }

    pub fn evidence(&self) -> Option<&VerificationEvidence> {
        match &self.check.as_ref()?.state {
            State::Result { evidence, .. } => Some(evidence),
            _ => None,
        }
    }
}

pub fn validate(thread: &Thread, turn: &Turn) -> Result<()> {
    let (Some(command), Some(check)) = (&thread.verification_command, &turn.check) else {
        ensure!(
            thread.verification_command.is_none() && turn.check.is_none(),
            "missing verification binding"
        );
        return Ok(());
    };
    let request = &check.approval.request;
    ensure!(
        thread.tool_mode == ToolMode::Coding
            && thread.parent_id.is_none()
            && request.tool_name == "shell"
            && request.arguments == shell_arguments(command)
            && Uuid::parse_str(&request.tool_id).is_ok()
            && Uuid::parse_str(&request.request_id).is_ok(),
        "invalid verification grant"
    );
    if !matches!(check.state, State::Ready) {
        ensure!(
            turn.response()
                .is_some_and(|r| r.calls.is_empty() && r.finish_reason == "stop")
                && check.approval.approved.is_some(),
            "verification precedes model completion or approval"
        );
        if let State::Result { effect, evidence } = &check.state {
            ensure!(
                evidence.command == *command
                    && (*effect == EffectStatus::NotExecuted
                        || check.approval.approved == Some(true)),
                "invalid verification result"
            );
        } else {
            ensure!(
                check.approval.approved == Some(true),
                "unapproved verification intent"
            );
        }
    }
    if let Some(outcome) = &turn.outcome {
        ensure!(
            check.next() != Next::Blocked || outcome.status == TurnStatus::RecoveryRequired,
            "checkpoint advances past unknown verification effect"
        );
        ensure!(
            outcome.status != TurnStatus::Completed
                || check.verdict() == VerificationStatus::Passed,
            "completed turn lacks passing verification"
        );
    }
    Ok(())
}

fn failure(command: &str, reason: String) -> VerificationEvidence {
    VerificationEvidence {
        command: command.into(),
        interpreter: None,
        exit_status: None,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        timed_out: false,
        error: Some(reason),
    }
}

fn evidence(
    command: &str,
    output: ToolResultOutput,
    effect: EffectStatus,
) -> (EffectStatus, VerificationEvidence) {
    match output {
        ToolResultOutput::Json { mut value } => {
            value["command"] = json!(command);
            if let Ok(evidence) = serde_json::from_value(value) {
                return (effect, evidence);
            }
        }
        ToolResultOutput::ErrorJson { value } => {
            let error = value["error"]
                .as_str()
                .unwrap_or("verification command failed");
            return (effect, failure(command, error.into()));
        }
        _ => {}
    }
    (
        EffectStatus::Unknown,
        failure(command, "invalid verification evidence".into()),
    )
}

pub async fn run(
    home: &dyn Journal,
    thread: &mut Thread,
    tools: &WorkspaceTools,
    cancel: &CancellationToken,
) -> Result<()> {
    let command = thread
        .verification_command
        .clone()
        .context("missing check command")?;
    let turn = thread.current()?;
    let check = turn.check.as_ref().context("missing verification")?;
    ensure!(
        check.next() == Next::Verify,
        "verification is not dispatchable"
    );
    let refused = if check.approval.approved != Some(true) {
        Some("verification denied".into())
    } else if turn.tool_calls() >= crate::runner::MAX_TOOL_CALLS {
        Some("verification exceeds tool call limit".into())
    } else {
        tools.authorize("shell").err()
    };
    let (effect, result) = if let Some(reason) = refused {
        (EffectStatus::NotExecuted, failure(&command, reason))
    } else {
        let request = check.approval.request.clone();
        thread
            .current_mut()?
            .check
            .as_mut()
            .context("missing verification")?
            .state = State::Intent;
        home.save(thread)?;
        thread.verify_workspace()?;
        if cancel.is_cancelled() {
            (
                EffectStatus::NotExecuted,
                failure(&command, "cancelled before verification".into()),
            )
        } else {
            let execution = tools.execute_with_effect(
                "shell",
                &request.arguments,
                cancel,
                &request.tool_id,
                None,
            );
            // The native shell has its own 30s deadline. Bound cleanup and pipe
            // collection as well, preserving uncertainty if either is interrupted.
            match tokio::time::timeout(std::time::Duration::from_secs(35), execution).await {
                Ok((output, effect)) => evidence(&command, output, effect),
                Err(_) => {
                    let mut result = failure(&command, "verification deadline exceeded".into());
                    result.timed_out = true;
                    (EffectStatus::Unknown, result)
                }
            }
        }
    };
    thread
        .current_mut()?
        .check
        .as_mut()
        .context("missing verification")?
        .state = State::Result {
        effect,
        evidence: Box::new(result),
    };
    home.save(thread)
}
