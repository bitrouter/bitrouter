use super::*;
use crate::eval::types::canonical_digest;
use crate::session_evidence::types::{Harness, MAX_OBJECT_BYTES, RecordInput};
use serde_json::json;

fn node() -> NodeKey {
    NodeKey {
        namespace: "profile".into(),
        harness: Harness::Codex,
        native_id: "child".into(),
        agent_id: None,
    }
}

fn row(kind: &str, payload: Value) -> Value {
    json!({"type":kind,"payload":payload})
}
fn event(kind: &str, turn: &str) -> Value {
    row("event_msg", json!({"type":kind,"turn_id":turn}))
}
fn context(turn: &str, root: &str) -> Value {
    row("turn_context", json!({"turn_id":turn,"root_turn_id":root}))
}

fn records(mut rows: Vec<Value>, base: u64) -> Result<Vec<StoredRecord>> {
    let source_id = canonical_digest(&"source")?;
    rows.iter_mut()
        .enumerate()
        .map(|(sequence, raw)| {
            if raw.get("ordinal").is_none() {
                raw["ordinal"] = json!(base + sequence as u64);
            }
            let input = RecordInput {
                generation: "fixture/1".into(),
                sequence: sequence as u64,
                byte_start: None,
                byte_end: None,
                producer_version: Some("0.153.4".into()),
                raw: raw.clone(),
            };
            Ok(StoredRecord {
                id: input.id(&source_id)?,
                source_id: source_id.clone(),
                digest: canonical_digest(&input)?,
                input,
            })
        })
        .collect()
}

fn scan(rows: Vec<Value>, base: u64, mut bytes: usize) -> Result<RolloutExecutions> {
    let mut scanner = Scanner::new(node());
    for record in records(rows, base)? {
        scanner.push(&record, &mut bytes);
    }
    Ok(scanner.finish())
}

#[test]
fn copied_parent_records_are_not_child_execution_and_followups_keep_new_root_turns() -> Result<()> {
    let evidence = scan(
        vec![
            row(
                "session_meta",
                json!({"id":"child","forked_from_id":"parent","subagent_history_start_ordinal":4}),
            ),
            row("session_meta", json!({"id":"parent"})),
            event("task_started", "parent-input"),
            context("parent-input", "parent-input"),
            event("task_started", "first"),
            context("first", "parent-input"),
            row(
                "response_item",
                json!({"type":"message","internal_chat_message_metadata_passthrough":{"turn_id":"first"}}),
            ),
            row(
                "compacted",
                json!({"replacement_history":[{"turn_id":"copied"}]}),
            ),
            context("first", "parent-input"),
            event("task_complete", "first"),
            event("task_started", "second"),
            context("second", "later-parent-input"),
            event("task_complete", "second"),
        ],
        0,
        MAX_OBJECT_BYTES,
    )?;
    assert!(evidence.gaps.is_empty());
    assert_eq!(evidence.runs.len(), 2);
    let first = &evidence.runs[0];
    assert_eq!(first.turn_id, "first");
    assert_eq!(first.root_turn_id.as_deref(), Some("parent-input"));
    assert_eq!(first.contexts.len(), 2);
    assert_eq!(
        first
            .records
            .iter()
            .map(|r| r.range.start)
            .collect::<Vec<_>>(),
        [4, 5, 6, 8, 9]
    );
    let span = first.observed_span.as_ref().context("own span")?;
    assert_eq!((span.start, span.end), (4, 10));
    assert_eq!(first.outcome, Some(RunOutcome::Completed));
    assert_eq!(evidence.unassigned[0].start, 7);
    assert_eq!(
        evidence.runs[1].root_turn_id.as_deref(),
        Some("later-parent-input")
    );
    assert_eq!(
        evidence.runs[1]
            .observed_span
            .as_ref()
            .context("second")?
            .start,
        10
    );
    Ok(())
}

#[test]
fn physical_revert_offset_does_not_apply_a_logical_parent_cut_to_local_records() -> Result<()> {
    let mut abort = event("turn_aborted", "snapshot-parent");
    abort["payload"]["reason"] = json!("interrupted");
    let evidence = scan(
        vec![
            row(
                "session_meta",
                json!({"id":"child","forked_from_id":"logical-parent","forked_from_ordinal_exclusive":90,
            "history_base":{"thread_id":"older-rollout","end_ordinal_exclusive":20}}),
            ),
            abort,
            event("task_started", "own"),
            context("own", "root"),
            event("task_complete", "own"),
        ],
        20,
        MAX_OBJECT_BYTES,
    )?;
    let own = evidence
        .runs
        .iter()
        .find(|run| run.turn_id == "own")
        .context("own run")?;
    assert_eq!(own.observed_span.as_ref().context("span")?.start, 2);
    let synthetic = evidence
        .runs
        .iter()
        .find(|run| run.turn_id == "snapshot-parent")
        .context("abort")?;
    assert!(synthetic.observed_span.is_none());
    assert!(synthetic.outcome.is_none());
    Ok(())
}

#[test]
fn unsupported_copied_forks_do_not_guess_own_boundaries() -> Result<()> {
    let evidence = scan(
        vec![
            row(
                "session_meta",
                json!({"id":"child","forked_from_id":"parent","forked_from_ordinal_exclusive":1}),
            ),
            event("task_started", "copied"),
            context("copied", "root"),
            event("task_complete", "copied"),
        ],
        0,
        MAX_OBJECT_BYTES,
    )?;
    assert!(evidence.runs.is_empty());
    assert!(
        evidence
            .gaps
            .contains("native_rollout_own_history_unavailable")
    );
    Ok(())
}

#[test]
fn gaps_and_conflicts_withhold_completed_execution_spans() -> Result<()> {
    let healthy = vec![
        row("session_meta", json!({"id":"child"})),
        event("task_started", "turn"),
        context("turn", "root"),
        event("task_complete", "turn"),
    ];
    for mutation in 0..6 {
        let mut rows = healthy.clone();
        match mutation {
            0 => rows[2]["ordinal"] = json!(9),
            1 => rows[2]["ordinal"] = Value::Null,
            2 => rows[2]["payload"]["thread_id"] = json!("foreign"),
            3 => rows.insert(3, context("turn", "other-root")),
            4 => rows.insert(3, event("task_started", "overlapping-open")),
            _ => rows.push(event("task_started", "turn")),
        }
        let evidence = scan(rows, 0, MAX_OBJECT_BYTES)?;
        let run = evidence
            .runs
            .iter()
            .find(|run| run.turn_id == "turn")
            .context("turn")?;
        assert!(run.observed_span.is_none(), "case {mutation}");
        assert!(run.outcome.is_none(), "case {mutation}");
        assert!(!run.gaps.is_empty());
        if mutation == 3 {
            assert!(run.root_turn_id.is_none());
        }
    }
    let evidence = scan(healthy, 0, 1)?;
    assert!(evidence.runs.is_empty());
    assert!(evidence.gaps.contains("native_rollout_execution_invalid"));
    Ok(())
}

#[test]
fn anonymous_aborts_and_unaddressed_messages_cannot_close_or_populate_a_turn() -> Result<()> {
    let evidence = scan(
        vec![
            row("session_meta", json!({"id":"child"})),
            event("task_started", "turn"),
            row(
                "response_item",
                json!({"type":"message","content":"unknown owner"}),
            ),
            row(
                "event_msg",
                json!({"type":"turn_aborted","reason":"interrupted"}),
            ),
        ],
        0,
        MAX_OBJECT_BYTES,
    )?;
    assert_eq!(evidence.runs[0].records.len(), 1);
    assert!(evidence.runs[0].outcome.is_none());
    assert_eq!(
        (evidence.unassigned[0].start, evidence.unassigned[0].end),
        (2, 4)
    );
    Ok(())
}

#[test]
fn a_different_source_or_missing_sequence_invalidates_the_inspected_prefix() -> Result<()> {
    for foreign in [true, false] {
        let mut rows = records(
            vec![
                row("session_meta", json!({"id":"child"})),
                event("task_started", "turn"),
                event("task_complete", "turn"),
            ],
            0,
        )?;
        if foreign {
            rows[2].source_id = canonical_digest(&"foreign")?;
        } else {
            rows[2].input.sequence = 3;
        }
        rows[2].id = rows[2].input.id(&rows[2].source_id)?;
        rows[2].digest = canonical_digest(&rows[2].input)?;
        let mut scanner = Scanner::new(node());
        let mut bytes = MAX_OBJECT_BYTES;
        for record in rows {
            scanner.push(&record, &mut bytes);
        }
        let evidence = scanner.finish();
        assert!(evidence.runs[0].outcome.is_none());
        assert!(evidence.gaps.contains("native_rollout_execution_invalid"));
    }
    Ok(())
}

#[test]
fn a_reopened_or_reversed_turn_keeps_its_ambiguous_tail_open_for_other_turns() -> Result<()> {
    for reversed in [true, false] {
        let mut rows = vec![row("session_meta", json!({"id":"child"}))];
        if !reversed {
            rows.push(event("task_started", "a"));
        }
        rows.extend([
            event("task_complete", "a"),
            event("task_started", "a"),
            event("task_started", "b"),
            event("task_complete", "b"),
        ]);
        let evidence = scan(rows, 0, MAX_OBJECT_BYTES)?;
        assert_eq!(evidence.runs.len(), 2);
        for run in &evidence.runs {
            assert!(run.observed_span.is_none());
            assert!(run.outcome.is_none());
            assert!(run.gaps.contains("native_rollout_execution_overlap"));
        }
    }
    Ok(())
}
