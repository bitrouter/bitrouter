//! Offline release of an unscoped native managed session. Owned envelopes,
//! retained artifacts and multi-agent runs require their original recovery paths.
use super::*;
use crate::storage::checkpoint::{CheckpointBatchExt, ack_for_batch, sha256};
use bitrouter_ai::core::checkpoint::{CheckpointAck, CheckpointBatch, DurableHead, ToolStartFence};
use bitrouter_ai::core::protocol::{
    HarnessManifest, Limits, OperationDisposition, OwnershipGrant, ToolExecute, ToolOutcome,
    ToolResult,
};
use bitrouter_ai::harness::agent::ToolMode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    format: u32,
    workspace: PathBuf,
    mode: ToolMode,
    grant: OwnershipGrant,
    limits: Limits,
    manifest: HarnessManifest,
    head: DurableHead,
    checkpoint: CheckpointBatch,
    ack: CheckpointAck,
    released: bool,
    starts: BTreeMap<String, Start>,
    #[serde(default)]
    owned_choices: BTreeMap<String, Value>,
    fences: BTreeSet<ToolStartFence>,
    artifacts: BTreeMap<String, Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    command: ToolExecute,
    started: bool,
    result: ToolResult,
}

pub(super) async fn threads(
    db: &DatabaseConnection,
    claimed_requests: &mut std::collections::BTreeSet<String>,
) -> Result<Threads> {
    let mut threads = Threads::new();
    // Reject scoped rows even if only part of the ownership envelope survives.
    let names = db
        .query_all(sql(
            "SELECT id FROM bro_managed_sessions ORDER BY id LIMIT 1025",
        ))
        .await?;
    ensure!(
        names.len() <= 1024,
        "managed session count exceeds transfer bound"
    );
    for name in names {
        let name: String = name.try_get_by_index(0)?;
        let row=db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT revision, CASE WHEN installation_id IS NULL AND tenant_id IS NULL AND creator_principal_id IS NULL AND session_principal_id IS NULL AND logical_name IS NULL AND harness_id IS NULL AND session_id IS NULL AND execution_revision IS NULL AND payload_bytes BETWEEN 1 AND 134217728 AND payload_bytes=length(CAST(payload AS BLOB)) THEN payload END AS payload FROM bro_managed_sessions WHERE id=?",
            [name.clone().into()])).await?.context("missing managed row")?;
        let j: Journal = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
        let (id, thread) = checked(
            db,
            claimed_requests,
            &j,
            &name,
            row.try_get("", "revision")?,
        )
        .await?;
        ensure!(
            threads.insert(id, thread).is_none(),
            "duplicate managed session identity"
        );
    }
    Ok(threads)
}

async fn checked(
    db: &DatabaseConnection,
    claimed_requests: &mut std::collections::BTreeSet<String>,
    j: &Journal,
    name: &str,
    revision: i64,
) -> Result<(Uuid, Value)> {
    j.grant.validate().map_err(|e| anyhow::anyhow!(e.message))?;
    j.head.validate().map_err(|e| anyhow::anyhow!(e.message))?;
    j.limits
        .validate()
        .map_err(|e| anyhow::anyhow!(e.message))?;
    ensure!(
        revision > 0
            && j.format == 1
            && j.released
            && j.grant.execution_epoch == 1
            && j.workspace.is_absolute()
            && j.manifest.workspace_id
                == format!(
                    "workspace_{}",
                    sha256(
                        j.workspace
                            .to_str()
                            .context("invalid M0 workspace")?
                            .as_bytes()
                    )
                )
            && j.owned_choices.is_empty()
            && j.artifacts.is_empty()
            && j.fences.is_empty(),
        "unsupported managed workspace, owner, artifacts or fences"
    );
    let p = j
        .checkpoint
        .decode(&j.limits)
        .map_err(|e| anyhow::anyhow!(e.message))?;
    let ack = ack_for_batch(&j.checkpoint, &p);
    ensure!(
        j.ack == ack
            && j.head == ack.head()
            && p.identity.session_id == j.grant.session_id
            && p.identity.execution_epoch == j.grant.execution_epoch
            && p.identity.core_instance_id == j.grant.core_instance_id
            && p.checkpoint.artifact_refs.is_empty()
            && p.tool_start_fences.is_empty(),
        "managed release ACK mismatch"
    );
    let state = transfer::decode_snapshot(&p.checkpoint.state, &p.identity, &j.limits)?;
    let event = p.events.last().context("missing release event")?;
    let op = event.payload["operation_id"]
        .as_str()
        .context("missing release operation")?;
    let release = state.releases.get(op).context("missing release record")?;
    let receipt = state
        .operations
        .get(op)
        .context("missing release receipt")?;
    ensure!(
        p.events.len() == 1
            && event.kind == "session.released"
            && state.session_id == j.grant.session_id
            && state.manifest == j.manifest
            && state.root_queue.paused
            && state.releases.len() == 1
            && release.grant == j.grant
            && release.operation_id == op
            && release.state_revision == j.head.state_revision
            && receipt.operation_id == op
            && receipt.state_revision == release.state_revision
            && receipt.disposition == OperationDisposition::Applied
            && receipt.error.is_none()
            && json!(receipt.assigned_ids)
                == json!({"session_id":j.grant.session_id,"core_instance_id":j.grant.core_instance_id,"execution_epoch":j.grant.execution_epoch.to_string()})
            && receipt.request_sha256
                == sha256(&serde_json::to_vec(
                    &json!({"type":"session.release","expected_state_revision":p.base_state_revision})
                )?)
            && event.payload == json!(receipt),
        "managed release receipt mismatch"
    );
    transfer::verify_settled(db, &state, j.mode, claimed_requests).await?;
    let root = state
        .agents
        .get(&state.agent_id)
        .context("missing managed root")?;
    let turn = root.turn.as_ref().context("missing managed turn")?;
    let run = state.run.as_ref().context("missing managed run")?;
    ensure!(
        turn.invocations.len() == j.starts.len(),
        "unmatched managed tool starts"
    );
    for inv in &turn.invocations {
        let d = &inv.dispatch;
        let start = j
            .starts
            .get(&d.invocation_id)
            .context("managed start missing")?;
        ensure!(
            d == &start.command
                && inv.result.as_ref() == Some(&start.result)
                && start.result.invocation_id == d.invocation_id
                && start.result.attempt_id == d.attempt_id
                && d.execution_epoch == j.grant.execution_epoch
                && d.workspace_id == j.manifest.workspace_id
                && d.tool_manifest_digest == j.manifest.tool_manifest_digest
                && d.run_id == run.run_id
                && d.agent_turn_id == turn.agent_turn_id
                && d.agent_id == root.agent_id
                && start.result.evidence.is_empty()
                && match start.result.status {
                    ToolOutcome::Succeeded | ToolOutcome::Failed => start.started,
                    ToolOutcome::NotExecuted | ToolOutcome::Denied => !start.started,
                    ToolOutcome::EffectUnknown => false,
                },
            "unconfirmed managed effect"
        );
    }
    let id = Uuid::parse_str(
        j.grant
            .session_id
            .strip_prefix("session_")
            .context("unsupported managed identity")?,
    )?;
    let turn_id = Uuid::parse_str(
        turn.agent_turn_id
            .strip_prefix("turn_")
            .context("unsupported managed turn identity")?,
    )?;
    let mut admissions = state
        .operations
        .iter()
        .filter(|(_, r)| r.assigned_ids.contains_key("agent_turn_id"));
    let (request_id, admission) = admissions.next().context("missing managed input receipt")?;
    let base = admission
        .state_revision
        .checked_sub(1)
        .context("invalid managed input revision")?;
    ensure!(
        admissions.next().is_none()
            && admission.operation_id == *request_id
            && admission.error.is_none()
            && admission.disposition == OperationDisposition::Accepted
            && json!(admission.assigned_ids)
                == json!({"agent_id":root.agent_id,"agent_turn_id":turn.agent_turn_id,"run_id":run.run_id})
            && admission.request_sha256
                == sha256(&serde_json::to_vec(
                    &json!({"type":"start","expected_state_revision":base,"input":run.input})
                )?),
        "conflicting managed input receipt"
    );
    let (task, instructions) = root
        .required_instructions
        .split_last()
        .context("missing managed input instructions")?;
    ensure!(
        task == &run.input.text,
        "unsupported managed input instructions"
    );
    let instructions = instructions.join("\n\n");
    let history = transfer::chat_history(&root.history)?;
    ensure!(
        history.first() == Some(&json!({"role":"user","content":run.input.text}))
            && history.last() == Some(&json!({"role":"assistant","content":run.final_answer})),
        "unsupported managed history boundary"
    );
    Ok((
        id,
        json!({"format":"bro-local/1","thread_id":id,"workspace":j.workspace,"model":run.input.model,"tool_mode":j.mode,"m0_instructions":instructions,"endpoint":"","effort":run.input.effort,"max_output_tokens":run.input.max_output_tokens,
        "turns":[{"turn_id":turn_id,"prompt":run.input.text,"request_id":request_id,"steps":[],"sealed_messages":history,"outcome":{"status":"completed","detail":format!("released managed session {name}"),"final_answer":run.final_answer}}]}),
    ))
}
