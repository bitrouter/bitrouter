use super::super::collector::NativeRoot;
use super::*;
use serde_json::{Value, json};
use std::path::Path;

mod own_runs;

fn row(ordinal: u64, kind: &str, payload: Value) -> Result<String> {
    Ok(format!(
        "{}\n",
        serde_json::to_string(&json!({"ordinal":ordinal,"type":kind,"payload":payload}))?
    ))
}

fn node(id: &str) -> NodeKey {
    NodeKey {
        namespace: "fixture".into(),
        harness: Harness::Codex,
        native_id: id.into(),
        agent_id: None,
    }
}

fn binding_key(child: &NodeKey, parent: &NodeKey, ordinal: u64, bytes: u64) -> Result<String> {
    ForkBinding::history_key(
        child,
        parent,
        Some(&RolloutPair {
            child: child.native_id.clone(),
            parent: parent.native_id.clone(),
        }),
        ordinal,
        bytes,
    )
}

async fn resolver(path: &Path) -> Result<HistoryResolver> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db, "local")?;
    let collector = NativeCollector::new(
        store.clone(),
        NativeRoot {
            harness: Harness::Codex,
            namespace: "fixture".into(),
            directory: path.into(),
        },
    )?;
    Ok(HistoryResolver::new(store, collector))
}

#[tokio::test]
async fn nested_bound_forks_survive_database_reopen_without_parent_files() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&database_url).await?;
    crate::db::run_migrations(&db).await?;
    let make_resolver = |db| -> Result<HistoryResolver> {
        let store = EvidenceStore::new(db, "local")?;
        let collector = NativeCollector::new(
            store.clone(),
            NativeRoot {
                harness: Harness::Codex,
                namespace: "fixture".into(),
                directory: directory.path().into(),
            },
        )?;
        Ok(HistoryResolver::new(store, collector))
    };
    let first = make_resolver(db.clone())?;
    let mut previous = None;
    for (index, id) in ["root", "middle", "leaf"].into_iter().enumerate() {
        let ordinal = (index * 2) as u64;
        let mut metadata = json!({"id":id,"cli_version":"0.153.0-alpha.5"});
        if let Some((parent, bytes)) = previous {
            metadata["history_base"] =
                json!({"thread_id":parent,"end_ordinal_exclusive":ordinal,"end_byte_offset":bytes});
        }
        let contents = row(ordinal, "session_meta", metadata)?
            + &row(
                ordinal + 1,
                "response_item",
                json!({"type":"message","role":"user","content":id}),
            )?;
        tokio::fs::write(
            directory.path().join(format!("rollout-{id}.jsonl")),
            &contents,
        )
        .await?;
        previous = Some((id, contents.len()));
    }
    let initial = first.resolve(node("leaf")).await?;
    assert!(initial.gaps.is_empty(), "{:?}", initial.gaps);
    for id in ["root", "middle"] {
        tokio::fs::remove_file(directory.path().join(format!("rollout-{id}.jsonl"))).await?;
    }
    drop(first);
    db.close().await?;
    let reopened = make_resolver(crate::db::connect(&database_url).await?)?;
    let restored = reopened.resolve(node("leaf")).await?;
    assert!(restored.gaps.is_empty(), "{:?}", restored.gaps);
    assert_eq!(
        serde_json::to_value(restored)?,
        serde_json::to_value(initial)?
    );
    Ok(())
}

#[tokio::test]
async fn bound_fork_never_rebinds_after_stored_parent_evidence_is_lost() -> Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    for corrupt in [false, true] {
        let directory = tempfile::tempdir()?;
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvidenceStore::new(db.clone(), "local")?;
        let collector = NativeCollector::new(
            store.clone(),
            NativeRoot {
                harness: Harness::Codex,
                namespace: "fixture".into(),
                directory: directory.path().into(),
            },
        )?;
        let resolver = HistoryResolver::new(store.clone(), collector);
        let root_path = directory.path().join("rollout-root.jsonl");
        let prefix = row(
            0,
            "session_meta",
            json!({"id":"root","cli_version":"0.153.0-alpha.5"}),
        )? + &row(
            1,
            "response_item",
            json!({"type":"message","content":"original"}),
        )?;
        tokio::fs::write(&root_path, &prefix).await?;
        tokio::fs::write(
            directory.path().join("rollout-leaf.jsonl"),
            row(
                2,
                "session_meta",
                json!({"id":"leaf","cli_version":"0.153.0-alpha.5","history_base":{
                "thread_id":"root","end_ordinal_exclusive":2,"end_byte_offset":prefix.len()}}),
            )?,
        )
        .await?;
        let initial = resolver.resolve(node("leaf")).await?;
        assert!(initial.gaps.is_empty(), "{:?}", initial.gaps);
        let key = binding_key(&node("leaf"), &node("root"), 2, prefix.len() as u64)?;
        let binding = store.fork_binding(&key).await?.context("binding")?;
        let records = store.records(&binding.checkpoint).await?;
        let id = &records.last().context("parent record")?.id;
        let sql = if corrupt {
            "UPDATE native_evidence_records SET record_json = '{}' WHERE id = ?"
        } else {
            "DELETE FROM native_evidence_records WHERE id = ?"
        };
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            [id.clone().into()],
        ))
        .await?;
        // A readable replacement cannot repair an immutable binding to different
        // evidence. The service turns this resolution error into a visible gap.
        tokio::fs::write(&root_path, prefix.replace("original", "modified")).await?;
        assert!(resolver.resolve(node("leaf")).await.is_err());
        assert_eq!(
            store
                .fork_binding(&key)
                .await?
                .context("unchanged binding")?,
            binding
        );
    }
    Ok(())
}

#[tokio::test]
async fn nested_fork_dependencies_use_each_native_cut_after_parents_continue() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let resolver = resolver(directory.path()).await?;
    let root = row(
        0,
        "session_meta",
        json!({"id":"root","cli_version":"0.153.0-alpha.5"}),
    )? + &row(
        1,
        "response_item",
        json!({"type":"message","role":"user","content":"original"}),
    )?;
    let middle = row(
        2,
        "session_meta",
        json!({"id":"middle","cli_version":"0.153.0-alpha.5","history_base":{"thread_id":"root","end_ordinal_exclusive":2,"end_byte_offset":root.len()}}),
    )? + &row(
        3,
        "response_item",
        json!({"type":"message","role":"assistant","content":"middle"}),
    )?;
    let leaf = row(
        4,
        "session_meta",
        json!({"id":"leaf","cli_version":"0.153.0-alpha.5","history_base":{"thread_id":"middle","end_ordinal_exclusive":4,"end_byte_offset":middle.len()}}),
    )? + &row(
        5,
        "response_item",
        json!({"type":"message","role":"assistant","content":"leaf"}),
    )?;
    tokio::fs::write(
        directory.path().join("rollout-root.jsonl"),
        root.clone()
            + &row(
                2,
                "response_item",
                json!({"type":"message","content":"later root"}),
            )?,
    )
    .await?;
    tokio::fs::write(
        directory.path().join("rollout-middle.jsonl"),
        middle.clone()
            + &row(
                4,
                "response_item",
                json!({"type":"message","content":"later middle"}),
            )?,
    )
    .await?;
    tokio::fs::write(directory.path().join("rollout-leaf.jsonl"), leaf).await?;
    // Backfill the continuing parents before resolving the fork. Stored tails
    // must not leak into the inherited context or its pinned range.
    resolver.resolve(node("root")).await?;
    resolver.resolve(node("middle")).await?;
    // A later filesystem rollback removes only post-fork work. Both existing
    // checkpoints still refer to the same intact prefix and generation.
    tokio::fs::write(directory.path().join("rollout-root.jsonl"), root).await?;
    tokio::fs::write(directory.path().join("rollout-middle.jsonl"), middle).await?;
    // Reconcile both rewritten parents first: the child's result must not
    // depend on node iteration order or the parent's current generation.
    resolver.resolve(node("root")).await?;
    resolver.resolve(node("middle")).await?;
    let history = resolver.resolve(node("leaf")).await?;
    assert!(history.gaps.is_empty(), "{:?}", history.gaps);
    let projection = history.projection.context("leaf projection")?;
    assert_eq!(projection.raw_record_ids.len(), 2);
    assert_eq!(projection.effective_context.len(), 3);
    let parent = history.parent.context("middle")?;
    assert_eq!(
        parent
            .source
            .context("middle source")?
            .range
            .context("middle range")?
            .end,
        2
    );
    let ancestor = parent.parent.context("root")?;
    assert_eq!(
        ancestor
            .source
            .context("root source")?
            .range
            .context("root range")?
            .end,
        2
    );
    Ok(())
}

#[tokio::test]
async fn missing_parent_and_unsupported_checkpoint_remain_explicit_gaps() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let resolver = resolver(directory.path()).await?;
    let path = directory.path().join("rollout-leaf.jsonl");
    tokio::fs::write(&path, row(0, "session_meta", json!({"id":"leaf","cli_version":"0.153.0-alpha.5","history_base":{"thread_id":"missing","end_ordinal_exclusive":2,"end_byte_offset":100}}))?).await?;
    let missing = resolver.resolve(node("leaf")).await?;
    assert!(missing.gaps.contains("native_history_unavailable"));
    assert!(missing.gaps.contains("inherited_history_missing"));
    tokio::fs::write(&path, row(0, "session_meta", json!({"id":"leaf","cli_version":"0.153.0-alpha.5","history_base":{"thread_id":"missing"}}))?).await?;
    let unsupported = resolver.resolve(node("leaf")).await?;
    assert!(unsupported.gaps.contains("history_base_unsupported"));
    assert!(unsupported.edge.is_none());
    Ok(())
}

#[tokio::test]
async fn inherited_empty_context_is_valid_when_the_parent_checkpoint_exists() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let resolver = resolver(directory.path()).await?;
    let root = row(
        0,
        "session_meta",
        json!({"id":"root","cli_version":"0.153.0-alpha.5"}),
    )?;
    tokio::fs::write(directory.path().join("rollout-leaf.jsonl"), row(1, "session_meta", json!({"id":"leaf","cli_version":"0.153.0-alpha.5","history_base":{"thread_id":"root","end_ordinal_exclusive":1,"end_byte_offset":root.len()}}))?).await?;
    tokio::fs::write(directory.path().join("rollout-root.jsonl"), root).await?;
    let history = resolver.resolve(node("leaf")).await?;
    assert!(history.gaps.is_empty(), "{:?}", history.gaps);
    assert!(
        history
            .projection
            .context("projection")?
            .effective_context
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn bound_fork_survives_parent_rewrite_removal_and_resolver_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db.clone(), "local")?;
    let collector = NativeCollector::new(
        store.clone(),
        NativeRoot {
            harness: Harness::Codex,
            namespace: "fixture".into(),
            directory: directory.path().into(),
        },
    )?;
    let resolver = HistoryResolver::new(store, collector);
    let path = directory.path().join("rollout-root.jsonl");
    let prefix = row(
        0,
        "session_meta",
        json!({"id":"root","cli_version":"0.153.0-alpha.5"}),
    )? + &row(
        1,
        "response_item",
        json!({"type":"message","content":"original"}),
    )?;
    tokio::fs::write(&path, &prefix).await?;
    let leaf_path = directory.path().join("rollout-leaf.jsonl");
    let leaf = row(
        2,
        "session_meta",
        json!({"id":"leaf","cli_version":"0.153.0-alpha.5","history_base":{"thread_id":"root","end_ordinal_exclusive":2,"end_byte_offset":prefix.len()}}),
    )?;
    tokio::fs::write(&leaf_path, &leaf).await?;
    let initial = resolver.resolve(node("leaf")).await?;
    assert!(initial.gaps.is_empty(), "{:?}", initial.gaps);
    let initial_context = initial
        .projection
        .context("initial context")?
        .effective_context;
    let binding_id = binding_key(&node("leaf"), &node("root"), 2, prefix.len() as u64)?;
    let binding = resolver
        .store
        .fork_binding(&binding_id)
        .await?
        .context("persisted binding")?;
    tokio::fs::write(&path, prefix.replace("original", "modified")).await?;
    resolver.resolve(node("root")).await?;
    tokio::fs::remove_file(&path).await?;
    // Appending and truncating the child also changes its source generation;
    // native fork ancestry and the binding key must remain stable.
    tokio::fs::write(
        &leaf_path,
        leaf.clone() + &row(3, "response_item", json!({"type":"message"}))?,
    )
    .await?;
    resolver.resolve(node("leaf")).await?;
    tokio::fs::write(&leaf_path, leaf).await?;
    let restarted = HistoryResolver::new(resolver.store.clone(), resolver.collector.clone());
    let restored = restarted.resolve(node("leaf")).await?;
    // The child's own filesystem rewrite is visible, while its inherited
    // parent evidence remains exactly the earlier bound record set.
    assert_eq!(
        restored
            .projection
            .context("restored context")?
            .effective_context,
        initial_context
    );
    assert!(restored.parent.context("parent")?.gaps.is_empty());
    assert_eq!(
        restarted.store.fork_binding(&binding_id).await?,
        Some(binding.clone())
    );
    let mut forged = binding.clone();
    let forged_digest = crate::eval::types::canonical_digest(&"different")?;
    for digest in forged.record_digests.values_mut() {
        *digest = forged_digest.clone();
    }
    assert!(restarted.store.bind_fork(&forged).await.is_err());
    let foreign = EvidenceStore::new(db, "another-owner")?;
    assert!(foreign.fork_binding(&binding_id).await?.is_none());
    assert!(foreign.bind_fork(&binding).await.is_err());
    Ok(())
}

#[tokio::test]
async fn unbound_fork_does_not_guess_between_different_historical_prefixes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let resolver = resolver(directory.path()).await?;
    let path = directory.path().join("rollout-root.jsonl");
    let prefix = row(
        0,
        "session_meta",
        json!({"id":"root","cli_version":"0.153.0-alpha.5"}),
    )? + &row(
        1,
        "response_item",
        json!({"type":"message","content":"original"}),
    )?;
    tokio::fs::write(&path, &prefix).await?;
    resolver.resolve(node("root")).await?;
    tokio::fs::write(&path, prefix.replace("original", "modified")).await?;
    resolver.resolve(node("root")).await?;
    tokio::fs::write(directory.path().join("rollout-leaf.jsonl"), row(2, "session_meta", json!({"id":"leaf","cli_version":"0.153.0-alpha.5","history_base":{"thread_id":"root","end_ordinal_exclusive":2,"end_byte_offset":prefix.len()}}))?).await?;
    let history = resolver.resolve(node("leaf")).await?;
    assert!(history.gaps.contains("fork_history_ambiguous"));
    assert!(
        resolver
            .store
            .fork_binding(&binding_key(
                &node("leaf"),
                &node("root"),
                2,
                prefix.len() as u64
            )?)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn reverted_rollouts_keep_one_thread_but_distinct_bounded_histories() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let resolver = resolver(directory.path()).await?;
    let root_path = directory.path().join("rollout-root.jsonl");
    let reverted_path = directory.path().join("rollout-root_revision-1.jsonl");
    let root = row(
        0,
        "session_meta",
        json!({"id":"root","cli_version":"0.153.4"}),
    )? + &row(
        1,
        "response_item",
        json!({"type":"message","content":"before revert"}),
    )?;
    tokio::fs::write(
        &root_path,
        root.clone()
            + &row(
                2,
                "response_item",
                json!({"type":"message","content":"discarded parent tail"}),
            )?,
    )
    .await?;
    let reverted = row(
        2,
        "session_meta",
        json!({"id":"root","cli_version":"0.153.4","history_base":{
            "thread_id":"root","end_ordinal_exclusive":2,"end_byte_offset":root.len()
        }}),
    )? + &row(
        3,
        "response_item",
        json!({"type":"message","content":"after revert"}),
    )?;
    tokio::fs::write(&reverted_path, &reverted).await?;
    let observed = resolver.resolve(node("root")).await?;
    assert_eq!(
        observed.gaps,
        BTreeSet::from(["native_active_rollout_unselected".into()])
    );
    assert!(observed.projection.is_none());
    assert_eq!(observed.variants.len(), 2);
    let revision = observed
        .variants
        .iter()
        .find(|history| {
            history
                .source
                .as_ref()
                .and_then(|source| source.rollout_id.as_deref())
                == Some("revision-1")
        })
        .context("reverted variant")?;
    assert!(revision.gaps.is_empty(), "{:?}", revision.gaps);
    assert_eq!(revision.node, node("root"));
    assert_eq!(
        revision.parent.as_ref().context("original history")?.node,
        node("root")
    );
    assert_eq!(
        revision.edge.as_ref().context("rewind edge")?.kind,
        EdgeKind::Rewind
    );
    assert_eq!(
        revision
            .projection
            .as_ref()
            .context("reverted projection")?
            .effective_context
            .len(),
        2
    );

    // A fork of the reverted rollout refers to revision-1, not to the stable
    // thread root. Continuing either physical parent cannot extend its cut.
    let leaf_path = directory.path().join("rollout-leaf.jsonl");
    tokio::fs::write(&leaf_path, row(4, "session_meta", json!({"id":"leaf","cli_version":"0.153.4",
        "forked_from_id":"root", "forked_from_ordinal_exclusive":4,
        "history_base":{"thread_id":"revision-1","end_ordinal_exclusive":4,"end_byte_offset":reverted.len()}
    }))? + &row(5, "response_item", json!({"type":"message","content":"fork work"}))?).await?;
    tokio::fs::write(
        &reverted_path,
        reverted.clone()
            + &row(
                4,
                "response_item",
                json!({"type":"message","content":"later reverted tail"}),
            )?,
    )
    .await?;
    let fork = resolver.resolve(node("leaf")).await?;
    assert!(fork.gaps.is_empty(), "{:?}", fork.gaps);
    assert_eq!(
        fork.edge.as_ref().context("logical fork edge")?.from,
        node("root")
    );
    assert_eq!(
        fork.projection
            .as_ref()
            .context("fork projection")?
            .effective_context
            .len(),
        3
    );
    assert_eq!(
        fork.parent
            .as_ref()
            .context("physical parent")?
            .source
            .as_ref()
            .context("parent source")?
            .rollout_id
            .as_deref(),
        Some("revision-1")
    );
    tokio::fs::remove_file(root_path).await?;
    tokio::fs::remove_file(reverted_path).await?;
    let restarted = HistoryResolver::new(resolver.store.clone(), resolver.collector.clone());
    let restored = restarted.resolve(node("leaf")).await?;
    assert_eq!(serde_json::to_value(restored)?, serde_json::to_value(fork)?);
    Ok(())
}

#[tokio::test]
async fn physical_prefix_does_not_invent_a_different_logical_fork_parent() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let resolver = resolver(directory.path()).await?;
    let parent = row(
        0,
        "session_meta",
        json!({"id":"physical","cli_version":"0.153.4"}),
    )?;
    tokio::fs::write(directory.path().join("rollout-physical.jsonl"), &parent).await?;
    tokio::fs::write(directory.path().join("rollout-leaf.jsonl"), row(1, "session_meta", json!({
        "id":"leaf","cli_version":"0.153.4","forked_from_id":"logical","forked_from_ordinal_exclusive":9,
        "history_base":{"thread_id":"physical","end_ordinal_exclusive":1,"end_byte_offset":parent.len()}
    }))?).await?;
    let history = resolver.resolve(node("leaf")).await?;
    assert!(history.gaps.is_empty(), "{:?}", history.gaps);
    assert_eq!(
        history.parent.context("physical parent")?.node,
        node("physical")
    );
    assert!(history.edge.is_none());
    Ok(())
}

#[tokio::test]
async fn old_fork_binding_remains_readable_without_rewriting_its_object() -> Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let directory = tempfile::tempdir()?;
    let database_url = format!("sqlite://{}", directory.path().join("legacy.db").display());
    let db = crate::db::connect(&database_url).await?;
    crate::db::run_migrations(&db).await?;
    let make = |db| -> Result<HistoryResolver> {
        let store = EvidenceStore::new(db, "local")?;
        Ok(HistoryResolver::new(
            store.clone(),
            NativeCollector::new(
                store,
                NativeRoot {
                    harness: Harness::Codex,
                    namespace: "fixture".into(),
                    directory: directory.path().into(),
                },
            )?,
        ))
    };
    let resolver = make(db.clone())?;
    let parent_path = directory.path().join("rollout-root.jsonl");
    let parent = row(
        0,
        "session_meta",
        json!({"id":"root","cli_version":"0.153.4"}),
    )?;
    tokio::fs::write(&parent_path, &parent).await?;
    tokio::fs::write(directory.path().join("rollout-leaf.jsonl"), row(1, "session_meta", json!({
        "id":"leaf","cli_version":"0.153.4","history_base":{"thread_id":"root","end_ordinal_exclusive":1,"end_byte_offset":parent.len()}
    }))?).await?;
    resolver.resolve(node("leaf")).await?;
    let new_id = binding_key(&node("leaf"), &node("root"), 1, parent.len() as u64)?;
    let mut legacy = resolver
        .store
        .fork_binding(&new_id)
        .await?
        .context("new binding")?;
    legacy.rollouts = None;
    legacy.id = ForkBinding::key(&legacy.child, &legacy.parent, 1, parent.len() as u64)?;
    assert!(serde_json::to_value(&legacy)?.get("rollouts").is_none());
    resolver.store.bind_fork(&legacy).await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "DELETE FROM native_evidence_objects WHERE kind = 'fork_binding' AND object_key = ?",
        [new_id.clone().into()],
    ))
    .await?;
    // A pre-rollout-identity database has neither the new binding nor any
    // identity objects. Its immutable ordinary fork still has original records.
    db.execute(Statement::from_string(
        DbBackend::Sqlite,
        "DELETE FROM native_evidence_objects WHERE kind = 'rollout_identity'".to_owned(),
    ))
    .await?;
    tokio::fs::remove_file(parent_path).await?;
    drop(resolver);
    db.close().await?;
    let resolver = make(crate::db::connect(&database_url).await?)?;
    let restored = resolver.resolve(node("leaf")).await?;
    assert!(restored.gaps.is_empty(), "{:?}", restored.gaps);
    assert!(
        resolver
            .store
            .rollout_identity(&legacy.checkpoint.source_id)
            .await?
            .is_none()
    );
    assert_eq!(resolver.store.fork_binding(&legacy.id).await?, Some(legacy));
    assert!(resolver.store.fork_binding(&new_id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn missing_top_level_rollout_survives_database_reopen_without_active_selection() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("evidence.db").display()
    );
    let make = |db| -> Result<HistoryResolver> {
        let store = EvidenceStore::new(db, "local")?;
        Ok(HistoryResolver::new(
            store.clone(),
            NativeCollector::new(
                store,
                NativeRoot {
                    namespace: "fixture".into(),
                    harness: Harness::Codex,
                    directory: directory.path().into(),
                },
            )?,
        ))
    };
    let db = crate::db::connect(&database_url).await?;
    crate::db::run_migrations(&db).await?;
    let initial = make(db.clone())?;
    let root_path = directory.path().join("rollout-root.jsonl");
    let root = row(
        0,
        "session_meta",
        json!({"id":"root","cli_version":"0.153.4"}),
    )?;
    tokio::fs::write(&root_path, &root).await?;
    tokio::fs::write(directory.path().join("rollout-root_revision-1.jsonl"), row(1, "session_meta", json!({
        "id":"root","cli_version":"0.153.4","history_base":{"thread_id":"root","end_ordinal_exclusive":1,"end_byte_offset":root.len()}
    }))?).await?;
    let before = initial.resolve(node("root")).await?;
    assert_eq!(before.variants.len(), 2);
    tokio::fs::remove_file(root_path).await?;
    drop(initial);
    db.close().await?;
    let reopened = make(crate::db::connect(&database_url).await?)?;
    let after = reopened.resolve(node("root")).await?;
    assert_eq!(after.variants.len(), 2);
    assert!(after.projection.is_none());
    assert!(after.gaps.contains("native_active_rollout_unselected"));
    assert!(after.gaps.contains("native_source_file_unavailable"));
    for variant in &after.variants {
        let id = &variant.source.as_ref().context("source")?.source.id;
        let previous = before
            .variants
            .iter()
            .find(|item| {
                item.source
                    .as_ref()
                    .is_some_and(|source| &source.source.id == id)
            })
            .context("same source")?;
        assert_eq!(variant.projection, previous.projection);
    }
    Ok(())
}

#[tokio::test]
async fn damaged_rollout_identity_does_not_hide_healthy_sibling_history() -> Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let directory = tempfile::tempdir()?;
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db.clone(), "local")?;
    let resolver = HistoryResolver::new(
        store.clone(),
        NativeCollector::new(
            store.clone(),
            NativeRoot {
                namespace: "fixture".into(),
                harness: Harness::Codex,
                directory: directory.path().into(),
            },
        )?,
    );
    for name in ["rollout-root.jsonl", "rollout-root_revision-1.jsonl"] {
        tokio::fs::write(
            directory.path().join(name),
            row(
                0,
                "session_meta",
                json!({"id":"root","cli_version":"0.153.4"}),
            )?,
        )
        .await?;
    }
    let before = resolver.resolve(node("root")).await?;
    let damaged = before
        .variants
        .iter()
        .filter_map(|history| history.source.as_ref())
        .find(|source| source.rollout_id.as_deref() == Some("revision-1"))
        .context("damaged source")?;
    db.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
        "UPDATE native_evidence_objects SET object_json = '{}' WHERE kind = 'rollout_identity' AND object_key = ?",
        [damaged.source.id.clone().into()])).await?;
    let after = resolver.resolve(node("root")).await?;
    assert_eq!(after.variants.len(), 2);
    assert!(after.gaps.contains("native_stored_history_invalid"));
    assert!(after.gaps.contains("native_active_rollout_unselected"));
    assert!(after.variants.iter().any(|history| {
        history.projection.is_some()
            && history
                .source
                .as_ref()
                .is_some_and(|source| source.rollout_id.as_deref() == Some("root"))
    }));
    Ok(())
}

#[tokio::test]
async fn unreadable_matching_parent_candidate_prevents_binding_but_keeps_healthy_prefix()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let resolver = resolver(directory.path()).await?;
    let root = row(
        0,
        "session_meta",
        json!({"id":"root","cli_version":"0.153.4"}),
    )?;
    tokio::fs::write(directory.path().join("rollout-root.jsonl"), &root).await?;
    let another = directory.path().join("another");
    tokio::fs::create_dir_all(&another).await?;
    tokio::fs::write(another.join("rollout-root.jsonl"), "invalid json\n").await?;
    tokio::fs::write(directory.path().join("rollout-leaf.jsonl"), row(1, "session_meta", json!({
        "id":"leaf","cli_version":"0.153.4","history_base":{"thread_id":"root","end_ordinal_exclusive":1,"end_byte_offset":root.len()}
    }))?).await?;
    let history = resolver.resolve(node("leaf")).await?;
    assert!(history.gaps.contains("native_rollout_candidate_unverified"));
    assert!(
        history
            .parent
            .context("healthy prefix")?
            .projection
            .is_some()
    );
    assert!(
        resolver
            .store
            .fork_binding(&binding_key(
                &node("leaf"),
                &node("root"),
                1,
                root.len() as u64
            )?)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn preferred_prefix_replacement_cannot_erase_an_observed_conflict() -> Result<()> {
    use crate::session_evidence::store::rollouts::RolloutIdentity;
    use crate::session_evidence::types::{
        RecordInput, RecordRef, SourceCursor, SourceDescriptor, SourceFormat,
    };
    use sha2::{Digest, Sha256};
    let directory = tempfile::tempdir()?;
    let resolver = resolver(directory.path()).await?;
    let mut sources = vec![];
    for index in 0..3 {
        sources.push(
            resolver
                .store
                .register(SourceDescriptor {
                    namespace: "fixture".into(),
                    harness: Harness::Codex,
                    format: SourceFormat::CodexRollout,
                    locator: format!("file:fixture-{index}"),
                    node: Some(node("root")),
                })
                .await?,
        );
    }
    // Set up deterministic database order: a replacement A, a conflicting B,
    // then an original generation C matching A. C may improve the selected
    // source's provenance, but cannot make the observed B conflict disappear.
    sources.sort_by(|left, right| left.id.cmp(&right.id));
    let mut cut_bytes = 0;
    for (index, source) in sources.into_iter().enumerate() {
        let generation = if index == 0 {
            "fixture/replacement/A"
        } else {
            "fixture/original"
        };
        let metadata = row(
            0,
            "session_meta",
            json!({"id":"root","cli_version":"0.153.4"}),
        )?;
        let message = row(
            1,
            "response_item",
            json!({"type":"message","content":if index == 1 {"prefixB"} else {"prefixA"}}),
        )?;
        let bytes = metadata.clone() + &message;
        cut_bytes = bytes.len() as u64;
        let mut records = vec![];
        let mut offset = 0;
        for (sequence, line) in [metadata, message].into_iter().enumerate() {
            records.push(RecordInput {
                generation: generation.into(),
                sequence: sequence as u64,
                byte_start: Some(offset),
                byte_end: Some(offset + line.len() as u64),
                producer_version: Some("0.153.4".into()),
                raw: serde_json::from_str(&line)?,
            });
            offset += line.len() as u64;
        }
        let source = resolver
            .store
            .append(
                &source,
                &records,
                SourceCursor {
                    generation: generation.into(),
                    offset,
                    next_sequence: 2,
                    anchor_digest: format!(
                        "sha256:{}",
                        hex::encode(Sha256::digest(bytes.as_bytes()))
                    ),
                },
            )
            .await?;
        let first = resolver
            .store
            .records(&SourceRange {
                source_id: source.id.clone(),
                generation: generation.into(),
                start: 0,
                end: 1,
            })
            .await?;
        resolver
            .store
            .bind_rollout_identity(&RolloutIdentity {
                id: source.id,
                revision: 0,
                node: node("root"),
                rollout_id: "root".into(),
                metadata: RecordRef::from_record(first.first().context("metadata")?)?,
                observed_name: "rollout-root.jsonl".into(),
            })
            .await?;
    }
    tokio::fs::write(directory.path().join("rollout-leaf.jsonl"), row(2, "session_meta", json!({
        "id":"leaf","cli_version":"0.153.4","history_base":{"thread_id":"root","end_ordinal_exclusive":2,"end_byte_offset":cut_bytes}
    }))?).await?;
    let history = resolver.resolve(node("leaf")).await?;
    assert!(history.gaps.contains("fork_history_ambiguous"));
    assert!(
        !history
            .parent
            .context("selected prefix")?
            .gaps
            .contains("source_replaced_or_truncated")
    );
    assert!(
        resolver
            .store
            .fork_binding(&binding_key(&node("leaf"), &node("root"), 2, cut_bytes)?)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn sibling_rollouts_share_import_budget_before_their_records_are_written() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let resolver = resolver(directory.path()).await?;
    for name in ["rollout-root.jsonl", "rollout-root_revision-1.jsonl"] {
        let mut bytes = row(
            0,
            "session_meta",
            json!({"id":"root","cli_version":"0.153.4"}),
        )?;
        for ordinal in 1..4 {
            bytes += &row(
                ordinal,
                "response_item",
                json!({"type":"message","content":"data"}),
            )?;
        }
        tokio::fs::write(directory.path().join(name), bytes).await?;
    }
    let mut budget = Budget {
        execution_bytes: MAX_OBJECT_BYTES,
        nodes: 0,
        records: 0,
        remaining_import: 5,
    };
    let first = resolver
        .visit(node("root"), None, &mut BTreeSet::new(), &mut budget)
        .await?;
    assert!(first.gaps.contains("native_import_budget_exhausted"));
    let sources = resolver.store.node_sources(&node("root")).await?;
    assert_eq!(
        sources
            .iter()
            .map(|source| source.cursor.next_sequence)
            .sum::<u64>(),
        5
    );
    assert!(
        sources
            .iter()
            .any(|source| source.cursor.next_sequence == 1)
    );
    let complete = resolver.resolve(node("root")).await?;
    assert_eq!(
        complete.gaps,
        BTreeSet::from(["native_active_rollout_unselected".into()])
    );
    assert_eq!(
        resolver
            .store
            .node_sources(&node("root"))
            .await?
            .iter()
            .map(|source| source.cursor.next_sequence)
            .sum::<u64>(),
        8
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires an isolated Codex capture with fork-after-revert and two compactions"]
async fn native_codex_capture_preserves_revert_and_compaction_history() -> Result<()> {
    let source_root = std::path::PathBuf::from(std::env::var("BITROUTER_TEST_CODEX_ROLLOUT_ROOT")?);
    let root_id = std::env::var("BITROUTER_TEST_CODEX_THREAD_ID")?;
    let fork_id = std::env::var("BITROUTER_TEST_CODEX_FORK_ID")?;
    let reverted_fork_id = std::env::var("BITROUTER_TEST_CODEX_REVERT_FORK_ID")?;
    let reader = resolver(&source_root).await?;
    let directory = tempfile::tempdir()?;
    // Work on copies: native producer files are never removed or modified.
    let mut copied = BTreeSet::new();
    for id in [&root_id, &fork_id, &reverted_fork_id] {
        for (_, path) in reader.collector.discover(id).await? {
            let name = path.file_name().context("native capture filename")?;
            if copied.insert(name.to_owned()) {
                tokio::fs::copy(&path, directory.path().join(name)).await?;
            }
        }
    }
    let resolver = resolver(directory.path()).await?;
    let root = resolver.resolve(node(&root_id)).await?;
    assert!(root.variants.len() >= 2);
    assert!(root.gaps.contains("native_active_rollout_unselected"));
    let reverted = root
        .variants
        .iter()
        .find(|history| {
            history
                .edge
                .as_ref()
                .is_some_and(|edge| edge.kind == EdgeKind::Rewind)
        })
        .context("native reverted history")?;
    assert_eq!(
        reverted.parent.as_ref().context("revert parent")?.node,
        node(&root_id)
    );
    assert!(!reverted.gaps.contains("history_dependency_cycle"));
    let fork = resolver.resolve(node(&fork_id)).await?;
    assert_eq!(
        fork.projection
            .as_ref()
            .context("native fork projection")?
            .transitions
            .iter()
            .filter(|transition| transition.kind == EdgeKind::Compact)
            .count(),
        2
    );
    // These native records remain outside the implemented context semantics;
    // this test must not claim full version admission or cost attribution.
    assert!(
        fork.gaps.iter().all(|gap| gap == "unknown_rollout_record"),
        "{:?}",
        fork.gaps
    );
    let reverted_fork = resolver.resolve(node(&reverted_fork_id)).await?;
    assert!(
        reverted_fork
            .gaps
            .iter()
            .all(|gap| gap == "unknown_rollout_record"),
        "{:?}",
        reverted_fork.gaps
    );
    let physical_parent = reverted_fork
        .parent
        .as_ref()
        .context("reverted fork parent")?;
    assert_eq!(physical_parent.node, node(&root_id));
    assert_eq!(
        physical_parent
            .source
            .as_ref()
            .context("parent rollout")?
            .rollout_id,
        reverted
            .source
            .as_ref()
            .context("reverted rollout")?
            .rollout_id
    );
    assert_ne!(
        physical_parent
            .source
            .as_ref()
            .context("parent rollout")?
            .rollout_id
            .as_deref(),
        Some(root_id.as_str())
    );
    assert_eq!(
        reverted_fork.edge.as_ref().context("logical fork")?.kind,
        EdgeKind::Fork
    );
    for (candidate, path) in resolver.collector.discover(&root_id).await? {
        assert_eq!(candidate, node(&root_id));
        tokio::fs::remove_file(path).await?;
    }
    let restored = resolver.resolve(node(&fork_id)).await?;
    assert_eq!(serde_json::to_value(restored)?, serde_json::to_value(fork)?);
    let restored = resolver.resolve(node(&reverted_fork_id)).await?;
    assert_eq!(
        serde_json::to_value(restored)?,
        serde_json::to_value(reverted_fork)?
    );
    Ok(())
}
