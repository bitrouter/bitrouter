use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde_json::json;

use super::EvidenceStore;
use crate::eval::types::canonical_digest;
use crate::session_evidence::types::{
    Artifact, Attempt, AttemptPhase, Coverage, EvidenceManifest, Harness, NodeKey, PARSER_VERSION,
    RecordInput, RegisteredSource, SCHEMA_VERSION, SourceCursor, SourceDescriptor, SourceFormat,
    SourceRange,
};

fn node(session: &str) -> NodeKey {
    NodeKey {
        namespace: "test-profile".into(),
        harness: Harness::ClaudeCode,
        native_id: session.into(),
        agent_id: None,
    }
}

fn source() -> SourceDescriptor {
    SourceDescriptor {
        namespace: "test-profile".into(),
        harness: Harness::ClaudeCode,
        format: SourceFormat::ClaudeTranscript,
        locator: "registered/session.jsonl".into(),
        node: Some(node("native-session")),
    }
}

fn record(sequence: u64, raw: serde_json::Value) -> RecordInput {
    RecordInput {
        generation: "generation-1".into(),
        sequence,
        byte_start: None,
        byte_end: None,
        producer_version: Some("2.1.220".into()),
        raw,
    }
}

fn cursor(next: u64) -> SourceCursor {
    SourceCursor {
        generation: "generation-1".into(),
        offset: next * 100,
        next_sequence: next,
        anchor_digest: format!("sha256:{}", "0".repeat(64)),
    }
}

fn range(source: &RegisteredSource) -> SourceRange {
    SourceRange {
        source_id: source.id.clone(),
        generation: source.cursor.generation.clone(),
        start: 0,
        end: source.cursor.next_sequence,
    }
}

async fn store() -> Result<EvidenceStore> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    EvidenceStore::new(db, "alice")
}

#[tokio::test]
async fn replay_is_idempotent_but_uuidless_occurrences_remain_distinct() -> Result<()> {
    let store = store().await?;
    let source = store.register(source()).await?;
    let records = vec![
        record(0, json!({"type":"mode","mode":"plan"})),
        record(1, json!({"type":"mode","mode":"plan"})),
    ];
    let source = store.append(&source, &records, cursor(2)).await?;
    let source = store.append(&source, &records, cursor(2)).await?;
    let saved = store.records(&range(&source)).await?;
    assert_eq!(saved.len(), 2);
    assert_ne!(saved[0].id, saved[1].id);
    assert_eq!(saved[0].input.raw, saved[1].input.raw);
    Ok(())
}

#[tokio::test]
async fn conflicting_replay_rolls_back_new_records_and_cursor() -> Result<()> {
    let store = store().await?;
    let source = store.register(source()).await?;
    let source = store
        .append(
            &source,
            &[record(0, json!({"type":"user","uuid":"u1"}))],
            cursor(1),
        )
        .await?;
    let error = store
        .append(
            &source,
            &[
                record(1, json!({"type":"assistant","uuid":"a1"})),
                record(0, json!({"type":"user","uuid":"changed"})),
            ],
            cursor(2),
        )
        .await;
    assert!(error.is_err());
    assert_eq!(store.source(&source.id).await?, Some(source.clone()));
    assert_eq!(
        store
            .records(&SourceRange {
                end: 2,
                ..range(&source)
            })
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn stale_collector_cannot_advance_another_collectors_cursor() -> Result<()> {
    let store = store().await?;
    let stale = store.register(source()).await?;
    let current = store
        .append(&stale, &[record(0, json!({"type":"user"}))], cursor(1))
        .await?;
    assert!(
        store
            .append(&stale, &[record(0, json!({"type":"assistant"}))], cursor(1))
            .await
            .is_err()
    );
    assert_eq!(store.source(&stale.id).await?, Some(current));
    Ok(())
}

#[tokio::test]
async fn generation_change_retains_the_old_file_and_rejects_sequence_gaps() -> Result<()> {
    let store = store().await?;
    let source = store.register(source()).await?;
    assert!(
        store
            .append(&source, &[record(1, json!({"type":"user"}))], cursor(2))
            .await
            .is_err()
    );
    let source = store
        .append(&source, &[record(0, json!({"type":"user"}))], cursor(1))
        .await?;
    let old_range = range(&source);
    let replacement = RecordInput {
        generation: "generation-2".into(),
        ..record(0, json!({"type":"user"}))
    };
    let source = store
        .append(
            &source,
            &[replacement],
            SourceCursor {
                generation: "generation-2".into(),
                ..cursor(1)
            },
        )
        .await?;
    let old = store.records(&old_range).await?;
    let new = store.records(&range(&source)).await?;
    assert_eq!(old.len(), 1);
    assert_eq!(new.len(), 1);
    assert_ne!(old[0].id, new[0].id);
    Ok(())
}

#[tokio::test]
async fn all_reads_and_cursor_writes_are_owner_scoped() -> Result<()> {
    let alice = store().await?;
    let bob = EvidenceStore::new(alice.db.clone(), "bob")?;
    let registered = alice.register(source()).await?;
    let registered = alice
        .append(
            &registered,
            &[record(0, json!({"type":"user","private":"alice"}))],
            cursor(1),
        )
        .await?;
    assert!(bob.source(&registered.id).await?.is_none());
    assert!(bob.records(&range(&registered)).await?.is_empty());
    assert!(bob.append(&registered, &[], cursor(1)).await.is_err());
    let other = bob.register(source()).await?;
    assert_ne!(registered.id, other.id);
    assert_eq!(bob.sources(None, 100).await?.len(), 1);
    Ok(())
}

fn attempt() -> Attempt {
    let root = node("native-session");
    Attempt {
        id: "attempt-1".into(),
        task_id: "task-1".into(),
        session: crate::session_evidence::types::AcpSessionKey {
            namespace: root.namespace.clone(),
            harness: root.harness,
            session_id: "adapter-session".into(),
        },
        members: BTreeSet::from([root]),
        execution_snapshot: None,
        phase: AttemptPhase::Collecting,
        revision: 0,
        latest_manifest: None,
        effective_manifest: None,
        started_at: "2026-09-05T00:00:00Z".into(),
    }
}

#[tokio::test]
async fn frozen_manifest_survives_new_execution_and_working_tree_changes() -> Result<()> {
    let store = store().await?;
    let registered = store.register(source()).await?;
    let registered = store
        .append(
            &registered,
            &[record(0, json!({"type":"user","uuid":"u1"}))],
            cursor(1),
        )
        .await?;
    let mut attempt = attempt();
    store.create_attempt(&attempt).await?;
    let records = store.records(&range(&registered)).await?;
    let manifest = EvidenceManifest {
        schema_version: SCHEMA_VERSION,
        parser_version: PARSER_VERSION.into(),
        attempt_id: attempt.id.clone(),
        revision: 0,
        members: attempt.members.clone(),
        ranges: vec![range(&registered)],
        record_digests: records.into_iter().map(|r| (r.id, r.digest)).collect(),
        edges: vec![],
        coverage: Coverage::default(),
        request_ids: BTreeSet::new(),
        decisions: vec![],
        decision_requests: BTreeMap::new(),
        artifacts: vec![Artifact {
            kind: "git_diff".into(),
            digest: canonical_digest(&"old patch")?,
            content: "old patch".into(),
            attributes: BTreeMap::new(),
        }],
        frozen_at: "2026-09-05T00:00:01Z".into(),
    };
    let frozen = store.freeze(&manifest).await?;
    assert_eq!(store.freeze(&manifest).await?, frozen);
    store
        .append(
            &registered,
            &[record(1, json!({"type":"assistant","uuid":"a1"}))],
            cursor(2),
        )
        .await?;
    attempt.revision = 1;
    attempt.phase = AttemptPhase::Collecting;
    attempt.latest_manifest = Some(frozen.clone());
    store.update_attempt(0, &attempt).await?;
    assert_eq!(store.manifest(&frozen).await?, Some(manifest.clone()));
    assert_eq!(store.freeze(&manifest).await?, frozen);
    let bob = EvidenceStore::new(store.db.clone(), "bob")?;
    assert!(bob.manifest(&frozen).await?.is_none());
    assert!(bob.attempt(&attempt.id).await?.is_none());
    let mut forged = manifest;
    forged.revision = 1;
    forged.record_digests.clear();
    assert!(store.freeze(&forged).await.is_err());
    Ok(())
}

#[tokio::test]
async fn restart_recovers_the_last_committed_cursor() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&database_url).await?;
    crate::db::run_migrations(&db).await?;
    let first = EvidenceStore::new(db, "alice")?;
    let source = first.register(source()).await?;
    let source = first
        .append(&source, &[record(0, json!({"type":"user"}))], cursor(1))
        .await?;
    first.db.close().await?;
    let second = EvidenceStore::new(crate::db::connect(&database_url).await?, "alice")?;
    let restored = second
        .source(&source.id)
        .await?
        .context("cursor missing after restart")?;
    assert_eq!(restored, source);
    assert_eq!(second.records(&range(&restored)).await?.len(), 1);
    Ok(())
}

#[test]
fn codex_tree_adoption_keeps_thread_identity_but_claude_children_are_distinct() -> Result<()> {
    let codex = NodeKey {
        harness: Harness::Codex,
        native_id: "thread-1".into(),
        ..node("tree-1")
    };
    let adopted = codex.clone();
    assert_eq!(BTreeSet::from([codex.clone(), adopted.clone()]).len(), 1);
    assert_eq!(codex.id()?, adopted.id()?);
    let child = NodeKey {
        agent_id: Some("child-1".into()),
        ..node("native-session")
    };
    assert_ne!(child.id()?, node("native-session").id()?);
    assert!(!Coverage::default().optimization_eligible());
    Ok(())
}

async fn checkpoint(
    store: &EvidenceStore,
    attempt: &Attempt,
    descriptor: SourceDescriptor,
) -> Result<EvidenceManifest> {
    let source = store.register(descriptor).await?;
    let source = store
        .append(
            &source,
            &[record(0, json!({"type":"user","uuid":"u1"}))],
            cursor(1),
        )
        .await?;
    let records = store.records(&range(&source)).await?;
    Ok(EvidenceManifest {
        schema_version: SCHEMA_VERSION,
        parser_version: PARSER_VERSION.into(),
        attempt_id: attempt.id.clone(),
        revision: attempt.revision,
        members: attempt.members.clone(),
        ranges: vec![range(&source)],
        record_digests: records
            .into_iter()
            .map(|record| (record.id, record.digest))
            .collect(),
        edges: vec![],
        coverage: Coverage::default(),
        request_ids: BTreeSet::new(),
        decisions: vec![],
        decision_requests: BTreeMap::new(),
        artifacts: vec![],
        frozen_at: "2026-09-05T00:00:01Z".into(),
    })
}

#[tokio::test]
async fn modified_cursor_with_correct_revision_cannot_skip_records() -> Result<()> {
    let store = store().await?;
    let source = store.register(source()).await?;
    let saved = store
        .append(&source, &[record(0, json!({"type":"user"}))], cursor(1))
        .await?;
    let mut forged = saved.clone();
    forged.cursor = cursor(4);
    assert!(
        store
            .append(
                &forged,
                &[record(4, json!({"type":"assistant"}))],
                cursor(5)
            )
            .await
            .is_err()
    );
    let mut skipped = saved.cursor.clone();
    skipped.offset += 50;
    assert!(store.append(&saved, &[], skipped).await.is_err());
    assert_eq!(store.source(&saved.id).await?, Some(saved));
    Ok(())
}

#[tokio::test]
async fn same_owner_unrelated_session_requires_bounded_fork_provenance() -> Result<()> {
    use crate::session_evidence::types::{EdgeKind, ExecutionEdge};
    let store = store().await?;
    let attempt = attempt();
    store.create_attempt(&attempt).await?;
    let mut foreign = source();
    foreign.node = Some(node("parent-session"));
    foreign.locator = "registered/parent.jsonl".into();
    let mut manifest = checkpoint(&store, &attempt, foreign).await?;
    assert!(store.freeze(&manifest).await.is_err());
    manifest.edges.push(ExecutionEdge {
        kind: EdgeKind::Fork,
        from: node("parent-session"),
        to: node("native-session"),
        checkpoint: Some(manifest.ranges[0].clone()),
        evidence_record_ids: manifest.record_digests.keys().cloned().collect(),
    });
    store.freeze(&manifest).await?;
    manifest.edges[0]
        .checkpoint
        .as_mut()
        .context("checkpoint")?
        .end = 2;
    assert!(store.freeze(&manifest).await.is_err());
    manifest.edges[0].checkpoint = Some(manifest.ranges[0].clone());
    manifest.edges[0].evidence_record_ids = BTreeSet::from([canonical_digest(&"missing-record")?]);
    assert!(store.freeze(&manifest).await.is_err());
    Ok(())
}

#[tokio::test]
async fn attempt_cannot_select_missing_foreign_stale_or_partial_manifest() -> Result<()> {
    let store = store().await?;
    let mut attempt = attempt();
    store.create_attempt(&attempt).await?;
    let manifest = checkpoint(&store, &attempt, source()).await?;
    let digest = store.freeze(&manifest).await?;
    attempt.revision = 1;
    attempt.latest_manifest = Some(canonical_digest(&"missing")?);
    assert!(store.update_attempt(0, &attempt).await.is_err());
    attempt.latest_manifest = Some(digest.clone());
    attempt.effective_manifest = Some(digest.clone());
    attempt.phase = AttemptPhase::Ready;
    assert!(store.update_attempt(0, &attempt).await.is_err());
    attempt.phase = AttemptPhase::Partial;
    attempt.effective_manifest = None;
    store.update_attempt(0, &attempt).await?;
    let mut other = attempt.clone();
    other.id = "different-attempt".into();
    other.revision = 0;
    other.latest_manifest = None;
    other.phase = AttemptPhase::Collecting;
    store.create_attempt(&other).await?;
    other.revision = 1;
    other.latest_manifest = Some(digest.clone());
    assert!(store.update_attempt(0, &other).await.is_err());
    let bob = EvidenceStore::new(store.db.clone(), "bob")?;
    other.revision = 0;
    other.latest_manifest = None;
    bob.create_attempt(&other).await?;
    other.revision = 1;
    other.latest_manifest = Some(digest);
    assert!(bob.update_attempt(0, &other).await.is_err());
    Ok(())
}

#[tokio::test]
async fn decision_binding_uses_gateway_request_id_not_routing_classification() -> Result<()> {
    let store = store().await?;
    let attempt = attempt();
    store.create_attempt(&attempt).await?;
    let mut manifest = checkpoint(&store, &attempt, source()).await?;
    manifest.request_ids.insert("request-42".into());
    manifest
        .decisions
        .push(crate::eval::types::EvalDecisionRef {
            decision_id: "decision-42".into(),
            policy: "coding".into(),
            route_projection: "coding".into(),
            request_key: "opening".into(),
            selected_tier: "standard".into(),
            selected_effort: None,
            baseline_tier: None,
            baseline_effort: None,
            policy_digest: canonical_digest(&"policy")?,
            experiment: None,
            route_measurement: None,
        });
    manifest
        .decision_requests
        .insert("decision-42".into(), "request-42".into());
    store.freeze(&manifest).await?;
    manifest
        .decision_requests
        .insert("decision-42".into(), "opening".into());
    assert!(store.freeze(&manifest).await.is_err());
    Ok(())
}

#[tokio::test]
async fn reads_detect_corrupted_record_and_object_payloads() -> Result<()> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};
    let store = store().await?;
    let attempt = attempt();
    store.create_attempt(&attempt).await?;
    let manifest = checkpoint(&store, &attempt, source()).await?;
    let digest = store.freeze(&manifest).await?;
    let id = manifest.record_digests.keys().next().context("record id")?;
    super::record_entity::Entity::update_many()
        .col_expr(
            super::record_entity::Column::RecordJson,
            Expr::value(serde_json::to_string(&record(
                0,
                json!({"type":"tampered"}),
            ))?),
        )
        .filter(super::record_entity::Column::Id.eq(id))
        .exec(&store.db)
        .await?;
    assert!(store.records(&manifest.ranges[0]).await.is_err());
    let mut changed = manifest.clone();
    changed.parser_version = "tampered".into();
    super::object_entity::Entity::update_many()
        .col_expr(
            super::object_entity::Column::ObjectJson,
            Expr::value(serde_json::to_string(&changed)?),
        )
        .filter(super::object_entity::Column::ObjectKey.eq(&digest))
        .exec(&store.db)
        .await?;
    assert!(store.manifest(&digest).await.is_err());
    Ok(())
}

#[tokio::test]
async fn case_distinct_owners_and_generations_use_distinct_database_keys() -> Result<()> {
    let alice = store().await?;
    let capitalized = EvidenceStore::new(alice.db.clone(), "Alice")?;
    assert_ne!(alice.owner_key, capitalized.owner_key);
    let first = alice.register(source()).await?;
    let first = alice
        .append(&first, &[record(0, json!({"type":"user"}))], cursor(1))
        .await?;
    assert!(capitalized.source(&first.id).await?.is_none());
    assert!(capitalized.sources(None, 10).await?.is_empty());
    let original = range(&first);
    let replacement = RecordInput {
        generation: "GENERATION-1".into(),
        ..record(0, json!({"type":"other"}))
    };
    let second = alice
        .append(
            &first,
            &[replacement],
            SourceCursor {
                generation: "GENERATION-1".into(),
                ..cursor(1)
            },
        )
        .await?;
    assert_ne!(
        alice.records(&original).await?[0].id,
        alice.records(&range(&second)).await?[0].id
    );
    Ok(())
}

#[tokio::test]
async fn complete_flags_cannot_make_empty_evidence_eligible() -> Result<()> {
    use crate::session_evidence::types::Completeness;
    let store = store().await?;
    let attempt = attempt();
    store.create_attempt(&attempt).await?;
    let mut manifest = checkpoint(&store, &attempt, source()).await?;
    manifest.ranges.clear();
    manifest.record_digests.clear();
    manifest.coverage = Coverage {
        history: Completeness::Complete,
        lineage: Completeness::Complete,
        requests: Completeness::Complete,
        artifacts: Completeness::Complete,
        gaps: BTreeSet::new(),
    };
    assert!(store.freeze(&manifest).await.is_err());
    Ok(())
}

#[tokio::test]
async fn source_pages_are_stable_and_record_reads_are_byte_bounded() -> Result<()> {
    let store = store().await?;
    for i in 0..4 {
        let mut descriptor = source();
        descriptor.locator = format!("registered/{i}.jsonl");
        store.register(descriptor).await?;
    }
    let first = store.sources(None, 2).await?;
    let second = store.sources(Some(&first[1].id), 2).await?;
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 2);
    assert!(
        first
            .iter()
            .all(|left| second.iter().all(|right| left.id != right.id))
    );
    assert!(
        store
            .records(&SourceRange {
                source_id: first[0].id.clone(),
                generation: "g".into(),
                start: 0,
                end: crate::session_evidence::types::RECORD_PAGE_SIZE + 1
            })
            .await
            .is_err()
    );
    Ok(())
}
