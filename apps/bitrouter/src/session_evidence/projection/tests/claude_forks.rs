use super::*;

fn copied_rows(origin: &str) -> Vec<Value> {
    let mut rows = vec![
        json!({"type":"user","uuid":"u","parentUuid":null}),
        json!({"type":"assistant","uuid":"a","parentUuid":"u"}),
        json!({"type":"system","subtype":"compact_boundary","uuid":"b","parentUuid":null,
            "compactMetadata":{"preservedMessages":{"anchorUuid":"old-summary","uuids":["old-a"]}}}),
        json!({"type":"user","uuid":"summary","parentUuid":"b","isMeta":true}),
        json!({"type":"assistant","uuid":"after","parentUuid":"a"}),
    ];
    for row in &mut rows {
        row["forkedFrom"] = json!({"sessionId":origin,"messageUuid":format!("{origin}-{}", row["uuid"].as_str().unwrap_or_default())});
    }
    rows
}

fn project(rows: Vec<Value>) -> Result<(Projection, Vec<StoredRecord>)> {
    let records = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| record(i as u64, row))
        .collect::<Result<Vec<_>>>()?;
    let mut projector = projector(Harness::ClaudeCode)?;
    for row in &records {
        projector.push(row)?;
    }
    Ok((projector.finish(), records))
}

fn context(projection: &Projection) -> Vec<&str> {
    projection
        .effective_context
        .iter()
        .map(|reference| reference.record_id.as_str())
        .collect()
}

#[test]
fn inherited_compact_references_are_not_rebound_to_fork_aliases() -> Result<()> {
    // A nested fork names its immediate parent; the compact UUIDs can still
    // name a grandparent. Neither alias set authorizes rewriting context.
    for origin in ["old", "middle"] {
        let (projection, records) = project(copied_rows(origin))?;
        assert!(projection.gaps.is_empty(), "{:?}", projection.gaps);
        assert_eq!(
            context(&projection),
            [0, 1, 4].map(|i| records[i].id.as_str())
        );
        assert_eq!(projection.raw_record_ids.len(), records.len());
        assert_eq!(projection.claude_fork_origins.len(), records.len());
        assert_eq!(
            projection.claude_unapplied_compactions,
            BTreeMap::from([(records[2].id.clone(), BTreeSet::from(["old-a".into()]))])
        );
        assert!(
            projection
                .claude_fork_origins
                .values()
                .all(|copy| copy.source_session_id == origin)
        );
        assert_eq!(projection.transitions.len(), 1);
    }
    Ok(())
}

#[test]
fn a_new_compaction_after_an_sdk_fork_uses_its_local_message_ids() -> Result<()> {
    let mut rows = copied_rows("old");
    rows.extend([
        json!({"type":"system","subtype":"compact_boundary","uuid":"local-b","parentUuid":null,
            "compactMetadata":{"preservedMessages":{"anchorUuid":"local-summary","uuids":["after"]}}}),
        json!({"type":"user","uuid":"local-summary","parentUuid":"local-b","isMeta":true}),
        json!({"type":"assistant","uuid":"local-after","parentUuid":"local-summary"}),
    ]);
    let (projection, records) = project(rows)?;
    assert!(projection.gaps.is_empty(), "{:?}", projection.gaps);
    assert_eq!(
        context(&projection),
        [5, 6, 4, 7].map(|i| records[i].id.as_str())
    );
    assert_eq!(projection.claude_unapplied_compactions.len(), 1);
    assert_eq!(projection.claude_fork_origins.len(), 5);
    assert_eq!(projection.transitions.len(), 2);
    Ok(())
}

#[test]
fn missing_inherited_references_skip_the_whole_boundary() -> Result<()> {
    let mut rows = copied_rows("old");
    rows[2]["compactMetadata"]["preservedMessages"]["uuids"] = json!(["a", "old-a"]);
    let (projection, records) = project(rows)?;
    assert!(projection.gaps.is_empty());
    assert_eq!(
        context(&projection),
        [0, 1, 4].map(|i| records[i].id.as_str())
    );
    assert_eq!(
        projection.claude_unapplied_compactions[&records[2].id],
        BTreeSet::from(["old-a".into()])
    );

    let mut rows = copied_rows("old");
    rows[2]["compactMetadata"]["preservedMessages"] = json!({"anchorUuid":"summary","uuids":["a"]});
    let (projection, records) = project(rows)?;
    assert!(projection.gaps.is_empty());
    assert!(projection.claude_unapplied_compactions.is_empty());
    assert_eq!(
        context(&projection),
        [2, 3, 1, 4].map(|i| records[i].id.as_str())
    );
    Ok(())
}

#[test]
fn fork_claims_do_not_hide_invalid_compaction_or_origin_evidence() -> Result<()> {
    for origin in [
        json!({}),
        json!("parent"),
        json!({"sessionId":"s1","messageUuid":"old-b"}),
        json!({"sessionId":"old","messageUuid":"b"}),
        json!({"sessionId":"old","messageUuid":"x".repeat(513)}),
    ] {
        let mut rows = copied_rows("old");
        rows[2]["forkedFrom"] = origin;
        let (projection, _) = project(rows)?;
        assert!(projection.gaps.contains("claude_fork_origin_invalid"));
        assert!(projection.gaps.contains("preserved_context_unavailable"));
        assert!(projection.claude_unapplied_compactions.is_empty());
    }
    for metadata in [
        json!({"preservedMessages":{"anchorUuid":"old-summary","uuids":["old-a"]},"preserved_messages":{}}),
        json!({"preservedMessages":{"anchorUuid":"old-summary","uuids":["old-a","old-a"]}}),
        json!({"preservedMessages":{"anchorUuid":"old-summary","uuids":[]}}),
        json!({"preservedSegment":{"headUuid":"old-a","tailUuid":"old-a","anchorUuid":"old-summary"}}),
    ] {
        let mut rows = copied_rows("old");
        rows[2]["compactMetadata"] = metadata;
        let (projection, _) = project(rows)?;
        assert!(projection.gaps.contains("preserved_context_unavailable"));
        assert!(projection.claude_unapplied_compactions.is_empty());
    }
    let mut rows = copied_rows("old");
    rows[2]
        .as_object_mut()
        .context("boundary")?
        .remove("forkedFrom");
    let (projection, _) = project(rows)?;
    assert!(projection.gaps.contains("preserved_context_unavailable"));
    assert!(projection.claude_unapplied_compactions.is_empty());
    Ok(())
}

#[test]
fn conflicting_fork_aliases_and_replayed_origin_records_remain_inspectable() -> Result<()> {
    let mut rows = copied_rows("old");
    rows.push(rows[4].clone());
    let (projection, records) = project(rows.clone())?;
    assert!(projection.gaps.is_empty());
    assert_eq!(projection.claude_fork_origins.len(), records.len());
    assert_eq!(
        context(&projection),
        [0, 1, 5].map(|i| records[i].id.as_str())
    );
    rows[5]["uuid"] = json!("conflicting-copy");
    let (projection, records) = project(rows)?;
    assert!(projection.gaps.contains("claude_fork_alias_ambiguous"));
    assert_eq!(projection.raw_record_ids.len(), records.len());
    assert_eq!(projection.claude_fork_origins.len(), records.len());
    Ok(())
}

#[test]
fn old_projection_serialization_does_not_acquire_fork_claims() -> Result<()> {
    let (projection, _) = project(vec![json!({"type":"user","uuid":"u"})])?;
    let wire = serde_json::to_value(&projection)?;
    assert!(wire.get("claude_fork_origins").is_none());
    assert!(wire.get("claude_unapplied_compactions").is_none());
    assert_eq!(serde_json::from_value::<Projection>(wire)?, projection);
    Ok(())
}

#[test]
fn fork_origin_and_skipped_compaction_storage_respect_shared_byte_limits() -> Result<()> {
    let original = record(0, copied_rows("old").remove(0))?;
    let replay = record(1, original.input.raw.clone())?;
    let cost = replay.id.len() + "u".len() + "old".len() + "old-u".len() + 256;
    for overflow in [false, true] {
        let mut projector = projector(Harness::ClaudeCode)?;
        projector.push(&original)?;
        projector.retained_bytes =
            super::super::super::types::MAX_OBJECT_BYTES - cost + usize::from(overflow);
        let result = projector.push(&replay);
        assert_eq!(result.is_err(), overflow);
        assert_eq!(
            projector.projection.claude_fork_origins.len(),
            if overflow { 1 } else { 2 }
        );
        assert!(projector.retained_bytes <= super::super::super::types::MAX_OBJECT_BYTES);
        let projection = projector.finish();
        assert!(!projection.gaps.contains("claude_fork_origin_invalid"));
        assert_eq!(
            context(&projection),
            [if overflow {
                original.id.as_str()
            } else {
                replay.id.as_str()
            }]
        );
    }
    for overflow in [false, true] {
        let rows = copied_rows("old")
            .into_iter()
            .enumerate()
            .map(|(i, row)| record(i as u64, row))
            .collect::<Result<Vec<_>>>()?;
        let mut projector = projector(Harness::ClaudeCode)?;
        for row in &rows {
            projector.push(row)?;
        }
        let cost = rows[2].id.len() + "old-a".len() + 64;
        projector.retained_bytes =
            super::super::super::types::MAX_OBJECT_BYTES - cost + usize::from(overflow);
        let projection = projector.finish();
        assert_eq!(
            projection.gaps.contains("preserved_context_unavailable"),
            overflow
        );
        assert_eq!(
            projection.claude_unapplied_compactions.len(),
            usize::from(!overflow)
        );
        assert_eq!(context(&projection), [0, 1, 4].map(|i| rows[i].id.as_str()));
        assert_eq!(projection.claude_fork_origins.len(), rows.len());
    }
    Ok(())
}
