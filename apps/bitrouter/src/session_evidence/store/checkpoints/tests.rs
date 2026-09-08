use super::*;
use crate::session_evidence::collector::{NativeCollector, NativeRoot};
use crate::session_evidence::journal::Journal;
use crate::session_evidence::types::{Harness, NodeKey, SourceFormat};
use serde_json::{Value, json};

fn session() -> AcpSessionKey {
    AcpSessionKey {
        namespace: "profile".into(),
        harness: Harness::ClaudeCode,
        session_id: "public".into(),
    }
}

async fn store() -> Result<EvidenceStore> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    EvidenceStore::new(db, "alice")
}

async fn journal(store: &EvidenceStore) -> Result<Journal> {
    Journal::new(
        store.clone(),
        SourceDescriptor {
            namespace: session().namespace,
            harness: session().harness,
            format: SourceFormat::Acp,
            locator: "controller:fixture".into(),
            node: None,
        },
        "fixture/1".into(),
    )
    .await
}

fn prompt(operation: &str, phase: &str) -> Value {
    json!({"method":"session/prompt","phase":phase,"operation_id":operation,
        "native_scope":"session","observed_at":"2026-09-08T00:00:00Z",
        "payload":if phase == "request" { json!({"sessionId":"public","prompt":[]}) }
            else { json!({"stopReason":"end_turn"}) }})
}

async fn checkpoint(store: &EvidenceStore, operation: &str, phase: &str) -> Result<String> {
    store
        .capture_checkpoint("fixture", operation, phase, session(), BTreeSet::new())
        .await
}

#[tokio::test]
async fn frozen_frontier_survives_append_replacement_reopen_and_file_removal() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite:{}?mode=rwc",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db, "alice")?;
    let collector = NativeCollector::new(
        store.clone(),
        NativeRoot {
            harness: session().harness,
            namespace: session().namespace,
            directory: directory.path().to_owned(),
        },
    )?;
    let node = NodeKey {
        namespace: session().namespace,
        harness: session().harness,
        native_id: "native".into(),
        agent_id: None,
    };
    let path = directory.path().join("native.jsonl");
    let first = json!({"type":"user","sessionId":"native","uuid":"one"});
    let second = json!({"type":"assistant","sessionId":"native","uuid":"two"});
    tokio::fs::write(&path, format!("{first}\n")).await?;
    let original = collector.reconcile(node.clone(), &path, None).await?;
    let id = checkpoint(&store, "one", "request").await?;
    let frozen = store.checkpoint_on(&store.db, &id).await?;
    assert_eq!(frozen.sources.len(), 1);
    assert_eq!(frozen.sources[0].source, original.source);
    assert_eq!(
        frozen.sources[0]
            .last_record
            .as_ref()
            .context("terminal")?
            .range
            .end,
        1
    );
    assert_eq!(checkpoint(&store, "one", "request").await?, id);
    tokio::fs::write(&path, format!("{first}\n{second}\n")).await?;
    let appended = collector.reconcile(node.clone(), &path, None).await?;
    assert_eq!(appended.source.cursor.next_sequence, 2);
    assert_eq!(store.checkpoint_on(&store.db, &id).await?, frozen);
    tokio::fs::write(&path, format!("{second}\n")).await?;
    let replacement = collector.reconcile(node, &path, None).await?;
    assert_ne!(
        replacement.source.cursor.generation,
        original.source.cursor.generation
    );
    tokio::fs::remove_file(path).await?;
    drop(collector);
    let db = store.db.clone();
    drop(store);
    db.close().await?;
    let reopened = EvidenceStore::new(crate::db::connect(&url).await?, "alice")?;
    assert_eq!(reopened.checkpoint_on(&reopened.db, &id).await?, frozen);
    let last = frozen.sources[0].last_record.as_ref().context("terminal")?;
    record_entity::Entity::delete_by_id(&last.record_id)
        .exec(&reopened.db)
        .await?;
    assert!(reopened.checkpoint_on(&reopened.db, &id).await.is_err());
    record_entity::Entity::delete_many()
        .filter(record_entity::Column::SourceId.eq(&replacement.source.id))
        .filter(
            record_entity::Column::Generation
                .eq(canonical_digest(&replacement.source.cursor.generation)?),
        )
        .exec(&reopened.db)
        .await?;
    let invalid = checkpoint(&reopened, "invalid", "response").await?;
    let partial = reopened.checkpoint_on(&reopened.db, &invalid).await?;
    assert!(partial.sources.is_empty());
    assert!(partial.gaps.contains("native_checkpoint_frontier_invalid"));
    Ok(())
}

#[tokio::test]
async fn source_inventory_overflow_cannot_be_presented_as_a_complete_cut() -> Result<()> {
    let store = store().await?;
    for index in 0..=MAX_GRAPH_ITEMS {
        store
            .register(SourceDescriptor {
                namespace: session().namespace,
                harness: session().harness,
                format: SourceFormat::ClaudeCli,
                locator: format!("process:{index}"),
                node: None,
            })
            .await?;
    }
    let id = checkpoint(&store, "bounded", "request").await?;
    let frozen = store.checkpoint_on(&store.db, &id).await?;
    assert_eq!(frozen.sources.len(), MAX_GRAPH_ITEMS);
    assert!(frozen.gaps.contains("native_checkpoint_inventory_limit"));
    Ok(())
}

#[tokio::test]
async fn source_inventory_crosses_pages_and_keeps_scope_and_corruption_explicit() -> Result<()> {
    let store = store().await?;
    let mut ids = BTreeSet::new();
    for index in 0..20 {
        let source = store
            .register(SourceDescriptor {
                namespace: session().namespace,
                harness: session().harness,
                format: SourceFormat::ClaudeCli,
                locator: format!("process:{index}"),
                node: None,
            })
            .await?;
        ids.insert(source.id);
    }
    for (owner, namespace, harness) in [
        ("bob", "profile", Harness::ClaudeCode),
        ("alice", "other", Harness::ClaudeCode),
        ("alice", "profile", Harness::Codex),
    ] {
        EvidenceStore::new(store.db.clone(), owner)?
            .register(SourceDescriptor {
                namespace: namespace.into(),
                harness,
                format: SourceFormat::Acp,
                locator: "controller:foreign".into(),
                node: None,
            })
            .await?;
    }
    let first = ids.pop_first().context("first source")?;
    source_entity::Entity::update_many()
        .col_expr(source_entity::Column::DescriptorJson, Expr::value("{}"))
        .filter(source_entity::Column::Id.eq(first))
        .exec(&store.db)
        .await?;
    let id = checkpoint(&store, "one", "request").await?;
    let frozen = store.checkpoint_on(&store.db, &id).await?;
    assert_eq!(
        frozen
            .sources
            .iter()
            .map(|source| source.source.id.clone())
            .collect::<BTreeSet<_>>(),
        ids
    );
    assert_eq!(
        frozen.gaps,
        BTreeSet::from([
            "native_checkpoint_source_invalid".into(),
            "native_checkpoint_source_uninitialized".into(),
        ])
    );
    let bob = EvidenceStore::new(store.db.clone(), "bob")?;
    assert!(bob.checkpoint_on(&bob.db, &id).await.is_err());
    let row = store
        .object(&store.db, "native_checkpoint", &id)
        .await?
        .context("checkpoint row")?;
    object_entity::Entity::update_many()
        .col_expr(object_entity::Column::ObjectJson, Expr::value("{}"))
        .filter(object_entity::Column::Id.eq(row.id))
        .exec(&store.db)
        .await?;
    assert!(store.checkpoint_on(&store.db, &id).await.is_err());
    Ok(())
}

#[tokio::test]
async fn task_selects_original_baseline_and_only_the_latest_response_candidate() -> Result<()> {
    let store = store().await?;
    let journal = journal(&store).await?;
    let mut baseline = None;
    let mut result = None;
    for operation in ["one", "two"] {
        for phase in ["request", "response"] {
            let id = checkpoint(&store, operation, phase).await?;
            let mut event = prompt(operation, phase);
            event["native_checkpoint"] = json!(id);
            journal.append(event).await?;
            if operation == "one" && phase == "request" {
                baseline = Some(id.clone());
            }
            if phase == "response" {
                result = Some(id);
            }
            let evidence = store.native_checkpoint_evidence(&session()).await?;
            assert_eq!(evidence.baseline, baseline);
            assert_eq!(evidence.latest_prompt_result, result);
        }
    }
    let attempt = store.active_attempt(&session()).await?.context("attempt")?;
    assert_eq!(attempt.phase, AttemptPhase::Settling);
    assert!(attempt.members.is_empty());
    assert!(attempt.effective_manifest.is_none());
    // A replay can be observed at a newer source position without replacing
    // either original operation boundary or the latest distinct response.
    for phase in ["request", "response"] {
        let id = checkpoint(&store, "one", phase).await?;
        let mut replay = prompt("one", phase);
        replay["native_checkpoint"] = json!(id);
        journal.append(replay).await?;
    }
    let replayed = store.native_checkpoint_evidence(&session()).await?;
    assert_eq!(replayed.baseline, baseline);
    assert_eq!(replayed.latest_prompt_result, result);
    let id = result.context("latest result")?;
    let row = store
        .object(&store.db, "native_checkpoint", &id)
        .await?
        .context("checkpoint")?;
    object_entity::Entity::delete_by_id(row.id)
        .exec(&store.db)
        .await?;
    assert!(store.active_attempt(&session()).await?.is_some());
    assert!(store.native_checkpoint_evidence(&session()).await.is_err());
    Ok(())
}

#[tokio::test]
async fn checkpoint_admission_rejects_other_boundaries_without_committing_the_prompt() -> Result<()>
{
    for case in [
        "controller",
        "operation",
        "phase",
        "session",
        "owner",
        "scope",
        "response",
    ] {
        let store = store().await?;
        let journal = journal(&store).await?;
        let mut root = session();
        if case == "session" {
            root.session_id = "other".into();
        }
        let owner = if case == "owner" {
            EvidenceStore::new(store.db.clone(), "bob")?
        } else {
            store.clone()
        };
        let id = owner
            .capture_checkpoint(
                if case == "controller" {
                    "other"
                } else {
                    "fixture"
                },
                if case == "operation" { "other" } else { "one" },
                if case == "phase" {
                    "response"
                } else {
                    "request"
                },
                root,
                BTreeSet::new(),
            )
            .await?;
        let mut event = prompt(
            "one",
            if case == "response" {
                "response"
            } else {
                "request"
            },
        );
        if case == "scope" {
            event["native_scope"] = json!("controller");
        }
        event["native_checkpoint"] = json!(id);
        assert!(journal.append(event).await.is_err(), "{case}");
        assert!(store.attempts(None, 16).await?.is_empty(), "{case}");
        assert_eq!(
            store.sources(None, 16).await?[0].cursor,
            SourceCursor::default(),
            "{case}"
        );
        let id = checkpoint(&store, "one", "request").await?;
        let mut valid = prompt("one", "request");
        valid["native_checkpoint"] = json!(id);
        journal.append(valid).await?;
        let id = checkpoint(&store, "other", "response").await?;
        let mut response = prompt("one", "response");
        response["native_checkpoint"] = json!(id);
        assert!(journal.append(response).await.is_err());
        assert_eq!(store.sources(None, 16).await?[0].cursor.next_sequence, 1);
        assert_eq!(
            store
                .active_attempt(&session())
                .await?
                .context("attempt")?
                .phase,
            AttemptPhase::Collecting
        );
    }
    Ok(())
}

#[tokio::test]
async fn historical_prompts_without_checkpoints_remain_visible_with_missing_evidence() -> Result<()>
{
    let store = store().await?;
    let journal = journal(&store).await?;
    journal.append(prompt("one", "request")).await?;
    journal.append(prompt("one", "response")).await?;
    let evidence = store.native_checkpoint_evidence(&session()).await?;
    assert!(evidence.baseline.is_none());
    assert!(evidence.latest_prompt_result.is_none());
    assert_eq!(
        evidence.gaps,
        BTreeSet::from([
            "native_checkpoint_baseline_unavailable".into(),
            "native_checkpoint_result_unavailable".into(),
        ])
    );
    assert!(store.active_attempt(&session()).await?.is_some());
    Ok(())
}
