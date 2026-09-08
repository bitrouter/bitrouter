use super::*;
use serde_json::{Value, json};

use crate::session_evidence::journal::Journal;
use crate::session_evidence::types::{Harness, RecordRef, SourceFormat};

fn node(id: &str) -> NodeKey {
    NodeKey {
        namespace: "native-profile".into(),
        harness: Harness::Codex,
        native_id: id.into(),
        agent_id: None,
    }
}

fn descriptor() -> SourceDescriptor {
    SourceDescriptor {
        namespace: "native-profile".into(),
        harness: Harness::Codex,
        format: SourceFormat::CodexAppServer,
        locator: "controller-test".into(),
        node: None,
    }
}

async fn store() -> Result<EvidenceStore> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    EvidenceStore::new(db, "alice")
}

async fn append(
    store: &EvidenceStore,
    source: SourceDescriptor,
    event: Value,
) -> Result<RegisteredSource> {
    Journal::new(store.clone(), source.clone(), "0.148.0".into())
        .await?
        .append(event)
        .await?;
    store.register(source).await
}

fn thread(id: &str, parent: Option<&str>, group: &str) -> Value {
    json!({"direction":"server","phase":"notification","method":"thread/started",
        "payload":{"thread":{"id":id,"parentThreadId":parent,"sessionId":group}}})
}

fn range(source: &RegisteredSource) -> SourceRange {
    SourceRange {
        source_id: source.id.clone(),
        generation: source.cursor.generation.clone(),
        start: 0,
        end: source.cursor.next_sequence,
    }
}

#[tokio::test]
async fn facts_survive_restart_and_replay_without_crossing_owner_or_namespace() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite:{}?mode=rwc",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db.clone(), "alice")?;
    let source = append(
        &store,
        descriptor(),
        thread("child", Some("root"), "same-group"),
    )
    .await?;
    let before = store
        .execution_graph(&BTreeSet::from([node("root")]))
        .await?;
    assert_eq!(before.nodes, BTreeSet::from([node("root"), node("child")]));
    store.index_execution_range(&range(&source)).await?;
    let replay = store
        .execution_graph(&BTreeSet::from([node("root")]))
        .await?;
    assert_eq!(before.facts, replay.facts);
    assert!(
        EvidenceStore::new(db.clone(), "Alice")?
            .node_facts(&node("root"), None, 100)
            .await?
            .is_empty()
    );
    let mut other_namespace = node("root");
    other_namespace.namespace = "Native-profile".into();
    assert!(
        store
            .node_facts(&other_namespace, None, 100)
            .await?
            .is_empty()
    );
    drop(store);
    db.close().await?;
    let reopened = EvidenceStore::new(crate::db::connect(&url).await?, "alice")?;
    assert_eq!(
        before.facts,
        reopened
            .execution_graph(&BTreeSet::from([node("root")]))
            .await?
            .facts
    );
    Ok(())
}

#[tokio::test]
async fn indexed_pagination_preserves_all_facts_and_grouping_does_not_adopt_nodes() -> Result<()> {
    let store = store().await?;
    for id in ["root", "unrelated"] {
        append(&store, descriptor(), thread(id, None, "shared-group")).await?;
    }
    for index in 0..140 {
        append(
            &store,
            descriptor(),
            json!({"direction":"server","phase":"notification","method":"turn/started",
            "payload":{"threadId":"root","turn":{"id":format!("turn-{index}"),"status":"inProgress"}}}),
        )
        .await?;
    }
    let mut after = None;
    let mut ids = BTreeSet::new();
    loop {
        let page = store.node_facts(&node("root"), after.as_deref(), 7).await?;
        if page.is_empty() {
            break;
        }
        for fact in &page {
            assert!(ids.insert(fact.id.clone()));
        }
        after = page.last().map(|fact| fact.id.clone());
    }
    assert_eq!(ids.len(), 142);
    let graph = store
        .execution_graph(&BTreeSet::from([node("root")]))
        .await?;
    assert_eq!(graph.nodes, BTreeSet::from([node("root")]));
    assert_eq!(graph.facts.len(), ids.len());
    assert_eq!(graph.codex_runs.len(), 140);
    assert!(
        graph
            .codex_runs
            .iter()
            .all(|run| run.outcome.is_none() && run.starts.len() == 1)
    );
    assert!(graph.gaps.is_empty());
    Ok(())
}

#[tokio::test]
async fn spawn_conflicts_and_cycles_keep_provenance_but_mark_graph_incomplete() -> Result<()> {
    let store = store().await?;
    append(&store, descriptor(), thread("child", Some("root"), "group")).await?;
    let mut other_source = descriptor();
    other_source.locator = "another-controller".into();
    append(
        &store,
        other_source,
        thread("child", Some("other"), "group"),
    )
    .await?;
    let conflict = store
        .execution_graph(&BTreeSet::from([node("root")]))
        .await?;
    assert!(conflict.gaps.contains("conflicting_native_parent"));
    assert_eq!(
        conflict
            .facts
            .iter()
            .filter(|fact| matches!(
                fact.event,
                FactKind::Relation {
                    relation: EdgeKind::Spawn
                }
            ))
            .count(),
        2
    );
    append(&store, descriptor(), thread("root", Some("child"), "group")).await?;
    let cycle = store
        .execution_graph(&BTreeSet::from([node("root")]))
        .await?;
    assert!(cycle.gaps.contains("native_spawn_cycle"));
    assert!(cycle.gaps.contains("conflicting_native_parent"));
    Ok(())
}

#[tokio::test]
async fn corrupt_index_columns_cannot_silently_disappear_during_replay() -> Result<()> {
    for column in [
        fact_entity::Column::Namespace,
        fact_entity::Column::ParserVersion,
        fact_entity::Column::RecordId,
        fact_entity::Column::NodeId,
        fact_entity::Column::RelatedNodeId,
    ] {
        let store = store().await?;
        let source = append(&store, descriptor(), thread("child", Some("root"), "group")).await?;
        fact_entity::Entity::update_many()
            .col_expr(column, Expr::value(canonical_digest(&"corrupt-index")?))
            .exec(&store.db)
            .await?;
        assert!(store.index_execution_range(&range(&source)).await.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn facts_are_rederived_from_raw_evidence_even_if_forged_json_has_a_valid_digest() -> Result<()>
{
    let store = store().await?;
    append(&store, descriptor(), thread("child", Some("root"), "group")).await?;
    let mut fact = store
        .node_facts(&node("root"), None, 100)
        .await?
        .into_iter()
        .find(|fact| matches!(fact.event, FactKind::Relation { .. }))
        .context("spawn fact")?;
    let old_key = canonical_digest(&(&store.owner_key, &fact.id))?;
    fact.event = FactKind::RunFinished {
        run_id: "invented".into(),
        status: "completed".into(),
    };
    fact_entity::Entity::update_many()
        .col_expr(
            fact_entity::Column::FactJson,
            Expr::value(serde_json::to_string(&fact)?),
        )
        .col_expr(
            fact_entity::Column::Digest,
            Expr::value(canonical_digest(&fact)?),
        )
        .filter(fact_entity::Column::Id.eq(old_key))
        .exec(&store.db)
        .await?;
    assert!(store.node_facts(&node("root"), None, 100).await.is_err());
    Ok(())
}

#[tokio::test]
async fn malformed_lifecycle_is_durable_uncertainty_and_does_not_block_raw_cursor() -> Result<()> {
    let store = store().await?;
    let source = append(
        &store,
        descriptor(),
        json!({"direction":"server","phase":"notification","method":"turn/completed",
        "payload":{"threadId":"root","turn":{"status":"completed"}}}),
    )
    .await?;
    assert_eq!(source.cursor.next_sequence, 1);
    assert_eq!(store.records(&range(&source)).await?.len(), 1);
    assert!(
        store
            .execution_graph(&BTreeSet::from([node("root")]))
            .await?
            .gaps
            .contains("native_lifecycle_invalid")
    );
    Ok(())
}

#[tokio::test]
async fn v3_run_bookends_recover_from_raw_without_rewriting_v2_facts() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite:{}?mode=rwc",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db.clone(), "alice")?;
    append(
        &store,
        descriptor(),
        json!({
            "direction":"server","phase":"notification","method":"turn/started",
            "payload":{"threadId":"root","turn":{"id":"turn","status":"inProgress"}}
        }),
    )
    .await?;
    let mut old = store
        .node_facts(&node("root"), None, 100)
        .await?
        .into_iter()
        .next()
        .context("started fact")?;
    old.parser_version = "native-evidence/2".into();
    old.record = None;
    old.source_format = None;
    old.id = canonical_digest(&(
        &old.parser_version,
        &old.record_id,
        &old.node,
        &old.related_node,
        &old.event,
    ))?;
    let old_json = serde_json::to_string(&old)?;
    let old_id = canonical_digest(&(&store.owner_key, &old.id))?;
    fact_entity::Entity::delete_many().exec(&db).await?;
    fact_entity::Entity::insert(fact_entity::ActiveModel {
        id: Set(old_id.clone()),
        owner: Set(store.owner_key.clone()),
        namespace: Set(canonical_digest(&(Harness::Codex, "native-profile"))?),
        parser_version: Set(canonical_digest(&old.parser_version)?),
        record_id: Set(old.record_id.clone()),
        digest: Set(canonical_digest(&old)?),
        node_id: Set(Some(node("root").id()?)),
        related_node_id: Set(None),
        fact_json: Set(old_json.clone()),
    })
    .exec(&db)
    .await?;
    let source = append(
        &store,
        descriptor(),
        json!({
            "direction":"server","phase":"notification","method":"turn/completed",
            "payload":{"threadId":"root","turn":{"id":"turn","status":"interrupted"}}
        }),
    )
    .await?;
    // Close with only v2 indexes present, then derive v3 for the first time
    // from the two durable raw records after reopening the database.
    fact_entity::Entity::delete_many()
        .filter(fact_entity::Column::ParserVersion.eq(canonical_digest(&PARSER_VERSION)?))
        .exec(&db)
        .await?;
    assert_eq!(fact_entity::Entity::find().all(&db).await?.len(), 1);
    drop(store);
    drop(db);
    let db = crate::db::connect(&url).await?;
    let reopened = EvidenceStore::new(db.clone(), "alice")?;
    reopened.index_execution_range(&range(&source)).await?;
    let graph = reopened
        .execution_graph(&BTreeSet::from([node("root")]))
        .await?;
    assert!(graph.gaps.is_empty());
    assert_eq!(graph.codex_runs.len(), 1);
    let run = &graph.codex_runs[0];
    assert_eq!(
        run.outcome,
        Some(super::super::super::execution::runs::RunOutcome::Interrupted)
    );
    assert_eq!(run.starts[0].record.range.start, 0);
    assert_eq!(run.terminations[0].record.range.start, 1);
    for reference in [&run.starts[0].record, &run.terminations[0].record] {
        let records = reopened.records(&reference.range).await?;
        assert_eq!(
            RecordRef::from_record(records.first().context("boundary raw record")?)?,
            *reference
        );
    }
    assert_eq!(
        fact_entity::Entity::find_by_id(old_id)
            .one(&db)
            .await?
            .context("legacy v2 fact")?
            .fact_json,
        old_json
    );
    // A terminal index is insufficient if its original evidence disappears.
    record_entity::Entity::delete_by_id(&run.terminations[0].record.record_id)
        .exec(&db)
        .await?;
    let damaged = reopened
        .execution_graph(&BTreeSet::from([node("root")]))
        .await?;
    assert!(damaged.gaps.contains("native_graph_evidence_invalid"));
    assert!(damaged.codex_runs.iter().all(|run| run.outcome.is_none()));
    Ok(())
}

#[tokio::test]
async fn copied_fact_filter_requires_the_original_owner_cut_and_exact_ordinal() -> Result<()> {
    for case in 0..5 {
        let store = store().await?;
        let source = SourceDescriptor {
            format: SourceFormat::CodexRollout,
            node: Some(node("root")),
            ..descriptor()
        };
        let mut first = json!({"ordinal":0,"type":"session_meta","payload":{"id":"root","subagent_history_start_ordinal":2}});
        let mut copied = json!({"ordinal":1,"type":"session_meta","payload":{"id":"parent"}});
        match case {
            1 => first["payload"]["subagent_history_start_ordinal"] = json!(1),
            2 => copied["ordinal"] = json!(0),
            3 => first["payload"]["id"] = json!("foreign-owner"),
            4 => {
                first["ordinal"] = json!(4);
                first["payload"]["subagent_history_start_ordinal"] = json!(8);
                copied["ordinal"] = json!(5);
            }
            _ => {}
        }
        append(&store, source.clone(), first).await?;
        append(&store, source, copied).await?;
        let graph = store
            .execution_graph(&BTreeSet::from([node("root")]))
            .await?;
        assert_eq!(
            graph.gaps.contains("native_lifecycle_invalid"),
            case != 0,
            "case {case}"
        );
        assert_eq!(graph.inherited_records.len(), usize::from(case == 0));
        assert!(store.node_facts(&node("root"),None,100).await?.iter().any(|fact|
            matches!(&fact.event,FactKind::Gap {reason} if reason=="native_lifecycle_invalid")));
    }
    Ok(())
}

#[tokio::test]
async fn adding_process_identity_preserves_legacy_fact_bytes_and_digests() -> Result<()> {
    let store = store().await?;
    append(&store, descriptor(), thread("root", None, "group")).await?;
    for fact in store.node_facts(&node("root"), None, 100).await? {
        assert!(fact.process_id.is_none());
        let serialized = serde_json::to_value(&fact)?;
        assert!(serialized.get("process_id").is_none());
        assert_eq!(serde_json::from_value::<NativeFact>(serialized)?, fact);
        assert_eq!(
            fact.id,
            canonical_digest(&(
                PARSER_VERSION,
                &fact.record_id,
                &fact.node,
                &fact.related_node,
                &fact.event
            ))?
        );
    }
    Ok(())
}

#[tokio::test]
async fn current_parser_rebuilds_reset_identity_after_reopen_without_overwriting_v1() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite:{}?mode=rwc",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db.clone(), "alice")?;
    let descriptor = SourceDescriptor {
        namespace: "native-profile".into(),
        harness: Harness::ClaudeCode,
        format: SourceFormat::Acp,
        locator: "controller:fixture".into(),
        node: None,
    };
    let source = append(&store, descriptor, json!({"method":"_claude/sdkMessage","phase":"notification","native_scope":"session","payload":{"sessionId":"acp-root","message":{"type":"system","subtype":"session_state_changed","session_id":"fresh-native","uuid":"native-event","state":"idle"}}})).await?;
    let record = store
        .records(&range(&source))
        .await?
        .into_iter()
        .next()
        .context("original SDK record")?;
    fact_entity::Entity::delete_many().exec(&db).await?;
    // Parser v1 rejected a valid post-reset native id that differed from ACP.
    let event = FactKind::Gap {
        reason: "native_lifecycle_invalid".into(),
    };
    let old = NativeFact {
        id: canonical_digest(&(
            "native-evidence/1",
            &record.id,
            Option::<NodeKey>::None,
            Option::<NodeKey>::None,
            &event,
        ))?,
        parser_version: "native-evidence/1".into(),
        record_id: record.id.clone(),
        record_digest: record.digest,
        source_id: source.id.clone(),
        record: None,
        source_format: None,
        process_id: None,
        acp_session_id: None,
        node: None,
        related_node: None,
        event,
    };
    let old_json = serde_json::to_string(&old)?;
    let old_id = canonical_digest(&(&store.owner_key, &old.id))?;
    fact_entity::Entity::insert(fact_entity::ActiveModel {
        id: Set(old_id.clone()),
        owner: Set(store.owner_key.clone()),
        namespace: Set(canonical_digest(&(Harness::ClaudeCode, "native-profile"))?),
        parser_version: Set(canonical_digest(&"native-evidence/1")?),
        record_id: Set(record.id),
        digest: Set(canonical_digest(&old)?),
        node_id: Set(None),
        related_node_id: Set(None),
        fact_json: Set(old_json.clone()),
    })
    .exec(&db)
    .await?;
    drop(store);
    drop(db);
    let db = crate::db::connect(&url).await?;
    let reopened = EvidenceStore::new(db.clone(), "alice")?;
    reopened.index_execution_range(&range(&source)).await?;
    let target = NodeKey {
        namespace: "native-profile".into(),
        harness: Harness::ClaudeCode,
        native_id: "fresh-native".into(),
        agent_id: None,
    };
    let facts = reopened.node_facts(&target, None, 100).await?;
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].parser_version, PARSER_VERSION);
    assert_eq!(facts[0].acp_session_id.as_deref(), Some("acp-root"));
    assert_eq!(facts[0].node.as_ref(), Some(&target));
    assert_eq!(
        fact_entity::Entity::find_by_id(old_id)
            .one(&db)
            .await?
            .context("retained v1 fact")?
            .fact_json,
        old_json
    );
    Ok(())
}

#[tokio::test]
async fn candidate_graph_keeps_healthy_nodes_while_strict_reads_reject_a_bad_record() -> Result<()>
{
    let store = store().await?;
    let broken = append(&store, descriptor(), thread("broken", None, "group")).await?;
    let mut healthy = descriptor();
    healthy.locator = "healthy-source".into();
    append(&store, healthy, thread("healthy", None, "group")).await?;
    record_entity::Entity::update_many()
        .col_expr(record_entity::Column::Digest, Expr::value("damaged"))
        .filter(record_entity::Column::SourceId.eq(broken.id))
        .exec(&store.db)
        .await?;
    assert!(store.node_facts(&node("broken"), None, 100).await.is_err());
    let graph = store
        .execution_graph(&BTreeSet::from([node("broken"), node("healthy")]))
        .await?;
    assert!(graph.gaps.contains("native_graph_evidence_invalid"));
    assert!(
        graph
            .facts
            .iter()
            .any(|fact| fact.node.as_ref() == Some(&node("healthy")))
    );
    assert!(
        !graph
            .facts
            .iter()
            .any(|fact| fact.node.as_ref() == Some(&node("broken")))
    );
    Ok(())
}
