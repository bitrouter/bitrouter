use super::*;
use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::eval::types::canonical_digest;
use crate::session_evidence::execution::extract;
use crate::session_evidence::types::{RecordInput, SourceDescriptor, SourceFormat, StoredRecord};

fn rollout(node: &str, sequence: u64, payload: Value) -> Result<Vec<NativeFact>> {
    let mut facts = parsed(
        node,
        "rollout",
        SourceFormat::CodexRollout,
        0,
        json!({"type":"session_meta","payload":{"id":node}}),
    )?;
    facts.extend(parsed(
        node,
        "rollout",
        SourceFormat::CodexRollout,
        sequence + 1,
        json!({"type":"event_msg","payload":payload}),
    )?);
    Ok(facts)
}

fn parsed(
    node: &str,
    locator: &str,
    format: SourceFormat,
    sequence: u64,
    raw: Value,
) -> Result<Vec<NativeFact>> {
    let source = SourceDescriptor {
        namespace: "profile".into(),
        harness: Harness::Codex,
        format,
        locator: format!("{locator}:{node}"),
        node: Some(NodeKey {
            namespace: "profile".into(),
            harness: Harness::Codex,
            native_id: node.into(),
            agent_id: None,
        }),
    };
    let source_id = source.id(&canonical_digest(&"local")?)?;
    let input = RecordInput {
        generation: "fixture/1".into(),
        sequence,
        byte_start: None,
        byte_end: None,
        producer_version: Some("0.148.0".into()),
        raw,
    };
    extract(
        &source,
        &StoredRecord {
            id: input.id(&source_id)?,
            source_id,
            digest: canonical_digest(&input)?,
            input,
        },
    )
}

#[test]
fn child_resumption_keeps_distinct_turns_and_their_original_bookends() -> Result<()> {
    let mut facts = rollout("child", 0, json!({"type":"task_started","turn_id":"first"}))?;
    facts.extend(rollout(
        "child",
        1,
        json!({"type":"task_complete","turn_id":"first"}),
    )?);
    let (before, before_gaps) = summarize(&facts);
    assert!(before_gaps.is_empty());
    assert_eq!(before[0].outcome, Some(RunOutcome::Completed));
    let original = serde_json::to_value(&before[0])?;
    facts.extend(rollout(
        "child",
        2,
        json!({"type":"task_started","turn_id":"second"}),
    )?);
    // An old emitter's anonymous abort cannot close the most recent start.
    facts.extend(rollout(
        "child",
        3,
        json!({"type":"turn_aborted","reason":"interrupted"}),
    )?);
    let (after, gaps) = summarize(&facts);
    assert_eq!(after.len(), 2);
    assert_eq!(
        after
            .iter()
            .find(|run| run.turn_id == "second")
            .context("resumed turn")?
            .outcome,
        None
    );
    assert!(gaps.contains("native_run_abort_identity_unavailable"));
    assert_eq!(
        serde_json::to_value(
            after
                .iter()
                .find(|run| run.turn_id == "first")
                .context("first turn")?
        )?,
        original
    );
    facts.extend(rollout(
        "child",
        4,
        json!({"type":"turn_aborted","turn_id":"second","reason":"budget_limited"}),
    )?);
    let (ended, _) = summarize(&facts);
    let second = ended
        .iter()
        .find(|run| run.turn_id == "second")
        .context("second turn")?;
    assert_eq!(second.outcome, Some(RunOutcome::Interrupted));
    assert_eq!(
        second.terminations[0].abort_reason.as_deref(),
        Some("budget_limited")
    );
    assert_eq!(second.starts[0].record.range.start, 3);
    assert_eq!(second.terminations[0].record.range.start, 5);
    Ok(())
}

#[test]
fn matching_native_ids_join_sources_but_never_join_different_nodes() -> Result<()> {
    let mut facts = rollout("root", 0, json!({"type":"task_started","turn_id":"turn"}))?;
    facts.extend(rollout(
        "root",
        1,
        json!({"type":"turn_aborted","turn_id":"turn","reason":"replaced"}),
    )?);
    facts.extend(parsed(
        "root",
        "app-server",
        SourceFormat::CodexAppServer,
        0,
        json!({"direction":"server","phase":"notification","method":"turn/completed","payload":{
            "threadId":"root","turn":{"id":"turn","status":"interrupted"}
        }}),
    )?);
    facts.extend(rollout(
        "child",
        0,
        json!({"type":"task_started","turn_id":"turn"}),
    )?);
    let (runs, gaps) = summarize(&facts);
    assert!(gaps.is_empty());
    assert_eq!(runs.len(), 2);
    let root = runs
        .iter()
        .find(|run| run.node.native_id == "root")
        .context("root")?;
    assert_eq!(root.terminations.len(), 2);
    assert_eq!(root.outcome, Some(RunOutcome::Interrupted));
    assert_eq!(
        runs.iter()
            .find(|run| run.node.native_id == "child")
            .context("child")?
            .outcome,
        None
    );
    facts.reverse();
    assert_eq!(
        serde_json::to_value(summarize(&facts).0)?,
        serde_json::to_value(runs)?
    );
    Ok(())
}

#[test]
fn conflicting_outcomes_and_reversed_same_source_bookends_stay_unresolved() -> Result<()> {
    let mut facts = rollout("root", 0, json!({"type":"task_started","turn_id":"turn"}))?;
    facts.extend(rollout(
        "root",
        1,
        json!({"type":"task_complete","turn_id":"turn"}),
    )?);
    facts.extend(rollout(
        "root",
        2,
        json!({"type":"turn_aborted","turn_id":"turn","reason":"interrupted"}),
    )?);
    let (runs, gaps) = summarize(&facts);
    assert_eq!(runs[0].outcome, None);
    assert!(gaps.contains("native_run_terminal_conflict"));
    let mut reversed = rollout("root", 2, json!({"type":"task_started","turn_id":"turn"}))?;
    reversed.extend(rollout(
        "root",
        1,
        json!({"type":"task_complete","turn_id":"turn"}),
    )?);
    let (runs, gaps) = summarize(&reversed);
    assert_eq!(runs[0].outcome, None);
    assert!(gaps.contains("native_run_boundary_order_invalid"));
    let end = rollout("root", 0, json!({"type":"task_complete","turn_id":"turn"}))?;
    let (runs, gaps) = summarize(&end);
    assert_eq!(runs[0].outcome, Some(RunOutcome::Completed));
    assert!(gaps.contains("native_run_start_unavailable"));
    Ok(())
}

#[test]
fn completed_agent_calls_do_not_close_child_runs() -> Result<()> {
    let mut facts = rollout(
        "child",
        0,
        json!({"type":"task_started","turn_id":"child-turn"}),
    )?;
    for tool in [
        "spawnAgent",
        "sendInput",
        "resumeAgent",
        "closeAgent",
        "wait",
    ] {
        facts.extend(parsed(
            "root",
            "app-server",
            SourceFormat::CodexAppServer,
            0,
            json!({"direction":"server","phase":"notification","method":"item/completed","payload":{
                "threadId":"root","turnId":"parent-turn","item":{
                    "type":"collabAgentToolCall","id":tool,"tool":tool,"status":"completed",
                    "senderThreadId":"root","receiverThreadIds":["child"]
                }
            }}),
        )?);
    }
    let (runs, gaps) = summarize(&facts);
    assert!(gaps.is_empty());
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, None);
    assert!(runs[0].terminations.is_empty());
    Ok(())
}

#[test]
fn copied_fork_history_never_becomes_the_childs_executed_turn() -> Result<()> {
    let mut facts = parsed(
        "child",
        "copied",
        SourceFormat::CodexRollout,
        0,
        json!({"type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}),
    )?;
    for (sequence, kind) in [(1, "task_started"), (2, "task_complete")] {
        facts.extend(parsed(
            "child",
            "copied",
            SourceFormat::CodexRollout,
            sequence,
            json!({"type":"event_msg","payload":{"type":kind,"turn_id":"inherited"}}),
        )?);
    }
    for (sequence, method, status) in [
        (0, "turn/started", "inProgress"),
        (1, "turn/completed", "completed"),
    ] {
        facts.extend(parsed(
            "child",
            "app-server",
            SourceFormat::CodexAppServer,
            sequence,
            json!({"direction":"server","phase":"notification","method":method,"payload":{
                "threadId":"child","turn":{"id":"own-turn","status":status}
            }}),
        )?);
    }
    let (runs, gaps) = summarize(&facts);
    let inherited = runs
        .iter()
        .find(|run| run.turn_id == "inherited")
        .context("copied turn")?;
    assert_eq!(inherited.outcome, None);
    assert!(inherited.gaps.contains("native_run_execution_unverified"));
    assert!(gaps.contains("native_run_execution_unverified"));
    let own = runs
        .iter()
        .find(|run| run.turn_id == "own-turn")
        .context("own turn")?;
    assert_eq!(own.outcome, Some(RunOutcome::Completed));
    assert!(own.gaps.is_empty());
    // Even if an emitter reuses an inherited turn id, that old terminal cannot
    // finish a new directly observed child execution.
    facts.extend(parsed(
        "child",
        "app-server",
        SourceFormat::CodexAppServer,
        2,
        json!({"direction":"server","phase":"notification","method":"turn/started","payload":{
            "threadId":"child","turn":{"id":"inherited","status":"inProgress"}
        }}),
    )?);
    assert_eq!(
        summarize(&facts)
            .0
            .iter()
            .find(|run| run.turn_id == "inherited")
            .context("reused id")?
            .outcome,
        None
    );
    for (sequence, kind) in [(3, "task_started"), (4, "task_complete")] {
        facts.extend(parsed(
            "child",
            "copied",
            SourceFormat::CodexRollout,
            sequence,
            json!({"type":"event_msg","payload":{"type":kind,"turn_id":"inherited"}}),
        )?);
    }
    facts.extend(parsed(
        "child",
        "app-server",
        SourceFormat::CodexAppServer,
        3,
        json!({"direction":"server","phase":"notification","method":"turn/completed","payload":{
            "threadId":"child","turn":{"id":"inherited","status":"completed"}
        }}),
    )?);
    let (runs, gaps) = summarize(&facts);
    let observed = runs
        .iter()
        .find(|run| run.turn_id == "inherited")
        .context("directly ended turn")?;
    assert_eq!(observed.outcome, Some(RunOutcome::Completed));
    assert!(gaps.contains("native_run_execution_unverified"));
    assert!(!gaps.contains("native_run_boundary_order_invalid"));
    Ok(())
}

#[test]
fn referenced_fork_synthetic_aborts_and_missing_metadata_are_not_execution_proof() -> Result<()> {
    for metadata in [
        Some(
            json!({"type":"session_meta","payload":{"id":"child","history_base":{
                "parent_thread_id":"parent","end_ordinal_exclusive":12,"end_byte_offset":300
            }}}),
        ),
        None,
    ] {
        let mut facts = Vec::new();
        if let Some(metadata) = metadata {
            facts.extend(parsed(
                "child",
                "referenced",
                SourceFormat::CodexRollout,
                0,
                metadata,
            )?);
        }
        facts.extend(parsed("child", "referenced", SourceFormat::CodexRollout, 1,
            json!({"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"parent-turn","reason":"interrupted"}}))?);
        let (runs, gaps) = summarize(&facts);
        assert_eq!(runs[0].outcome, None);
        assert_eq!(runs[0].terminations[0].origin, RunOrigin::UnverifiedHistory);
        assert!(gaps.contains("native_run_execution_unverified"));
        assert!(gaps.contains("native_run_start_unavailable"));
    }
    Ok(())
}

#[test]
fn large_replayed_boundary_sets_stay_bounded_and_keep_source_order() -> Result<()> {
    let mut facts = Vec::new();
    for sequence in 0..5_000 {
        facts.extend(rollout(
            "root",
            sequence,
            json!({"type":"task_started","turn_id":"same"}),
        )?);
        facts.extend(rollout(
            "root",
            sequence + 5_000,
            json!({"type":"task_complete","turn_id":"same"}),
        )?);
    }
    // Duplicate observations do not duplicate the same original record.
    facts.extend(facts.clone());
    let (runs, gaps) = summarize(&facts);
    assert!(gaps.is_empty());
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].starts.len(), 5_000);
    assert_eq!(runs[0].terminations.len(), 5_000);
    assert_eq!(runs[0].outcome, Some(RunOutcome::Completed));
    let mut many_runs = Vec::new();
    for sequence in 0..=MAX_GRAPH_ITEMS {
        many_runs.extend(rollout(
            "root",
            sequence as u64,
            json!({"type":"task_started","turn_id":format!("turn-{sequence}")}),
        )?);
    }
    let (runs, gaps) = summarize(&many_runs);
    assert_eq!(runs.len(), MAX_GRAPH_ITEMS);
    assert!(gaps.contains("native_run_limit"));
    Ok(())
}
