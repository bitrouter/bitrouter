use super::super::collector::NativeRoot;
use super::*;
use serde_json::{Value, json};
use std::path::Path;

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
        let key = ForkBinding::key(&node("leaf"), &node("root"), 2, prefix.len() as u64)?;
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
    let binding_id = ForkBinding::key(&node("leaf"), &node("root"), 2, prefix.len() as u64)?;
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
            .fork_binding(&ForkBinding::key(
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
