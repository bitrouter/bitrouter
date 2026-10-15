//! Exact journal bytes written by the native driver, replayed with or without core.

use super::Saved;
use crate::harness::storage::ExecutionRecord;
use crate::storage::checkpoint::CheckpointBatchExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn facts() -> Result<Vec<ExecutionRecord>, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/native-replay/journal.json.zst"
    ))?;
    let json = zstd::stream::decode_all(bytes.as_slice())?;
    Ok(serde_json::from_slice(&json)?)
}

fn replay(facts: &[ExecutionRecord]) -> Result<Saved, String> {
    let mut saved = None;
    for fact in facts {
        Saved::replay(&mut saved, fact)?;
    }
    saved.ok_or("fixture has no native checkpoint".into())
}

#[test]
fn native_writer_journal_replays_without_loading_execution_resources() -> TestResult {
    let facts = facts()?;
    let saved = replay(&facts)?;
    assert!(saved.complete_projection());
    assert!(saved.bytes() > 0);
    let batch = saved
        .journal
        .checkpoint
        .as_ref()
        .ok_or("missing checkpoint")?;
    let state: crate::storage::checkpoint::snapshot::SessionSnapshot =
        serde_json::from_value(batch.decode(&saved.journal.limits)?.checkpoint.state)?;
    assert!(!state.context_store.work.is_empty());
    assert!(!state.context_store.evidence.is_empty());
    Ok(())
}

#[test]
fn corrupt_native_payload_never_becomes_a_valid_projection() -> TestResult {
    let mut facts = facts()?;
    let batch = facts
        .iter_mut()
        .find_map(|fact| match fact {
            ExecutionRecord::CoreCheckpoint { batch, .. } => Some(batch),
            _ => None,
        })
        .ok_or("missing checkpoint")?;
    batch.payload_sha256 = "0".repeat(64);
    assert!(replay(&facts).is_err());
    Ok(())
}

#[test]
fn earlier_immutable_fact_rewrite_is_rejected_even_with_valid_digests() -> TestResult {
    let mut facts = facts()?;
    let mut changed = false;
    for fact in &mut facts {
        if let ExecutionRecord::CoreCheckpoint { batch, limits } = fact {
            let mut payload = batch.decode(limits)?;
            if let Some(work) = payload.checkpoint.state["context_store"]["work"]
                .as_object_mut()
                .and_then(|work| work.values_mut().next())
            {
                work["text"] = serde_json::json!("rewritten prior evidence");
                *batch = bitrouter_ai::core::checkpoint::CheckpointBatch::encode(&payload, limits)?;
                changed = true;
                break;
            }
        }
    }
    assert!(changed);
    let error = replay(&facts)
        .err()
        .ok_or("immutable history rewrite was accepted")?;
    assert!(error.contains("immutable fact"), "{error}");
    Ok(())
}

#[test]
fn display_records_cannot_diverge_from_native_checkpoint_evidence() -> TestResult {
    let mut facts = facts()?;
    let prompt = facts
        .iter_mut()
        .find_map(|fact| match fact {
            ExecutionRecord::ModelRequest { prompt, .. } => Some(prompt),
            _ => None,
        })
        .ok_or("missing display projection")?;
    prompt.model = "unrelated/model".into();
    let error = replay(&facts)
        .err()
        .ok_or("altered projection was accepted")?;
    assert!(error.contains("display projection identity"), "{error}");
    Ok(())
}
