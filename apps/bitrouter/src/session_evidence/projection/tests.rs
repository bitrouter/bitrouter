use super::*;
use crate::session_evidence::types::RecordInput;
use serde_json::json;

fn projector(harness: Harness) -> Result<Projector> {
    Projector::new(
        NodeKey {
            namespace: "local".into(),
            harness,
            native_id: "s1".into(),
            agent_id: None,
        },
        if harness == Harness::Codex {
            SourceFormat::CodexRollout
        } else {
            SourceFormat::ClaudeTranscript
        },
    )
}

fn record(sequence: u64, raw: Value) -> Result<StoredRecord> {
    let mut raw = raw;
    if raw.get("type").and_then(Value::as_str) != Some("session_meta") {
        raw.as_object_mut()
            .context("object")?
            .entry("sessionId")
            .or_insert(json!("s1"));
    }
    let input = RecordInput {
        generation: "g1".into(),
        sequence,
        byte_start: None,
        byte_end: None,
        producer_version: Some("fixture/1".into()),
        raw,
    };
    let source_id = canonical_digest(&"source")?;
    Ok(StoredRecord {
        id: input.id(&source_id)?,
        source_id,
        digest: canonical_digest(&input)?,
        input,
    })
}

#[test]
fn codex_detects_native_ordinal_holes_in_roots_and_paginated_leaves() -> Result<()> {
    for base in [None, Some(20)] {
        let mut projector = projector(Harness::Codex)?;
        let mut meta = json!({"id":"s1","cli_version":"0.153.0-alpha.5"});
        if let Some(base) = base {
            meta["history_base"] = json!({"end_ordinal_exclusive":base});
        }
        let start = base.unwrap_or(0);
        projector.push(&record(
            0,
            json!({"ordinal":start,"type":"session_meta","payload":meta}),
        )?)?;
        projector.push(&record(
            1,
            json!({"ordinal":start + 2,"type":"response_item","payload":{}}),
        )?)?;
        assert!(projector.finish().gaps.contains("native_ordinal_gap"));
    }
    Ok(())
}

#[test]
fn identical_claude_uuid_replay_updates_leaf_position_without_erasing_raw() -> Result<()> {
    let user = record(
        0,
        json!({"type":"user","uuid":"u","parentUuid":null,"message":{"content":"task"}}),
    )?;
    let first = record(
        1,
        json!({"type":"assistant","uuid":"a","parentUuid":"u","message":{"content":"a"}}),
    )?;
    let other = record(
        2,
        json!({"type":"assistant","uuid":"b","parentUuid":"u","message":{"content":"b"}}),
    )?;
    let replay = record(3, first.input.raw.clone())?;
    let mut projector = projector(Harness::ClaudeCode)?;
    for record in [&user, &first, &other, &replay] {
        projector.push(record)?;
    }
    let projection = projector.finish();
    assert!(projection.gaps.is_empty());
    assert_eq!(projection.raw_record_ids.len(), 4);
    assert_eq!(
        projection
            .effective_context
            .iter()
            .map(|reference| &reference.record_id)
            .collect::<Vec<_>>(),
        [&user.id, &replay.id]
    );
    Ok(())
}

#[test]
fn consecutive_claude_compactions_relink_preserved_messages_without_erasing_audit() -> Result<()> {
    let rows = [
        json!({"type":"user","uuid":"u0","parentUuid":null,"message":{"content":"original"}}),
        json!({"type":"assistant","uuid":"a0","parentUuid":"u0","message":{"content":"keep me"}}),
        json!({"type":"system","subtype":"compact_boundary","uuid":"b1","parentUuid":null,
            "compactMetadata":{"preservedSegment":{"headUuid":"a0","tailUuid":"a0","anchorUuid":"s1"}}}),
        json!({"type":"user","uuid":"s1","parentUuid":"b1","message":{"content":"summary 1"}}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"a0","message":{"content":"later result"}}),
        json!({"type":"system","subtype":"compact_boundary","uuid":"b2","parentUuid":null,
            "compactMetadata":{"preservedMessages":{"anchorUuid":"s2","uuids":["a0","a1"]}}}),
        json!({"type":"user","uuid":"s2","parentUuid":"b2","message":{"content":"summary 2"}}),
    ];
    let mut projector = projector(Harness::ClaudeCode)?;
    let records = rows
        .into_iter()
        .enumerate()
        .map(|(i, raw)| record(i as u64, raw))
        .collect::<Result<Vec<_>>>()?;
    for record in &records {
        projector.push(record)?;
    }
    let result = projector.finish();
    assert_eq!(result.raw_record_ids.len(), 7);
    assert_eq!(result.transitions.len(), 2);
    assert_eq!(
        result
            .effective_context
            .iter()
            .map(|item| item.record_id.as_str())
            .collect::<Vec<_>>(),
        [5, 6, 1, 4].map(|index| records[index].id.as_str())
    );
    assert!(result.gaps.is_empty());
    Ok(())
}

#[test]
fn codex_replacement_and_rollback_change_context_but_keep_execution() -> Result<()> {
    let rows = [
        json!({"type":"session_meta","payload":{"id":"s1","cli_version":"0.148.0"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"t0"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[]}}),
        json!({"type":"compacted","payload":{"replacement_history":[{"type":"message","role":"user"}]}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
        json!({"type":"response_item","payload":{"type":"function_call","call_id":"call1"}}),
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
    ];
    let mut projector = projector(Harness::Codex)?;
    for (i, row) in rows.into_iter().enumerate() {
        projector.push(&record(i as u64, row)?)?;
    }
    let result = projector.finish();
    assert_eq!(result.raw_record_ids.len(), 7);
    assert_eq!(result.effective_context.len(), 1);
    assert_eq!(
        result.effective_context[0].pointer,
        "/payload/replacement_history/0"
    );
    assert_eq!(
        result
            .transitions
            .iter()
            .map(|transition| transition.kind)
            .collect::<Vec<_>>(),
        [EdgeKind::Compact, EdgeKind::Rewind]
    );
    Ok(())
}

#[test]
fn missing_parents_unknown_records_and_conflicting_uuids_remain_visible() -> Result<()> {
    let mut codex = projector(Harness::Codex)?;
    codex.push(&record(
        0,
        json!({"type":"session_meta","payload":{"id":"s1","history_base":{"thread_id":"parent"}}}),
    )?)?;
    codex.push(&record(1, json!({"type":"future_record","payload":{}}))?)?;
    let result = codex.finish();
    assert!(result.gaps.contains("inherited_history_missing"));
    assert!(result.gaps.contains("unknown_rollout_record"));
    let mut claude = projector(Harness::ClaudeCode)?;
    claude.push(&record(
        0,
        json!({"type":"assistant","uuid":"a1","parentUuid":"missing","message":{"content":"one"}}),
    )?)?;
    claude.push(&record(
        1,
        json!({"type":"assistant","uuid":"a1","parentUuid":"missing","message":{"content":"two"}}),
    )?)?;
    let result = claude.finish();
    assert_eq!(result.raw_record_ids.len(), 2);
    assert!(result.gaps.contains("conflicting_message_uuid"));
    assert!(result.gaps.contains("context_parent_missing"));
    Ok(())
}

#[test]
fn parent_and_child_project_independently_and_sources_cannot_be_concatenated() -> Result<()> {
    let mut parent = projector(Harness::ClaudeCode)?;
    parent.push(&record(
        0,
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s1"}),
    )?)?;
    let child_node = NodeKey {
        agent_id: Some("child-1".into()),
        ..parent.projection.node.clone()
    };
    let mut child = Projector::new(child_node, SourceFormat::ClaudeTranscript)?;
    child.push(&record(
        0,
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s1","agentId":"child-1"}),
    )?)?;
    child.push(&record(
        1,
        json!({"type":"system","subtype":"compact_boundary","uuid":"b1","compactMetadata":{}}),
    )?)?;
    assert_eq!(parent.finish().transitions.len(), 0);
    assert_eq!(child.finish().transitions.len(), 1);
    let mut projector = projector(Harness::Codex)?;
    projector.push(&record(0, json!({"type":"turn_context","payload":{}}))?)?;
    let mut unrelated = record(1, json!({"type":"turn_context","payload":{}}))?;
    unrelated.source_id = canonical_digest(&"another source")?;
    assert!(projector.push(&unrelated).is_err());
    Ok(())
}

#[test]
fn claude_restores_sibling_blocks_and_tool_results_and_ignores_meta_leaf() -> Result<()> {
    let rows = [
        json!({"type":"user","uuid":"u0","parentUuid":null}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u0","message":{"id":"message-1","content":[{"type":"text","text":"part 1"}]}}),
        json!({"type":"assistant","uuid":"a2","parentUuid":"u0","message":{"id":"message-1","content":[{"type":"tool_use","id":"tool-1"}]}}),
        json!({"type":"user","uuid":"r2","parentUuid":"a2","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1"}]}}),
        json!({"type":"assistant","uuid":"final","parentUuid":"a1","message":{"id":"final-message"}}),
        json!({"type":"system","uuid":"meta","parentUuid":null,"isMeta":true}),
    ];
    let mut projector = projector(Harness::ClaudeCode)?;
    let records = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| record(i as u64, row))
        .collect::<Result<Vec<_>>>()?;
    for record in &records {
        projector.push(record)?;
    }
    let result = projector.finish();
    assert_eq!(
        result
            .effective_context
            .iter()
            .map(|entry| entry.record_id.as_str())
            .collect::<Vec<_>>(),
        [0, 1, 2, 3, 4].map(|i| records[i].id.as_str())
    );
    assert!(result.gaps.is_empty());
    Ok(())
}

#[test]
fn native_compaction_redirects_later_children_of_the_summary_anchor() -> Result<()> {
    let rows = [
        json!({"type":"user","uuid":"old","parentUuid":null}),
        json!({"type":"assistant","uuid":"keep","parentUuid":"old"}),
        json!({"type":"system","subtype":"compact_boundary","uuid":"boundary","parentUuid":null,
            "compactMetadata":{"preservedMessages":{"anchorUuid":"summary","uuids":["keep"]}}}),
        json!({"type":"user","uuid":"summary","parentUuid":"boundary"}),
        json!({"type":"assistant","uuid":"after","parentUuid":"summary"}),
    ];
    let mut projector = projector(Harness::ClaudeCode)?;
    let records = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| record(i as u64, row))
        .collect::<Result<Vec<_>>>()?;
    for record in &records {
        projector.push(record)?;
    }
    let result = projector.finish();
    assert_eq!(
        result
            .effective_context
            .iter()
            .map(|entry| entry.record_id.as_str())
            .collect::<Vec<_>>(),
        [2, 3, 1, 4].map(|i| records[i].id.as_str())
    );
    Ok(())
}

#[test]
fn context_expansion_and_native_identifiers_are_bounded() -> Result<()> {
    let mut codex = projector(Harness::Codex)?;
    let excessive = json!({"type":"compacted","payload":{"replacement_history":vec![Value::Null; MAX_RECORDS + 1]}});
    assert!(codex.push(&record(0, excessive)?).is_err());
    let mut claude = projector(Harness::ClaudeCode)?;
    assert!(
        claude
            .push(&record(0, json!({"type":"user","uuid":"x".repeat(513)}))?)
            .is_err()
    );
    Ok(())
}
