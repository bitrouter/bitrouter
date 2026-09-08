use super::*;
use crate::session_evidence::types::{SourceDescriptor, SourceFormat};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

fn make(
    db: DatabaseConnection,
    path: &Path,
    owner: &str,
    namespace: &str,
) -> Result<HistoryResolver> {
    let store = EvidenceStore::new(db, owner)?;
    Ok(HistoryResolver::new(
        store.clone(),
        NativeCollector::new(
            store,
            NativeRoot {
                directory: path.into(),
                harness: Harness::ClaudeCode,
                namespace: namespace.into(),
            },
        )?,
    ))
}

fn claude_node(namespace: &str, agent: Option<&str>) -> NodeKey {
    NodeKey {
        namespace: namespace.into(),
        harness: Harness::ClaudeCode,
        native_id: "session".into(),
        agent_id: agent.map(str::to_owned),
    }
}

#[tokio::test]
async fn removed_claude_histories_recover_with_owner_profile_and_agent_isolation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let native = directory.path().join("projects");
    tokio::fs::create_dir(&native).await?;
    let parent_path = native.join("session.jsonl");
    let child_path = native.join("agent-worker.jsonl");
    let mut expected = std::collections::BTreeMap::new();
    for (path, agent) in [(&parent_path, None), (&child_path, Some("worker"))] {
        let node = claude_node("profile", agent);
        let mut raw = json!({"type":"user","uuid":"message","sessionId":"session",
            "parentUuid":null,"version":"2.1.220","message":{"content":agent.unwrap_or("parent")}});
        if let Some(agent) = agent {
            raw["agentId"] = json!(agent);
        }
        tokio::fs::write(path, format!("{}\n", serde_json::to_string(&raw)?)).await?;
        let resolver = make(db.clone(), &native, "local", "profile")?;
        let source = resolver
            .collector
            .reconcile(node.clone(), path, None)
            .await?;
        expected.insert(node, source.range.context("imported history")?);
    }
    for (owner, namespace) in [("other-owner", "profile"), ("local", "other-profile")] {
        make(db.clone(), &native, owner, namespace)?
            .collector
            .reconcile(claude_node(namespace, None), &parent_path, None)
            .await?;
    }
    tokio::fs::remove_file(&parent_path).await?;
    tokio::fs::remove_file(&child_path).await?;
    db.close().await?;
    let restored = make(crate::db::connect(&url).await?, &native, "local", "profile")?;
    for (node, range) in expected {
        let history = restored.resolve(node.clone()).await?;
        assert!(history.variants.is_empty());
        assert_eq!(
            history.gaps,
            BTreeSet::from(["native_source_file_unavailable".into()])
        );
        assert_eq!(
            history.source.context("restored source")?.range,
            Some(range.clone())
        );
        let projection = history.projection.context("restored projection")?;
        assert!(projection.gaps.is_empty());
        assert_eq!(projection.node, node);
        assert_eq!(projection.raw_record_ids.len(), 1);
        assert_eq!(projection.effective_context.len(), 1);
        let raw = restored.store.records(&range).await?;
        assert_eq!(
            raw[0].input.raw["message"]["content"],
            node.agent_id.as_deref().unwrap_or("parent")
        );
    }
    Ok(())
}

#[tokio::test]
async fn claude_inventory_preserves_missing_siblings_and_isolates_corrupt_registrations()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let resolver = make(db.clone(), directory.path(), "local", "profile")?;
    let node = claude_node("profile", None);
    for (name, content) in [("old", "original"), ("live", "different")] {
        let parent = directory.path().join(name);
        tokio::fs::create_dir(&parent).await?;
        let path = parent.join("session.jsonl");
        tokio::fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"user","uuid":"same-uuid",
            "sessionId":"session","parentUuid":null,"message":{"content":content}})
            ),
        )
        .await?;
        resolver
            .collector
            .reconcile(node.clone(), &path, None)
            .await?;
        if name == "old" {
            tokio::fs::remove_file(&path).await?;
        }
    }
    let corrupt = resolver
        .store
        .register(SourceDescriptor {
            namespace: "profile".into(),
            harness: Harness::ClaudeCode,
            node: Some(node.clone()),
            format: SourceFormat::ClaudeTranscript,
            locator: "file:corrupt".into(),
        })
        .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE native_evidence_sources SET cursor_digest = 'invalid' WHERE id = ?",
        [corrupt.id.into()],
    ))
    .await?;
    let history = resolver.resolve(node).await?;
    assert!(history.projection.is_none());
    assert!(history.gaps.contains("native_history_ambiguous"));
    assert!(
        history
            .gaps
            .contains("claude_history_inventory_registration_invalid")
    );
    assert_eq!(history.variants.len(), 2);
    assert_eq!(
        history
            .variants
            .iter()
            .filter(|variant| variant.gaps.contains("native_source_file_unavailable"))
            .count(),
        1
    );
    assert!(history.variants.iter().all(|variant| {
        variant
            .projection
            .as_ref()
            .is_some_and(|projection| projection.effective_context.len() == 1)
    }));
    Ok(())
}
