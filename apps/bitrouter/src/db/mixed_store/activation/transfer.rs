//! The offline harness reader checks every native batch, projection and artifact.
//! Only a released single-root local turn with complete metering evidence can move.
use super::*;
mod codec;
use crate::gateway::caller::CallerContext;
use crate::gateway::pipeline::native_accounting::{NativeCostBasis, NativeCostSource};
use crate::metering::store::MeteringStore;
use crate::storage::checkpoint::snapshot::{
    RunStatus, SessionSnapshot,
    accounting::work::{CostWorkKind, CostWorkState},
};
use crate::storage::checkpoint::{CheckpointBatchExt, ack_for_batch};
use anyhow::bail;
use bitrouter_ai::core::checkpoint::{CheckpointBatch, DurableHead};
use bitrouter_ai::core::protocol::Limits;
use bitrouter_ai::harness::agent::{AgentConfig, ToolMode};
use bitrouter_ai::harness::thread::PermissionProfile;
use bitrouter_ai::harness::thread::ThreadSnapshot;
use bitrouter_ai::harness::thread::ThreadStatus;
use bitrouter_ai::types::{Content, Message, Role, ToolResultOutput};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

pub(super) async fn threads(
    db: &DatabaseConnection,
    claimed_requests: &mut BTreeSet<String>,
) -> Result<BTreeMap<Uuid, Value>> {
    let owners=db.query_all(sql("SELECT generation,server_instance_id,stopped_at_ms FROM bro_runtime_owners ORDER BY generation LIMIT 100001")).await?;
    ensure!(
        !owners.is_empty() && owners.len() <= 100000,
        "unsupported stopped M0 owner history"
    );
    for (i, row) in owners.iter().enumerate() {
        ensure!(
            row.try_get_by_index::<i64>(0)? == i64::try_from(i + 1)?
                && !row.try_get_by_index::<String>(1)?.is_empty()
                && row.try_get_by_index::<i64>(2)? >= 0,
            "M0 owner history incomplete or still running"
        );
    }
    ensure!(count(db,"SELECT count(*) n FROM bro_runtime_ownership".into()).await?==1 && count(db,"SELECT count(*) n FROM bro_runtime_ownership c JOIN bro_runtime_owners h USING(generation,server_instance_id,stopped_at_ms) WHERE c.id=1 AND c.generation=(SELECT max(generation) FROM bro_runtime_owners)".into()).await?==1,"M0 stopped owner head mismatch");
    let rows = db
        .query_all(sql(
            "SELECT id,version,format_version FROM bro_executions ORDER BY id LIMIT 1025",
        ))
        .await?;
    ensure!(
        !rows.is_empty() && rows.len() <= 1024,
        "unsupported thread count"
    );
    ensure!(
        count(db, "SELECT count(*) n FROM bro_execution_index".into()).await?
            == i64::try_from(rows.len())?,
        "M0 index mismatch"
    );

    let mut result = BTreeMap::new();
    for row in rows {
        let id: String = row.try_get_by_index(0)?;
        let head: u64 = u64::try_from(row.try_get_by_index::<i64>(1)?)?;
        ensure!(
            row.try_get_by_index::<i64>(2)? == 7 && (1..=100000).contains(&head),
            "unsupported M0 execution head"
        );
        let uuid = id.parse::<Uuid>()?;
        ensure!(uuid.to_string() == id, "noncanonical M0 identity");
        let thread = project(db, &id, head, claimed_requests)
            .await
            .with_context(|| format!("thread {id}"))?;
        result.insert(uuid, thread);
    }
    Ok(result)
}

async fn project(
    db: &DatabaseConnection,
    id: &str,
    head: u64,
    claimed_requests: &mut BTreeSet<String>,
) -> Result<Value> {
    use futures::TryStreamExt;
    use sea_orm::sqlx;
    let rows=sqlx::query_as::<_,(i64,Option<String>)>("SELECT sequence,CASE WHEN length(CAST(payload AS BLOB)) BETWEEN 1 AND 4194304 THEN payload END FROM bro_execution_records WHERE execution_id=? ORDER BY sequence")
        .bind(id).fetch(db.get_sqlite_connection_pool());
    futures::pin_mut!(rows);
    let mut sequence = 0;
    let mut epoch = None;
    let mut header = None;
    let mut accepted = Vec::new();
    let mut queued = None;
    let mut key = None;
    let mut active = false;
    let mut core: Option<(CheckpointBatch, SessionSnapshot)> = None;
    let mut core_head = DurableHead::default();
    let mut settled = None;
    let mut released = false;
    let mut finished = false;
    let mut checkpoint = false;
    let mut requests = BTreeMap::new();
    let mut responses = BTreeMap::new();
    let mut intents = BTreeMap::new();
    let mut results = BTreeMap::new();
    while let Some((seq, raw)) = rows.try_next().await? {
        sequence += 1;
        ensure!(
            seq == sequence && u64::try_from(seq)? <= head,
            "noncontiguous M0 history"
        );
        let raw = raw.context("oversized M0 record")?;
        let v: Value = serde_json::from_str(&codec::decode(&raw).map_err(anyhow::Error::msg)?)?;
        match v["record"].as_str() {
            Some("thread_created") if seq == 1 => {
                let s: ThreadSnapshot = serde_json::from_value(v["snapshot"].clone())?;
                let c: AgentConfig = serde_json::from_value(v["config"].clone())?;
                ensure!(
                    s.thread_id == id
                        && s.status == ThreadStatus::Idle
                        && s.queued.is_empty()
                        && s.active_turn_id.is_none()
                        && s.workspace.is_absolute()
                        && s.model == c.model
                        && c.skills.is_none()
                        && c.max_spend_microusd.is_none()
                        && c.estimate_rates.is_none()
                        && c.max_steps == 32
                        && c.max_tool_calls == 128
                        && c.max_context_bytes == 524288
                        && c.max_duration == std::time::Duration::from_secs(600)
                        && c.model_mode == bitrouter_ai::core::protocol::ModelMode::Fixed
                        && v["verification_command"].is_null()
                        && v["caller"]
                            == json!({"api_key_id":"local","user_id":"local","local":true})
                        && matches!(
                            (c.tool_mode, s.permission_profile),
                            (ToolMode::ReadOnly, PermissionProfile::ReadOnly)
                                | (ToolMode::Coding, PermissionProfile::Ask)
                        ),
                    "unsupported M0 thread configuration"
                );
                epoch = Some(s.server_instance_id.clone());
                header = Some((s, c));
            }
            Some("turn_queued") if queued.is_none() && header.is_some() => {
                queued = Some((
                    field(&v, "turn_id")?.to_owned(),
                    field(&v, "prompt")?.to_owned(),
                ))
            }
            Some("turn_activated")
                if !active && queued.as_ref().is_some_and(|(id, _)| v["turn_id"] == *id) =>
            {
                active = true
            }
            Some("accepted_key") => {
                let e = &v["entry"];
                let scope = if let Some(t) = e["turn_id"].as_str() {
                    let (admitted, prompt) = queued
                        .as_ref()
                        .context("start key without an admitted turn")?;
                    ensure!(
                        admitted == t
                            && e["fingerprint"]
                                == crate::storage::checkpoint::sha256(&serde_json::to_vec(prompt)?)
                            && key.replace(field(e, "key")?.to_owned()).is_none(),
                        "start key mismatch"
                    );
                    json!(["local", "local", "start", id])
                } else {
                    json!(["local", "local", "create_thread", null])
                };
                let scope = serde_json::to_string(&scope)?;
                ensure!(
                    e["thread_id"] == id && e["scope"].as_str() == Some(scope.as_str()),
                    "acceptance scope mismatch"
                );
                accepted.push(e.clone());
            }
            Some("thread_event") => {
                ensure!(
                    v["event"]["seq"] == seq
                        && v["event"]["thread_id"] == id
                        && v["event"]["server_instance_id"].as_str() == epoch.as_deref(),
                    "event identity mismatch"
                );
            }
            Some("turn_record")
                if active && queued.as_ref().is_some_and(|(id, _)| v["turn_id"] == *id) =>
            {
                let f = &v["fact"];
                match f["record"].as_str() {
                    Some("core_checkpoint") if settled.is_none() => {
                        let batch: CheckpointBatch = serde_json::from_value(f["batch"].clone())?;
                        let limits: Limits = serde_json::from_value(f["limits"].clone())?;
                        limits.validate().map_err(|e| anyhow::anyhow!(e.message))?;
                        let payload = batch
                            .decode(&limits)
                            .map_err(|e| anyhow::anyhow!(e.message))?;
                        ensure!(
                            payload.base_event_seq == core_head.event_seq
                                && payload.base_state_revision == core_head.state_revision
                                && payload.checkpoint.artifact_refs.is_empty()
                                && payload.tool_start_fences.is_empty()
                                && batch.identity.execution_epoch == 1
                                && core
                                    .as_ref()
                                    .is_none_or(|(prev, _)| prev.identity.session_id
                                        == batch.identity.session_id
                                        && prev.identity.core_instance_id
                                            == batch.identity.core_instance_id),
                            "M0 checkpoint chain, artifacts or epoch unsupported"
                        );
                        core_head = ack_for_batch(&batch, &payload).head();
                        let state =
                            decode_snapshot(&payload.checkpoint.state, &batch.identity, &limits)?;
                        ensure!(
                            state.session_id == batch.identity.session_id
                                && state.agents.len() == 1
                                && state.cost_work.len() <= 1
                                && state.recovery_archive.is_none(),
                            "unsupported native checkpoint state"
                        );
                        if let Some((_, previous)) = &core {
                            previous
                                .validate_context_successor(&state)
                                .map_err(|e| anyhow::anyhow!(e.message))?;
                            ensure!(
                                previous.run.as_ref().is_none_or(|old| state
                                    .run
                                    .as_ref()
                                    .is_some_and(|r| r.run_id == old.run_id))
                                    && previous.cost_work.iter().all(|(id, run)| state
                                        .cost_work
                                        .get(id)
                                        .is_some_and(|new| run
                                            .work
                                            .keys()
                                            .all(|k| new.work.contains_key(k))
                                            && run
                                                .charges
                                                .iter()
                                                .all(|(key, claim)| new.charges.get(key)
                                                    == Some(claim)))),
                                "native run or cost history was discarded"
                            );
                        }
                        core = Some((batch, state));
                    }
                    Some("model_request") if settled.is_none() => ensure!(
                        f["item_id"] == f["step_id"]
                            && requests
                                .insert(field(f, "step_id")?.to_owned(), f["prompt"].clone())
                                .is_none(),
                        "duplicate model request"
                    ),
                    Some("model_response") if settled.is_none() => ensure!(
                        requests.contains_key(field(f, "step_id")?)
                            && f["item_id"] == f["step_id"]
                            && responses
                                .insert(
                                    (
                                        field(f, "step_id")?.to_owned(),
                                        field(f, "request_id")?.to_owned()
                                    ),
                                    f.clone()
                                )
                                .is_none()
                            && !f["usage"].is_null(),
                        "unpaired model response"
                    ),
                    Some("tool_intent") if settled.is_none() => ensure!(
                        intents
                            .insert(field(&f["call"], "item_id")?.to_owned(), f["call"].clone())
                            .is_none(),
                        "duplicate tool intent"
                    ),
                    Some("tool_result") if settled.is_none() => ensure!(
                        intents.contains_key(field(f, "item_id")?)
                            && matches!(f["effect"].as_str(), Some("completed" | "not_executed"))
                            && results
                                .insert(field(f, "item_id")?.to_owned(), f.clone())
                                .is_none(),
                        "unpaired or unknown tool effect"
                    ),
                    Some("core_dispatch") if settled.is_none() => ensure!(
                        f == &json!({"record":"core_dispatch"}),
                        "unsupported dispatch"
                    ),
                    Some("run_checkpoint") if settled.is_none() => ensure!(
                        f["estimated_spend_microusd"].as_u64()
                            == Some(
                                core.as_ref()
                                    .map(|(_, s)| projected_spend(s))
                                    .transpose()?
                                    .unwrap_or(0)
                            ),
                        "intermediate estimate differs from recorded work"
                    ),
                    Some("harness_inventory") if settled.is_none() => {
                        let inventory = &f["inventory"];
                        ensure!(
                            inventory.as_object().is_some_and(|o| o.len() == 5)
                                && ["tools", "instructions", "skills", "skill_problems"]
                                    .iter()
                                    .all(|k| inventory[k] == json!([])),
                            "external M0 harness resources are unsupported"
                        );
                    }
                    Some("instruction_context") if settled.is_none() => {
                        let snapshot = &f["snapshot"];
                        ensure!(
                            snapshot["materials"] == json!([])
                                && snapshot["body"] == ""
                                && snapshot["warnings"] == json!([])
                                && f["message"].is_null(),
                            "external instruction context is unsupported"
                        );
                    }
                    Some("settled")
                        if settled.is_none() && f["outcome"]["status"] == "completed" =>
                    {
                        settled = Some(f.clone())
                    }
                    Some("workspace_release_prepared") if settled.is_some() && !released => {
                        let (s, _) = header.as_ref().context("missing M0 header")?;
                        ensure!(
                            f["workspace"] == json!(s.workspace)
                                && f["execution_id"] == v["turn_id"]
                                && !field(f, "lease_id")?.is_empty(),
                            "workspace release mismatch"
                        );
                        released = true;
                    }
                    Some("turn_lifecycle")
                        if f["lifecycle"]["kind"] == "started" && settled.is_none() => {}
                    Some("turn_lifecycle") if released && !finished => {
                        let l = &f["lifecycle"];
                        ensure!(
                            f["turn_id"] == v["turn_id"]
                                && l["kind"] == "finished"
                                && l["status"] == "completed"
                                && l["unknown_effect"] == false
                                && l["verification"] == "not_requested"
                                && l["final_answer"]
                                    == settled.as_ref().context("missing outcome")?["outcome"]["final_answer"],
                            "terminal lifecycle mismatch"
                        );
                        finished = true;
                    }
                    _ => bail!("unsupported or unresolved M0 turn record"),
                }
            }
            Some("thread_checkpoint")
                if finished && !checkpoint && u64::try_from(seq + 1)? == head =>
            {
                let s: ThreadSnapshot = serde_json::from_value(v["snapshot"].clone())?;
                let (start, _) = header.as_ref().context("missing M0 header")?;
                ensure!(
                    s.cursor == head
                        && s.thread_id == id
                        && s.workspace == start.workspace
                        && s.permission_profile == start.permission_profile
                        && s.model == start.model
                        && s.server_instance_id == start.server_instance_id
                        && s.status == ThreadStatus::Idle
                        && s.queued.is_empty()
                        && s.active_turn_id.is_none()
                        && v["messages"]
                            == settled.as_ref().context("missing settlement")?["messages"],
                    "M0 final checkpoint mismatch"
                );
                checkpoint = true;
            }
            _ => bail!("unsupported M0 record sequence"),
        }
    }

    ensure!(
        u64::try_from(sequence)? == head && checkpoint,
        "unsealed M0 history"
    );
    let (snapshot, config) = header.context("missing M0 header")?;
    let owner = sqlx::query_scalar::<_, i64>(
        "SELECT stopped_at_ms FROM bro_runtime_owners WHERE server_instance_id=?",
    )
    .bind(&snapshot.server_instance_id)
    .fetch_one(db.get_sqlite_connection_pool())
    .await?;
    ensure!(owner >= 0, "source owner still running");
    let keys = sqlx::query_as::<_, (String, String, String, Option<String>)>(
        "SELECT scope,key,fingerprint,turn_id FROM bro_acceptance_keys WHERE thread_id=?",
    )
    .bind(id)
    .fetch_all(db.get_sqlite_connection_pool())
    .await?;
    ensure!(
        keys.len() == accepted.len()
            && keys.iter().all(|(scope, key, fp, turn)| accepted.contains(
                &json!({"scope":scope,"key":key,"fingerprint":fp,"thread_id":id,"turn_id":turn})
            )),
        "M0 key inventory mismatch"
    );
    let (turn, prompt) = queued.context("missing admitted turn")?;
    let key = key.context("missing historical start key")?;
    let (_, state) = core.context("missing final native checkpoint")?;
    verify_settled(db, &state, config.tool_mode, claimed_requests).await?;
    let run = state.run.as_ref().context("missing root run")?;
    let root = state.agents.get(&state.agent_id).context("missing root")?;
    let native = root.turn.as_ref().context("missing root turn")?;
    let settled = settled.context("missing settlement")?;
    ensure!(
        state.manifest.workspace_id
            == crate::storage::checkpoint::sha256(
                snapshot
                    .workspace
                    .to_str()
                    .context("non-UTF8 workspace")?
                    .as_bytes()
            )
            && state.manifest.workspace_id
                == native
                    .steps
                    .first()
                    .context("missing model step")?
                    .manifest
                    .workspace_id
            && settled["estimated_spend_microusd"].as_u64() == Some(projected_spend(&state)?)
            && settled["model_steps"] == native.steps.len()
            && settled["tool_calls"] == native.invocations.len()
            && run.input.text == prompt
            && run.input.model == config.model
            && json!(run.input.effort) == json!(config.effort)
            && run.input.max_output_tokens == config.max_output_tokens
            && serde_json::to_value(&root.history)? == settled["messages"]
            && json!(run.final_answer) == settled["outcome"]["final_answer"]
            && native.steps.len() == requests.len()
            && native.steps.len() == responses.len()
            && native.steps.iter().all(|s| requests
                .get(&s.step_id)
                .is_some_and(|prompt| prompt["messages"] == json!(s.input_history)
                    && prompt["model"] == config.model)
                && s.attempts[0].receipt.as_ref().is_some_and(|r| responses
                    .get(&(s.step_id.clone(), r.report.request_id.clone()))
                    .is_some_and(
                        |f| r.report.result.as_ref().is_some_and(|reply| f["message"]
                            == json!({"role":"assistant","content":reply.content})
                            && f["requested_model"] == config.model
                            && f["usage"] == json!(reply.usage)
                            && f["estimated_spend_microusd"].as_u64()
                                == Some(r.report.token_cost.estimated_micro_usd().unwrap_or(0)))
                    )))
            && native.invocations.len() == intents.len()
            && intents.len() == results.len(),
        "terminal native state disagrees with M0 thread history"
    );
    for i in &native.invocations {
        let call = intents
            .get(&i.public_call_id)
            .context("missing confirmed tool intent")?;
        let result = results
            .get(&i.public_call_id)
            .context("missing confirmed tool result")?;
        let native_result = i.result.as_ref().context("unknown native tool result")?;
        ensure!(
            call["provider_call_id"] == i.provider_call_id
                && call["name"] == i.dispatch.tool
                && serde_json::from_str::<Value>(field(call, "arguments")?)?
                    == i.dispatch.arguments
                && result["step_id"] == i.dispatch.step_id
                && result["message"]["content"][0]["call_id"] == i.provider_call_id
                && root
                    .history
                    .iter()
                    .any(|m| serde_json::to_value(m).ok().as_ref() == Some(&result["message"]))
                && match native_result.status {
                    bitrouter_ai::core::protocol::ToolOutcome::Succeeded
                    | bitrouter_ai::core::protocol::ToolOutcome::Failed =>
                        result["effect"] == "completed",
                    bitrouter_ai::core::protocol::ToolOutcome::NotExecuted
                    | bitrouter_ai::core::protocol::ToolOutcome::Denied =>
                        result["effect"] == "not_executed",
                    _ => false,
                },
            "M0 tool receipt mismatch"
        );
    }
    let history = chat_history(&root.history)?;
    Ok(
        json!({"format":"bro-local/1","thread_id":id,"workspace":snapshot.workspace,"model":config.model,"tool_mode":config.tool_mode,"endpoint":"","effort":config.effort,"max_output_tokens":config.max_output_tokens,"m0_instructions":config.instructions,
        "turns":[{"turn_id":turn.parse::<Uuid>()?,"prompt":prompt,"request_id":key,"steps":[],"sealed_messages":history,"outcome":settled["outcome"]}]}),
    )
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("missing M0 {key}"))
}

// M0 snapshot structures accept missing historical fields. Reject additions
// dropped by their DTOs so future opaque state cannot silently grant a transfer.
pub(super) fn decode_snapshot(
    raw: &Value,
    identity: &bitrouter_ai::core::checkpoint::BatchIdentity,
    limits: &Limits,
) -> Result<SessionSnapshot> {
    let state: SessionSnapshot = serde_json::from_value(raw.clone())?;
    fn represented(raw: &Value, typed: &Value) -> bool {
        match raw {
            Value::Object(map) => map
                .iter()
                .all(|(k, v)| typed.get(k).is_some_and(|t| represented(v, t))),
            Value::Array(items) => typed.as_array().is_some_and(|a| {
                a.len() == items.len() && items.iter().zip(a).all(|(v, t)| represented(v, t))
            }),
            _ => raw == typed,
        }
    }
    ensure!(
        represented(raw, &serde_json::to_value(&state)?),
        "unrecognized native checkpoint fields"
    );
    state
        .validate_checkpoint(identity, limits)
        .map_err(|e| anyhow::anyhow!(e.message))?;
    Ok(state)
}

// The M0 native budget sums configured token estimates for completed generation
// work. An unavailable estimate contributes zero to this display field only.
fn projected_spend(state: &SessionSnapshot) -> Result<u64> {
    state
        .run
        .as_ref()
        .and_then(|run| state.cost_work.get(&run.run_id))
        .into_iter()
        .flat_map(|ledger| ledger.work.values())
        .filter(|w| {
            w.kind == CostWorkKind::ProviderAttempt
                && w.state == CostWorkState::OutcomeRecorded
                && w.generation_usage.is_some()
        })
        .filter_map(|w| w.token_estimate.as_ref()?.estimated_micro_usd())
        .try_fold(0u64, |total, n| {
            total
                .checked_add(n)
                .context("native estimated spend overflow")
        })
}

pub(super) async fn verify_settled(
    db: &DatabaseConnection,
    state: &SessionSnapshot,
    mode: ToolMode,
    claimed_requests: &mut BTreeSet<String>,
) -> Result<()> {
    let names: BTreeSet<_> = state
        .manifest
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    let expected: &[&str] = match mode {
        ToolMode::ReadOnly => &["read", "glob", "grep"],
        ToolMode::Coding => &["read", "glob", "grep", "edit", "write", "shell"],
    };
    ensure!(
        state.manifest.tool_manifest_digest
            == crate::storage::checkpoint::sha256(&serde_json::to_vec(&state.manifest.tools)?)
            && expected.iter().all(|t| names.contains(t))
            && names
                .iter()
                .all(|t| expected.contains(t) || *t == "context_read_artifact"),
        "M0 tool manifest differs from restored permissions"
    );
    use bitrouter_ai::core::protocol::ToolEffect;
    ensure!(
        state.manifest.tools.len() == names.len()
            && state.manifest.tools.iter().all(|t| match t.name.as_str() {
                "shell" => t.effect == ToolEffect::Shell && t.approval_required,
                "edit" | "write" => t.effect == ToolEffect::Write && t.approval_required,
                _ => t.effect == ToolEffect::Read && !t.approval_required,
            }),
        "unsupported tool permissions"
    );
    let run = state.run.as_ref().context("missing native run")?;
    let agent = state
        .agents
        .get(&state.agent_id)
        .context("missing root agent")?;
    let turn = agent.turn.as_ref().context("missing native turn")?;
    let accounting = run
        .token_accounting
        .as_ref()
        .context("missing native token accounting")?;
    ensure!(
        matches!(run.status, RunStatus::Completed)
            && state.agents.len() == 1
            && state.waits.is_empty()
            && state.activity_gaps.is_empty()
            && state.root_queue.pending.is_empty()
            && state.provider_evidence.is_empty()
            && state.responses.is_empty()
            && state.signals.revision == 0
            && state.signals.facts.is_empty()
            && state.signals.requests.is_empty()
            && state.signals.materials.is_empty()
            && state.signals.versions.is_empty()
            && run.input.acceptance_criteria.is_empty()
            && run.resource_error.is_none()
            && run.cancellation.is_none()
            && accounting
                .known_attempts
                .checked_add(accounting.unknown_attempts)
                == Some(run.model_attempts)
            && state
                .context_store
                .work
                .values()
                .all(|w| w.skills.is_none())
            && agent
                .context_sources
                .iter()
                .all(|src| src.materials.is_empty()
                    && src.permission_revision == state.manifest.permission_revision
                    && src.tool_manifest_digest == state.manifest.tool_manifest_digest)
            && run.activity_reconciliations.is_empty()
            && state.steering.is_empty()
            && agent.queue.is_empty()
            && agent.mailbox.is_empty()
            && state.recovery_archive.is_none()
            && turn.run_id == run.run_id
            && turn.status == bitrouter_ai::harness::inspection::AgentStatus::Completed
            && turn.core_calls.is_empty()
            && state.context_store.decisions.is_empty()
            && turn.invocations.iter().all(|i| i.consumed
                && i.result.as_ref().is_some_and(
                    |r| r.status != bitrouter_ai::core::protocol::ToolOutcome::EffectUnknown
                )),
        "unsettled or unsupported native execution"
    );
    ensure!(
        run.input.required_materials.is_empty()
            && run.input.verification.is_none()
            && run.input.discardable_history.is_none()
            && run.model_attempts as usize
                == turn.steps.iter().map(|s| s.attempts.len()).sum::<usize>()
            && turn.agent_turn_id == run.agent_turn_id
            && turn.final_answer == run.final_answer
            && state.allocations.is_empty()
            && state.context_store.derived.is_empty()
            && state.context_store.extracts.is_empty()
            && state.context_store.artifacts.is_empty(),
        "unsupported native delegation, context or accounting"
    );
    let mut history = vec![Message::text(Role::User, run.input.text.clone())];
    for step in &turn.steps {
        ensure!(
            serde_json::to_value(&step.input_history)? == serde_json::to_value(&history)?
                && step.manifest == state.manifest
                && step.attempts.len() == 1
                && step.preparation_work.iter().all(|w| w.report.is_some())
                && step.attempts[0]
                    .provider_work
                    .iter()
                    .all(|w| w.report.is_some()),
            "unpaired native context or provider work"
        );
        let receipt = step.attempts[0]
            .receipt
            .as_ref()
            .context("unsettled attempt")?;
        let result = receipt
            .report
            .result
            .as_ref()
            .context("missing native response")?;
        history.push(Message {
            role: Role::Assistant,
            content: result.content.clone(),
        });
        for inv in turn
            .invocations
            .iter()
            .filter(|i| i.dispatch.step_id == step.step_id)
        {
            let result = inv.result.as_ref().context("unconfirmed native tool")?;
            let output: ToolResultOutput =
                serde_json::from_str(&result.output).unwrap_or_else(|_| ToolResultOutput::Text {
                    value: result.output.clone(),
                });
            ensure!(
                result.invocation_id == inv.dispatch.invocation_id
                    && result.attempt_id == inv.dispatch.attempt_id
                    && result.evidence.is_empty()
                    && inv.dispatch.workspace_id == state.manifest.workspace_id
                    && inv.dispatch.permission_revision == state.manifest.permission_revision
                    && inv.dispatch.tool_manifest_digest == state.manifest.tool_manifest_digest
                    && expected.contains(&inv.dispatch.tool.as_str())
                    && inv.dispatch.agent_id == agent.agent_id
                    && inv.dispatch.agent_turn_id == turn.agent_turn_id
                    && inv.dispatch.run_id == run.run_id
                    && !inv.dispatch.verification
                    && inv.dispatch.response_id.is_none(),
                "unmatched native tool start"
            );
            history.push(Message {
                role: Role::Tool,
                content: vec![Content::ToolResult {
                    call_id: inv.provider_call_id.clone(),
                    tool_name: Some(inv.dispatch.tool.clone()),
                    output,
                    dynamic: false,
                    provider_metadata: Default::default(),
                }],
            });
        }
    }
    ensure!(
        serde_json::to_value(history)? == serde_json::to_value(&agent.history)?,
        "native context differs from confirmed outputs"
    );
    let metering = MeteringStore::new(db.clone());
    let mut ids = BTreeSet::new();
    let mut claims = BTreeMap::new();
    let mut known = 0u32;
    let mut subtotal = 0u64;
    for step in &turn.steps {
        ensure!(
            step.settled && !step.interrupted && !step.attempts.is_empty(),
            "unsettled native step"
        );
        for attempt in &step.attempts {
            let r = &attempt
                .receipt
                .as_ref()
                .context("unconfirmed native model attempt")?
                .report;
            ensure!(
                r.error.is_none()
                    && r.result.as_ref().is_some_and(|r| r.usage.is_some())
                    && ids.insert(r.request_id.clone())
                    && claimed_requests.insert(r.request_id.clone()),
                "unknown or duplicate native attempt"
            );
            use bitrouter_ai::core::evidence::{
                NativeContinuationInput, NativeContinuationOutput, PrivateContextEvidence,
            };
            ensure!(
                matches!(r.private_context.input, PrivateContextEvidence::NotPresent)
                    && matches!(r.private_context.output, PrivateContextEvidence::NotPresent)
                    && matches!(
                        r.continuation.input,
                        NativeContinuationInput::FullHistory { .. }
                    )
                    && !matches!(r.continuation.output, NativeContinuationOutput::Issued)
                    && r.route.protocol == bitrouter_ai::types::ApiProtocol::ChatCompletions,
                "unsupported private model continuity"
            );
            let observations = metering
                .read(&CallerContext::local(), std::slice::from_ref(&r.request_id))
                .await?;
            let [observation] = observations.as_slice() else {
                bail!("missing native settlement observation")
            };
            ensure!(
                observation.request_id == r.request_id
                    && observation.unknown_reason.is_none()
                    && observation
                        .claims
                        .iter()
                        .any(|c| c.basis == NativeCostBasis::Reconciled
                            && c.request_id == r.request_id
                            && Some(&c.model) == r.actual_model.as_ref()
                            && Some(&c.provider) == r.actual_provider.as_ref()),
                "unreconciled native model request {}",
                r.request_id
            );
            use sea_orm::EntityTrait;
            let row = crate::metering::entities::requests::Entity::find_by_id(&r.request_id)
                .one(db)
                .await?
                .context("missing native bill")?;
            let usage = r
                .result
                .as_ref()
                .and_then(|v| v.usage.as_ref())
                .context("missing native usage")?;
            ensure!(
                u64::try_from(row.prompt_tokens)? == usage.prompt_tokens
                    && u64::try_from(row.completion_tokens)? == usage.completion_tokens
                    && u64::try_from(row.reasoning_tokens)? == usage.reasoning_tokens
                    && u64::try_from(row.cache_read_tokens)? == usage.cache_read_tokens
                    && u64::try_from(row.cache_write_tokens)? == usage.cache_write_tokens,
                "native usage conflicts with the reconciled bill"
            );
            for claim in &observation.claims {
                let key = crate::storage::checkpoint::sha256(&serde_json::to_vec(&(
                    &claim.source,
                    &claim.bill_id,
                    claim.basis,
                ))?);
                ensure!(
                    claims.insert(key, claim.clone()).is_none(),
                    "duplicate native bill"
                );
            }
            if let Some(amount) = r.token_cost.estimated_micro_usd() {
                known += 1;
                subtotal = subtotal
                    .checked_add(amount)
                    .context("native token estimate overflow")?;
            }
        }
    }
    ensure!(
        accounting.known_attempts == known && accounting.known_subtotal_micro_usd == Some(subtotal),
        "native token estimates do not match attempt evidence"
    );
    let cost = state
        .cost_work
        .get(&run.run_id)
        .context("missing cost coverage")?;
    ensure!(
        state.cost_work.len() == 1
            && cost.charge_unknown.keys().all(|id| ids.contains(id))
            && cost
                .charges
                .iter()
                .all(|(key, c)| claims.get(key) == Some(c))
            && cost
                .work
                .values()
                .all(|w| w.state == CostWorkState::OutcomeRecorded
                    && match w.kind {
                        CostWorkKind::WorkspaceTool => true,
                        CostWorkKind::Preparation
                        | CostWorkKind::PreparationCallback
                        | CostWorkKind::ProviderAttempt
                        | CostWorkKind::Authentication
                        | CostWorkKind::HttpDispatch =>
                            w.request_id.as_ref().is_some_and(|id| ids.contains(id)),
                        _ => false,
                    }),
        "unreconciled auxiliary work"
    );
    Ok(())
}

pub(super) fn chat_history(messages: &[Message]) -> Result<Vec<Value>> {
    // Preserve only representations with an exact local Chat equivalent. In
    // particular, provider-executed calls and private continuation never degrade.
    crate::gateway::model::history::validate_history(messages).map_err(anyhow::Error::msg)?;
    let mut chat = Vec::new();
    for m in messages {
        let parts = serde_json::to_value(&m.content)?;
        let msg = match (m.role, m.content.as_slice()) {
            (Role::User | Role::Assistant, [Content::Text { text, .. }])
                if parts == json!([{"type":"text","text":text}]) =>
            {
                json!({"role":m.role,"content":text})
            }
            (
                Role::Assistant,
                [
                    Content::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    },
                ],
            ) if parts
                == json!([{"type":"tool_call","id":id,"name":name,"arguments":arguments}]) =>
            {
                json!({"role":"assistant","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":arguments}}]})
            }
            (
                Role::Tool,
                [
                    Content::ToolResult {
                        call_id,
                        tool_name: Some(name),
                        output: ToolResultOutput::Text { value },
                        ..
                    },
                ],
            ) if parts
                == json!([{"type":"tool_result","call_id":call_id,"tool_name":name,"output":{"type":"text","value":value}}]) =>
            {
                json!({"role":"tool","tool_call_id":call_id,"content":value})
            }
            _ => bail!("M0 history contains unsupported content or provider continuity"),
        };
        chat.push(msg);
    }
    Ok(chat)
}
