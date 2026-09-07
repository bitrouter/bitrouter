use super::*;
use serde_json::{Value, json};

use crate::session_evidence::journal::Journal;
use crate::session_evidence::types::{Harness, SourceFormat};

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
            "payload":{"threadId":"root","turn":{"id":format!("turn-{index}")}}}),
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
