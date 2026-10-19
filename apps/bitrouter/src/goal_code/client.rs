use super::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }
    pub async fn request(&self, command: GoalCommand) -> Result<GoalReply> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let command = crate::daemon::DaemonCommand::Goals {
                protocol: bitrouter_orchestrator::goal::PROTOCOL_VERSION,
                command,
            };
            let mut bytes = serde_json::to_vec(&command)?;
            anyhow::ensure!(
                bytes.len() <= bitrouter_orchestrator::goal::MAX_RECORD_BYTES,
                "Goal request exceeds byte limit"
            );
            bytes.push(b'\n');
            let mut stream = crate::daemon::transport::connect(&self.socket).await?;
            stream.write_all(&bytes).await?;
            stream.flush().await?;
            let mut reader = BufReader::new(
                stream.take((2 * bitrouter_orchestrator::goal::MAX_RECORD_BYTES) as u64),
            );
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            anyhow::ensure!(
                line.ends_with('\n'),
                "Goal response closed or exceeded byte limit"
            );
            match serde_json::from_str::<crate::daemon::DaemonResponse>(&line)? {
                crate::daemon::DaemonResponse::Goals { protocol, response }
                    if protocol == bitrouter_orchestrator::goal::PROTOCOL_VERSION =>
                {
                    response.map_err(anyhow::Error::new)
                }
                _ => anyhow::bail!(
                    "daemon has no matching Goal service; the UI cannot substitute a native Thread"
                ),
            }
        })
        .await
        .context("Goal request timed out; mutation acceptance may be unknown")?
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Pending {
    pub command: GoalCommand,
    pub text: Option<String>,
    pub target: Option<String>,
}

impl Pending {
    pub(super) fn verifies(&self, operation: &GoalOperation) -> Result<()> {
        use sha2::{Digest, Sha256};
        anyhow::ensure!(
            operation.id == self.id()?
                && operation.fingerprint
                    == hex::encode(Sha256::digest(serde_json::to_vec(&self.command)?))
                && self
                    .target
                    .as_ref()
                    .is_none_or(|target| target == &operation.goal_id),
            "receipt does not match the original Goal operation"
        );
        Ok(())
    }
    pub(super) fn id(&self) -> Result<&str> {
        match &self.command {
            GoalCommand::Create { operation_id, .. }
            | GoalCommand::Input { operation_id, .. }
            | GoalCommand::Control { operation_id, .. }
            | GoalCommand::RespondDecision { operation_id, .. }
            | GoalCommand::AcceptDelivery { operation_id, .. }
            | GoalCommand::SetBudget { operation_id, .. } => Ok(operation_id),
            _ => anyhow::bail!("pending request must be a mutation"),
        }
    }
}

pub(super) async fn prepare(client: &Client, workspace: &Path, effect: &Effect) -> Result<Pending> {
    let operation_id = uuid::Uuid::new_v4().to_string();
    let (command, text, target) = match effect {
        Effect::Create { text, limits } => (
            GoalCommand::Create {
                operation_id,
                title: text
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("New Goal")
                    .chars()
                    .take(64)
                    .collect(),
                workspace: workspace.into(),
                objective: text.clone(),
                budget_limits: budget_limits(limits),
            },
            Some(text.clone()),
            None,
        ),
        Effect::SetBudget {
            goal_id,
            original,
            limits,
        } => {
            let GoalReply::Snapshot { goal } = client
                .request(GoalCommand::Get {
                    goal_id: goal_id.clone(),
                })
                .await?
            else {
                anyhow::bail!("expected Goal snapshot before budget admission");
            };
            anyhow::ensure!(
                goal.id == *goal_id,
                "snapshot does not match the requested Goal"
            );
            anyhow::ensure!(
                goal.budget.limits == budget_limits(original),
                "Budget limits changed since this form opened; reopen the form before updating"
            );
            (
                GoalCommand::SetBudget {
                    operation_id,
                    goal_id: goal_id.clone(),
                    expected_revision: goal.revision,
                    limits: budget_limits(limits),
                },
                None,
                Some(goal_id.clone()),
            )
        }
        Effect::Submit { goal_id, .. } | Effect::Control { goal_id, .. } => {
            let GoalReply::Snapshot { goal } = client
                .request(GoalCommand::Get {
                    goal_id: goal_id.clone(),
                })
                .await?
            else {
                anyhow::bail!("expected Goal snapshot before admission");
            };
            anyhow::ensure!(
                goal.id == *goal_id,
                "snapshot does not match the requested Goal"
            );
            let command = if let Effect::Submit { text, .. } = effect {
                let task_reference = text
                    .strip_prefix("/task ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .map(str::to_owned);
                GoalCommand::Input {
                    operation_id,
                    goal_id: goal_id.clone(),
                    expected_revision: goal.revision,
                    text: text.clone(),
                    task_reference,
                }
            } else if let Effect::Control { intent, .. } = effect {
                GoalCommand::Control {
                    operation_id,
                    goal_id: goal_id.clone(),
                    expected_revision: goal.revision,
                    intent: match intent {
                        Control::Pause => RunIntent::Pause,
                        Control::Stop => RunIntent::Stop,
                        Control::Run => RunIntent::Run,
                    },
                }
            } else {
                anyhow::bail!("unexpected mutation");
            };
            (
                command,
                if let Effect::Submit { text, .. } = effect {
                    Some(text.clone())
                } else {
                    None
                },
                Some(goal_id.clone()),
            )
        }
        Effect::Respond {
            goal_id,
            decision_id,
            requirements_revision,
            option_id,
        } => (
            GoalCommand::RespondDecision {
                operation_id,
                goal_id: goal_id.clone(),
                decision_id: decision_id.clone(),
                expected_requirements_revision: *requirements_revision,
                option_id: option_id.clone(),
            },
            None,
            Some(goal_id.clone()),
        ),
        Effect::Accept {
            goal_id,
            attempt_id,
            requirements_revision,
            delivery_sha256,
        } => (
            GoalCommand::AcceptDelivery {
                operation_id,
                goal_id: goal_id.clone(),
                expected_requirements_revision: *requirements_revision,
                attempt_id: attempt_id.clone(),
                delivery_sha256: delivery_sha256.clone(),
            },
            None,
            Some(goal_id.clone()),
        ),
        _ => anyhow::bail!("effect is observation only"),
    };
    Ok(Pending {
        command,
        text,
        target,
    })
}

fn budget_limits(
    limits: &bitrouter_tui::goals::budget::Limits,
) -> bitrouter_orchestrator::goal::budget::Limits {
    bitrouter_orchestrator::goal::budget::Limits {
        elapsed_ms: limits.elapsed_ms,
        managed_token_estimates_micro_usd: limits.managed_token_estimates_micro_usd,
    }
}
