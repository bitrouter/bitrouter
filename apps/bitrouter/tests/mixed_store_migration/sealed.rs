use super::*;
use bitrouter::cloud::settlement::{SettlementReceipt, SettlementState, SettlementUsage};
use bitrouter::metering::store::MeteringStore;
use serde_json::{Value, json};
#[path = "accounting.rs"]
mod accounting;
#[path = "acp.rs"]
mod acp;
#[path = "capture.rs"]
mod capture;

const REQUESTS: [&str; 2] = [
    "2ad653eb-9a51-4999-b59f-3c4b1d65174c",
    "55caad5a-b17a-47cb-93c0-32d22e203796",
];

// Copy the captured execution unchanged. Its mock provider reported usage,
// but the capture did not include final bills. Reconcile those separately.
async fn captured(dir: &Path) -> Result<(PathBuf, DatabaseConnection)> {
    let (path, db) = fresh(dir).await?;
    capture::install_capture(dir, &db, CAPTURE, false).await?;
    Ok((path, db))
}

async fn settle(db: &DatabaseConnection, id: &str, amount: i64) -> Result<()> {
    db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO requests(request_id,user_id,api_key_id,model_id,provider_id,prompt_tokens,completion_tokens,reasoning_tokens,created_at,charge_status,reconciliation_status) VALUES(?,'local','local','test-model','mock',11,7,0,'now','unknown','pending')",[id.into()])).await?;
    MeteringStore::new(db.clone())
        .apply_authoritative_receipt_charge(&SettlementReceipt {
            request_id: id.into(),
            state: SettlementState::Computed,
            model_id: Some("test-model".into()),
            provider_id: Some("mock".into()),
            usage: SettlementUsage {
                uncached_input_tokens: 11,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 7,
                reasoning_tokens: 0,
            },
            final_charge_micro_usd: Some(amount),
        })
        .await?;
    Ok(())
}

#[tokio::test]
async fn transfers_sealed_read_once_only_after_both_zero_cost_receipts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (path, db) = captured(dir.path()).await?;
    settle(&db, REQUESTS[0], 0).await?;
    db.close().await?;
    let dest = dir.path().join("active");
    let report = mixed_store::activate(&path, &dest).await?;
    ensure!(
        !report.dispatch_allowed
            && !dest.exists()
            && report
                .blockers
                .iter()
                .any(|b| b.contains("missing native bill")),
        "{:?}",
        report.blockers
    );
    let db = connection(&path, false).await?;
    settle(&db, REQUESTS[1], 0).await?;
    db.close().await?;
    let report = mixed_store::activate(&path, &dest).await?;
    ensure!(report.dispatch_allowed, "{:?}", report.blockers);
    let origin: Value = serde_json::from_slice(&fs::read(dest.join("bro/sealed.json"))?)?;
    let live: Value =
        serde_json::from_slice(&fs::read(dest.join(format!("bro/threads/{THREAD}.json")))?)?;
    ensure!(
        origin[THREAD] == live
            && live["turns"][0]["sealed_messages"][2]["content"]
                .as_str()
                .is_some_and(|s| s.contains("baseline persisted payload"))
            && live["turns"][0]["request_id"] == "feba0903-39c1-4d18-a768-4c9fcaba4c55:start"
    );
    Ok(())
}

#[tokio::test]
async fn refuses_conflicting_receipt_amount_or_released_history() -> Result<()> {
    for paid in [false, true] {
        let dir = tempfile::tempdir()?;
        let (path, db) = captured(dir.path()).await?;
        settle(&db, REQUESTS[0], i64::from(paid)).await?;
        settle(&db, REQUESTS[1], 0).await?;
        if paid {
            db.execute_unprepared("UPDATE requests SET estimated_charge_micro_usd=7 WHERE estimated_charge_micro_usd=1").await?;
        } else {
            db.execute_unprepared("UPDATE bro_execution_records SET payload=replace(payload,'baseline persisted payload','different settled content') WHERE sequence=283").await?;
        }
        db.close().await?;
        let dest = dir.path().join("active");
        let report = mixed_store::activate(&path, &dest).await?;
        ensure!(!report.dispatch_allowed && !dest.exists());
    }
    Ok(())
}

#[tokio::test]
async fn transfers_only_the_sealed_result_of_the_approved_write() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (source, db) = fresh(dir.path()).await?;
    let thread = capture::install_approved(dir.path(), &db).await?;
    db.close().await?;
    let dest = dir.path().join("active");
    let result = mixed_store::activate(&source, &dest).await?;
    ensure!(result.dispatch_allowed, "{:?}", result.blockers);
    let origin: Value = serde_json::from_slice(&fs::read(dest.join("bro/sealed.json"))?)?;
    let live: Value =
        serde_json::from_slice(&fs::read(dest.join(format!("bro/threads/{thread}.json")))?)?;
    let turn = &live["turns"][0];
    ensure!(
        origin[thread.as_str()] == live
            && turn["steps"] == json!([])
            && turn["outcome"]["final_answer"] == "Wrote evidence.txt."
            && turn["sealed_tool_results"]
                == json!({"write-evidence":{
                    "effect":"completed", "output":{"type":"json","value":{"path":"evidence.txt","bytes":18}}
                }})
    );
    Ok(())
}

#[tokio::test]
async fn approved_mutation_requires_ordered_approval_call_result_and_effect() -> Result<()> {
    let cases = [
        (
            "$.entry.scope",
            "answer_approval",
            "$.entry.fingerprint",
            r#""wrong-fingerprint""#,
        ),
        (
            "$.fact.lifecycle.kind",
            "input_resolved",
            "$.fact.lifecycle.approved",
            "false",
        ),
        (
            "$.fact.lifecycle.kind",
            "input_requested",
            "$.fact.lifecycle.tool_id",
            r#""different-call""#,
        ),
        (
            "$.fact.record",
            "tool_result",
            "$.fact.effect",
            r#""not_executed""#,
        ),
        (
            "$.fact.record",
            "tool_result",
            "$.fact.message.content[0].output.value.bytes",
            "19",
        ),
    ];
    for (selector, tag, field, replacement) in cases {
        let dir = tempfile::tempdir()?;
        let (source, db) = fresh(dir.path()).await?;
        capture::install_approved(dir.path(), &db).await?;
        let changed=db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE bro_execution_records SET payload=json_set(payload,?,json(?)) WHERE instr(CASE WHEN json_valid(payload) THEN json_extract(payload,?) END,?)>0",
            [field.into(),replacement.into(),selector.into(),tag.into()])).await?;
        ensure!(changed.rows_affected() == 1);
        db.close().await?;
        let dest = dir.path().join("active");
        let result = mixed_store::activate(&source, &dest).await?;
        ensure!(
            !result.dispatch_allowed
                && !dest.exists()
                && result
                    .blockers
                    .iter()
                    .any(|b| b.contains("bro_execution_records")),
            "{tag}: {:?}",
            result.blockers
        );
    }
    Ok(())
}

// Construct the unscoped managed writer's release envelope around the recorded
// completed run. This fixture uses a harness without optional context views.
fn managed_journal() -> Result<Value> {
    managed_journal_with(|_| Ok(()))
}
fn managed_journal_with(edit: impl FnOnce(&mut Value) -> Result<()>) -> Result<Value> {
    use bitrouter::storage::checkpoint::{CheckpointBatchExt, sha256};
    use bitrouter_ai::core::checkpoint::{CheckpointAck, CheckpointBatch};
    use bitrouter_ai::core::protocol::Limits;
    let journal: Vec<Value> = serde_json::from_slice(&zstd::stream::decode_all(
        &include_bytes!("../fixtures/baseline-runtime-v7/journal.json.zst")[..],
    )?)?;
    let fact = &journal
        .iter()
        .rev()
        .find(|v| v["fact"]["record"] == "core_checkpoint")
        .context("missing captured batch")?["fact"];
    let batch: CheckpointBatch = serde_json::from_value(fact["batch"].clone())?;
    let limits: Limits = serde_json::from_value(fact["limits"].clone())?;
    let mut p = batch
        .decode(&limits)
        .map_err(|e| anyhow::anyhow!(e.message))?;
    let session = format!("session_{}", p.identity.session_id);
    let grant = json!({"session_id":session,"harness_id":"harness_00000000-0000-4000-b000-000000000010","execution_epoch":1,"core_instance_id":p.identity.core_instance_id});
    let workspace = "/original/managed";
    let mut manifest = p.checkpoint.state["manifest"].clone();
    manifest["workspace_id"] = json!(format!("workspace_{}", sha256(workspace.as_bytes())));
    manifest["required_features"] = json!([]);
    let base = p.checkpoint.state_revision;
    let rev = base + 1;
    let op = "release_one";
    let release = json!({"operation_id":op,"grant":grant,"state_revision":rev});
    let receipt = json!({"operation_id":op,"request_sha256":sha256(&serde_json::to_vec(&json!({"type":"session.release","expected_state_revision":base}))?),"disposition":"applied","assigned_ids":{"session_id":session,"core_instance_id":grant["core_instance_id"],"execution_epoch":"1"},"state_revision":rev,"error":null});
    p.base_event_seq = p.events.last().context("missing event")?.event_seq;
    p.base_state_revision = base;
    p.identity.session_id = session;
    p.identity.batch_id = "batch_managed_release".into();
    p.events = serde_json::from_value(
        json!([{"event_seq":p.base_event_seq+1,"type":"session.released","agent_id":null,"run_id":null,"payload":receipt}]),
    )?;
    p.checkpoint.state_revision = rev;
    let state = &mut p.checkpoint.state;
    state["session_id"] = json!(p.identity.session_id);
    state["manifest"] = manifest.clone();
    state["context_store"] = json!({"work":{},"evidence":{},"derived":{},"extracts":{},"artifacts":{},"decisions":{},"views":{},"executions":{}});
    state["operations"][op] = receipt;
    state["releases"][op] = release;
    state["root_queue"]["paused"] = json!(true);
    let root = state["agent_id"].as_str().context("no root")?.to_owned();
    let turn = &mut state["agents"][root]["turn"];
    for step in turn["steps"].as_array_mut().context("missing steps")? {
        step["manifest"] = manifest.clone();
    }
    let mut starts = json!({});
    for inv in turn["invocations"]
        .as_array_mut()
        .context("missing tools")?
    {
        inv["dispatch"]["workspace_id"] = manifest["workspace_id"].clone();
        let id = inv["dispatch"]["invocation_id"]
            .as_str()
            .context("missing invocation")?;
        starts[id] = json!({"command":inv["dispatch"],"started":true,"result":inv["result"]});
    }
    edit(state)?;
    let batch = CheckpointBatch::encode(&p, &limits).map_err(|e| anyhow::anyhow!(e.message))?;
    let ack = CheckpointAck {
        batch_id: batch.identity.batch_id.clone(),
        payload_sha256: batch.payload_sha256.clone(),
        through_event_seq: p.base_event_seq + 1,
        state_revision: rev,
        execution_epoch: 1,
    };
    Ok(
        json!({"format":1,"workspace":workspace,"mode":"read_only","grant":grant,"limits":limits,"manifest":manifest,"head":ack.head(),"checkpoint":batch,"ack":ack,"released":true,"starts":starts,"fences":[],"artifacts":{}}),
    )
}

#[tokio::test]
async fn released_managed_run_preserves_input_key_and_requires_confirmed_start() -> Result<()> {
    for uncertain in [true, false] {
        let dir = tempfile::tempdir()?;
        let (path, db) = fresh(dir.path()).await?;
        for id in REQUESTS {
            settle(&db, id, 0).await?;
        }
        let mut j = managed_journal()?;
        if uncertain {
            j["starts"]
                .as_object_mut()
                .context("starts")?
                .values_mut()
                .next()
                .context("tool")?["result"] = Value::Null;
        }
        let bytes = serde_json::to_string(&j)?;
        db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO bro_managed_sessions(id,revision,payload_bytes,payload) VALUES('captured-release',95,?,?)",[i64::try_from(bytes.len())?.into(),bytes.into()])).await?;
        db.close().await?;
        let dest = dir.path().join("active");
        let report = mixed_store::activate(&path, &dest).await?;
        ensure!(
            report.dispatch_allowed != uncertain && dest.exists() != uncertain,
            "{:?}",
            report.blockers
        );
        if !uncertain {
            let stored: Value = serde_json::from_slice(&fs::read(
                dest.join("bro/threads/c8367dd5-5e9d-4a46-a6b1-6dbf8e617d81.json"),
            )?)?;
            ensure!(
                stored["turns"][0]["request_id"] == "6b1c3c54-fc75-49e3-bfa8-9638247cd383"
                    && stored["turns"][0]["sealed_messages"][2]["role"] == "tool"
            );
        }
    }
    Ok(())
}
