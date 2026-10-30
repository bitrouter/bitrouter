//! Match real socket work to each persisted family step and its owned receipt.
use super::*;
use crate::{
    storage::checkpoint::snapshot::{RunStatus, SessionSnapshot},
    tenancy::receipts::read,
};
use bitrouter_ai::harness::inspection::AgentStatus;
use bitrouter_ai::tenancy::{
    execution::{ExecutionPage, ExecutionReceipt, Outcome, Provenance, ReceiptState},
    identity::RevisionId,
};
use std::collections::BTreeSet;

pub(super) async fn phase(
    fixture: &Fixture,
    provider: &mut Provider,
    state: &SessionSnapshot,
    family: (&str, &str),
    revision: &RevisionId,
    executions: &mut BTreeSet<String>,
) -> Result<()> {
    let (root, child) = family;
    let run = state.run.as_ref().context("completed run")?;
    ensure!(run.status == RunStatus::Completed && run.model_attempts == 3);
    let parent = state.agents.get(root).context("root state")?;
    let descendant = state.agents.get(child).context("child state")?;
    ensure!(parent.parent_id.is_none() && descendant.parent_id.as_deref() == Some(root));
    let parent_turn = parent.turn.as_ref().context("root turn")?;
    let child_turn = descendant.turn.as_ref().context("child turn")?;
    ensure!(parent_turn.run_id == run.run_id && child_turn.run_id == run.run_id);
    ensure!(
        parent_turn.status == AgentStatus::Completed && child_turn.status == AgentStatus::Completed
    );
    ensure!(parent_turn.steps.len() == 2 && child_turn.steps.len() == 1 && child_turn.notified);
    let delivered = parent
        .mailbox
        .iter()
        .filter(|mail| {
            mail.sender_id == child
                && mail.sender_turn_id == child_turn.agent_turn_id
                && mail.kind == "agent_result"
        })
        .collect::<Vec<_>>();
    ensure!(delivered.len() == 1 && delivered[0].consumed);
    let mail_id = &delivered[0].message_id;
    // Core waits for its dependent child, consumes the result mailbox, clears
    // the first final answer, and runs a second parent step on that new history.
    let first = parent_turn.steps[0]
        .plan
        .as_ref()
        .context("initial parent plan")?;
    let wake = parent_turn.steps[1]
        .plan
        .as_ref()
        .context("parent wakeup plan")?;
    ensure!(!serde_json::to_string(&first.prompt.messages)?.contains(mail_id));
    ensure!(serde_json::to_string(&wake.prompt.messages)?.contains(mail_id));
    ensure!(parent_turn.steps[1].context_revision > parent_turn.steps[0].context_revision);

    let mut pending = Vec::new();
    let mut steps = BTreeSet::new();
    let mut attempts = BTreeSet::new();
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    for turn in [parent_turn, child_turn] {
        for step in &turn.steps {
            ensure!(step.settled && !step.interrupted && step.attempts.len() == 1);
            ensure!(steps.insert(step.step_id.clone()), "duplicate Core step");
            let plan = step.plan.as_ref().context("committed plan")?;
            ensure!(plan.routes.len() == 1 && plan.routes[0].model == "model-a");
            let attempt = &step.attempts[0];
            ensure!(attempt.index == 0 && attempts.insert(attempt.attempt_id.clone()));
            let applied = attempt
                .receipt
                .as_ref()
                .context("Core attempt outcome ACK")?;
            ensure!(
                applied.attempt_id == attempt.attempt_id && applied.decision_id == step.decision_id
            );
            ensure!(
                applied.report.request_id == plan.request_id
                    && applied.report.route == plan.routes[0]
            );
            ensure!(applied.report.result.is_some() && applied.report.error.is_none());
            ensure!(
                executions.insert(plan.request_id.clone()),
                "model steps reused an execution"
            );
            let bytes = fixture
                .service
                .read_receipt(
                    fixture.operator.clone(),
                    fixture.tenant.clone(),
                    read::Request::One(plan.request_id.as_str().try_into()?),
                )
                .await
                .map_err(failure)?;
            let receipt: ExecutionReceipt = serde_json::from_slice(&bytes)?;
            ensure!(
                receipt.execution_id.as_str() == plan.request_id
                    && receipt.execution_revision == *revision
            );
            ensure!(
                receipt.installation_id == fixture.installation
                    && receipt.tenant_id == fixture.tenant
                    && receipt.principal_id == fixture.principal
            );
            ensure!(
                receipt.state == ReceiptState::Incomplete
                    && receipt.outcome == Outcome::Completed
                    && receipt.attempts.len() == 1
            );
            let actual = &receipt.attempts[0];
            let target = actual.target.as_ref().context("actual owned target")?;
            ensure!(
                actual.outcome == Outcome::Completed && target.model_id == plan.routes[0].model
            );
            ensure!(
                target.provider_id.as_str() == plan.routes[0].provider
                    && target.account_id.as_str().starts_with("acct1-")
            );
            ensure!(
                target.credential_id.as_str().starts_with("cred1-")
                    && target.credential_version.as_str() == "r1"
            );
            ensure!(
                actual.usage.provenance == Provenance::Provider
                    && actual.usage.input_tokens == Some(4)
                    && actual.usage.output_tokens == Some(2)
            );
            // This fixture reports only input/output totals. Missing cache and
            // reasoning dimensions must stay unknown even for completed sends.
            ensure!(
                !actual.usage.complete
                    && actual.usage.cache_read_tokens.is_none()
                    && actual.usage.reasoning_tokens.is_none()
            );
            pending.push(history(&plan.prompt.messages)?);
        }
    }
    ensure!(pending.len() == 3);
    for _ in 0..steps.len() {
        let request = provider.receive().await?;
        ensure!(request.path == "/v1/chat/completions");
        ensure!(
            request.body["model"] == "model-a" && request.body["max_tokens"] == 25,
            "family step substituted active R2"
        );
        ensure!(
            request
                .headers
                .get("authorization")
                .is_some_and(|value| value == "Bearer owned-upstream-secret")
        );
        let sent = wire_history(&request.body)?;
        let matches = pending
            .iter()
            .enumerate()
            .filter(|(_, expected)| **expected == sent)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        ensure!(
            matches.len() == 1,
            "socket request did not identify exactly one unobserved Core step"
        );
        pending.remove(matches[0]);
    }
    ensure!(pending.is_empty() && provider.calls() == executions.len());
    ensure!(
        matches!(
            provider.received.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ),
        "extra provider request beyond Core steps"
    );
    receipts(fixture, executions).await
}

pub(super) async fn receipts(fixture: &Fixture, expected: &BTreeSet<String>) -> Result<()> {
    let bytes = fixture
        .service
        .read_receipt(
            fixture.operator.clone(),
            fixture.tenant.clone(),
            read::Request::Page {
                correlation: "same-correlation".try_into()?,
                after: None,
                limit: 100,
            },
        )
        .await
        .map_err(failure)?;
    let page: ExecutionPage = serde_json::from_slice(&bytes)?;
    ensure!(page.next_after.is_none() && page.multiple_executions);
    ensure!(
        page.execution_ids
            .into_iter()
            .map(|id| id.to_string())
            .collect::<BTreeSet<_>>()
            == *expected,
        "receipt page differs from observed Core executions"
    );
    Ok(())
}

fn history(messages: &[Message]) -> Result<Vec<(Role, String)>> {
    messages
        .iter()
        .filter(|message| message.role != Role::System)
        .map(|message| {
            let mut text = String::new();
            for content in &message.content {
                let Content::Text { text: part, .. } = content else {
                    anyhow::bail!("family fixture emitted unexpected nontext history");
                };
                text.push_str(part);
            }
            Ok((message.role, text))
        })
        .collect()
}

fn wire_history(body: &Value) -> Result<Vec<(Role, String)>> {
    // Inspect the actual Chat Completions socket body, independently of the renderer.
    // https://platform.openai.com/docs/api-reference/chat/create
    body["messages"]
        .as_array()
        .context("wire messages")?
        .iter()
        .filter(|message| message["role"] != "system" && message["role"] != "developer")
        .map(|message| {
            let role: Role = serde_json::from_value(message["role"].clone())?;
            let text = if let Some(text) = message["content"].as_str() {
                text.to_owned()
            } else {
                let mut text = String::new();
                for part in message["content"].as_array().context("wire text content")? {
                    ensure!(part["type"] == "text", "unexpected nontext socket body");
                    text.push_str(part["text"].as_str().context("wire text")?);
                }
                text
            };
            Ok((role, text))
        })
        .collect()
}
