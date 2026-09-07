use super::*;
use crate::session_evidence::journal::Journal;
use crate::session_evidence::types::Harness;
use serde_json::json;

fn root(session: &str) -> NodeKey {
    NodeKey {
        namespace: "native-profile".into(),
        harness: Harness::ClaudeCode,
        native_id: session.into(),
        agent_id: None,
    }
}

#[tokio::test]
async fn stale_journals_refresh_the_cursor_and_preserve_all_task_boundaries() -> Result<()> {
    let store = store().await?;
    let node = root("session");
    let first = journal(&store, "controller", &node).await?;
    let stale = journal(&store, "controller", &node).await?;
    first.append(request("one", "session")).await?;
    stale.append(request("two", "session")).await?;
    first.append(response("one")).await?;
    stale.append(response("two")).await?;
    assert_eq!(store.sources(None, 16).await?[0].cursor.next_sequence, 4);
    assert_eq!(active(&store, &node).await?.phase, AttemptPhase::Settling);
    assert_eq!(store.attempts(None, 16).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn missing_or_corrupt_boundaries_including_completed_prompts_invalidate_task_state()
-> Result<()> {
    for completed in [false, true] {
        for corrupt in [false, true] {
            let store = store().await?;
            let node = root("session");
            let journal = journal(&store, "controller", &node).await?;
            journal.append(request("origin", "session")).await?;
            journal.append(response("origin")).await?;
            journal.append(request("later", "session")).await?;
            if completed {
                journal.append(response("later")).await?;
            }
            let key = PromptOperation::key("controller", "later")?;
            let operation = store
                .prompt_operation(&store.db, &key)
                .await?
                .context("operation")?;
            let boundary = operation.response.as_ref().unwrap_or(&operation.request);
            if corrupt {
                record_entity::Entity::update_many()
                    .col_expr(record_entity::Column::RecordJson, Expr::value("{}"))
                    .filter(record_entity::Column::Id.eq(&boundary.record_id))
                    .exec(&store.db)
                    .await?;
            } else {
                record_entity::Entity::delete_by_id(&boundary.record_id)
                    .exec(&store.db)
                    .await?;
            }
            assert!(store.active_attempt(&node).await.is_err());
            let source = store.sources(None, 16).await?.remove(0);
            assert!(journal.append(response("later")).await.is_err());
            assert_eq!(
                store.source(&source.id).await?.context("source")?.cursor,
                source.cursor
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn two_controllers_on_separate_connections_share_task_without_losing_open_operations()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite:{}?mode=rwc",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let first_store = EvidenceStore::new(db, "alice")?;
    let second_store = EvidenceStore::new(crate::db::connect(&url).await?, "alice")?;
    let node = root("session");
    let first = journal(&first_store, "first", &node).await?;
    let second = journal(&second_store, "second", &node).await?;
    let (a, b) = tokio::join!(
        first.append(request("one", "session")),
        second.append(request("one", "session"))
    );
    a?;
    b?;
    assert_eq!(first_store.attempts(None, 16).await?.len(), 1);
    assert!(second_store.has_unobserved_prompts(&node, "second").await?);
    second.append(response("one")).await?;
    assert_eq!(
        active(&second_store, &node).await?.phase,
        AttemptPhase::Collecting
    );
    assert!(second_store.has_unobserved_prompts(&node, "second").await?);
    // The origin can still finish its RPC; another controller never invents
    // that response merely because its own prompt ended.
    first.append(response("one")).await?;
    assert_eq!(
        active(&second_store, &node).await?.phase,
        AttemptPhase::Settling
    );
    assert!(!second_store.has_unobserved_prompts(&node, "second").await?);
    Ok(())
}

async fn store() -> Result<EvidenceStore> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    EvidenceStore::new(db, "alice")
}

async fn verify_concurrent_read_snapshot(url: &str) -> Result<()> {
    let db = crate::db::connect(url).await?;
    crate::db::run_migrations(&db).await?;
    let reader = EvidenceStore::new(db, format!("fixture-{}", uuid::Uuid::new_v4()))?;
    let writer = EvidenceStore::new(crate::db::connect(url).await?, reader.owner())?;
    let node = root("session");
    let journal = journal(&writer, "writer", &node).await?;
    journal.append(request("one", "session")).await?;
    let transaction = reader.task_read_transaction().await?;
    let old_task = reader
        .active_task(&transaction, &node)
        .await?
        .context("task snapshot")?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        journal.append(response("one")),
    )
    .await??;
    let old_attempt = reader.task_attempt(&transaction, &old_task).await?;
    assert_eq!(old_attempt.phase, AttemptPhase::Collecting);
    transaction.commit().await?;
    assert_eq!(active(&reader, &node).await?.phase, AttemptPhase::Settling);
    Ok(())
}

#[tokio::test]
async fn task_reads_keep_one_snapshot_while_another_connection_finishes_a_prompt() -> Result<()> {
    let directory = tempfile::tempdir()?;
    verify_concurrent_read_snapshot(&format!(
        "sqlite:{}?mode=rwc",
        directory.path().join("evidence.db").display()
    ))
    .await
}

#[tokio::test]
#[ignore = "requires a dedicated PostgreSQL database in BITROUTER_TEST_POSTGRES_URL"]
async fn postgres_task_reads_keep_one_snapshot_during_concurrent_completion() -> Result<()> {
    let url =
        std::env::var("BITROUTER_TEST_POSTGRES_URL").context("dedicated PostgreSQL fixture URL")?;
    ensure!(
        url.starts_with("postgres://") || url.starts_with("postgresql://"),
        "PostgreSQL fixture required"
    );
    verify_concurrent_read_snapshot(&url).await
}

async fn journal(store: &EvidenceStore, controller: &str, root: &NodeKey) -> Result<Journal> {
    Journal::new(
        store.clone(),
        SourceDescriptor {
            namespace: root.namespace.clone(),
            harness: root.harness,
            format: SourceFormat::Acp,
            locator: format!("controller:{controller}"),
            node: None,
        },
        "fixture/1".into(),
    )
    .await
}

fn request(operation: &str, session: &str) -> Value {
    json!({"method":"session/prompt","phase":"request","operation_id":operation,
        "native_scope":"session","observed_at":"2026-09-07T00:00:00Z",
        "payload":{"sessionId":session,"prompt":[{"type":"text","text":"same task"}]}})
}

fn response(operation: &str) -> Value {
    json!({"method":"session/prompt","phase":"response","operation_id":operation,
        "native_scope":"operation","observed_at":"2026-09-07T00:01:00Z",
        "payload":{"stopReason":"end_turn"}})
}

async fn active(store: &EvidenceStore, root: &NodeKey) -> Result<Attempt> {
    store.active_attempt(root).await?.context("active attempt")
}

#[tokio::test]
async fn overlapping_prompts_share_one_attempt_and_rpc_completion_only_starts_settlement()
-> Result<()> {
    let store = store().await?;
    let node = root("session");
    let journal = journal(&store, "controller", &node).await?;
    let (one, two) = tokio::join!(
        journal.append(request("one", "session")),
        journal.append(request("two", "session")),
    );
    one?;
    two?;
    let started = active(&store, &node).await?;
    assert_eq!(started.phase, AttemptPhase::Collecting);
    assert_eq!(store.attempts(None, 16).await?.len(), 1);
    journal.append(response("one")).await?;
    assert_eq!(active(&store, &node).await?.phase, AttemptPhase::Collecting);
    journal.append(response("two")).await?;
    let settled = active(&store, &node).await?;
    assert_eq!(settled.id, started.id);
    assert_eq!(settled.phase, AttemptPhase::Settling);
    assert!(settled.latest_manifest.is_none());
    assert!(settled.effective_manifest.is_none());
    let revision = settled.revision;
    journal.append(response("two")).await?;
    journal.append(request("two", "session")).await?;
    assert_eq!(active(&store, &node).await?.revision, revision);
    journal.append(request("three", "session")).await?;
    assert_eq!(active(&store, &node).await?.phase, AttemptPhase::Collecting);
    let mut error = response("three");
    error["payload"] = json!({"error_code":-32603});
    journal.append(error).await?;
    assert_eq!(active(&store, &node).await?.phase, AttemptPhase::Settling);
    let op = store
        .prompt_operation(&store.db, &PromptOperation::key("controller", "three")?)
        .await?
        .context("durable operation")?;
    for boundary in [&op.request, op.response.as_ref().context("response")?] {
        let records = store.records(&boundary.range).await?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, boundary.record_id);
        assert_eq!(records[0].digest, boundary.record_digest);
    }
    Ok(())
}

#[tokio::test]
async fn task_identity_survives_database_reopen_and_isolates_forks_profiles_and_owners()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite:{}?mode=rwc",
        directory.path().join("evidence.db").display()
    );
    let first_id;
    let task_id;
    {
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        let store = EvidenceStore::new(db, "alice")?;
        let journal = journal(&store, "first", &root("session")).await?;
        journal.append(request("one", "session")).await?;
        journal.append(response("one")).await?;
        let attempt = active(&store, &root("session")).await?;
        first_id = attempt.id;
        task_id = attempt.task_id;
    }
    let store = EvidenceStore::new(crate::db::connect(&url).await?, "alice")?;
    let next = journal(&store, "next", &root("session")).await?;
    next.append(request("one", "session")).await?;
    let resumed = active(&store, &root("session")).await?;
    assert_eq!(resumed.id, first_id);
    assert_eq!(resumed.task_id, task_id);
    next.append(request("fork", "forked-session")).await?;
    assert_ne!(active(&store, &root("forked-session")).await?.id, first_id);
    let mut profile = root("session");
    profile.namespace = "other-native-profile".into();
    journal(&store, "other-profile", &profile)
        .await?
        .append(request("one", "session"))
        .await?;
    assert_ne!(active(&store, &profile).await?.id, first_id);
    let foreign = EvidenceStore::new(store.db.clone(), "bob")?;
    assert!(foreign.active_attempt(&root("session")).await?.is_none());
    journal(&foreign, "bob-controller", &root("session"))
        .await?
        .append(request("one", "session"))
        .await?;
    assert_ne!(active(&foreign, &root("session")).await?.id, first_id);
    assert_eq!(store.attempts(None, 16).await?.len(), 3);
    Ok(())
}

#[tokio::test]
async fn conflicting_operation_rolls_back_its_raw_record_and_cursor() -> Result<()> {
    let store = store().await?;
    let journal = journal(&store, "controller", &root("session")).await?;
    journal.append(request("one", "session")).await?;
    let before = store.sources(None, 16).await?.remove(0);
    let attempt = active(&store, &root("session")).await?;
    let mut changed = request("one", "session");
    changed["payload"]["prompt"][0]["text"] = json!("different task");
    assert!(journal.append(changed).await.is_err());
    assert_eq!(store.source(&before.id).await?.context("source")?, before);
    assert_eq!(active(&store, &root("session")).await?, attempt);
    assert!(
        store
            .records(&SourceRange {
                source_id: before.id,
                generation: before.cursor.generation,
                start: before.cursor.next_sequence,
                end: before.cursor.next_sequence + 1,
            })
            .await?
            .is_empty()
    );
    journal.append(response("one")).await?;
    let mut conflict = response("one");
    conflict["payload"] = json!({"error_code":-32603});
    assert!(journal.append(conflict).await.is_err());
    assert_eq!(
        active(&store, &root("session")).await?.phase,
        AttemptPhase::Settling
    );
    Ok(())
}

#[tokio::test]
async fn failed_operation_insert_rolls_back_task_and_observation_then_retry_succeeds() -> Result<()>
{
    let store = store().await?;
    let journal = journal(&store, "controller", &root("session")).await?;
    store.db.execute_unprepared(
        "CREATE TRIGGER fail_prompt BEFORE INSERT ON native_evidence_objects WHEN NEW.kind = 'prompt_operation' BEGIN SELECT RAISE(ABORT, 'fixture failure'); END"
    ).await?;
    assert!(journal.append(request("one", "session")).await.is_err());
    assert!(store.attempts(None, 16).await?.is_empty());
    assert!(store.active_attempt(&root("session")).await?.is_none());
    assert_eq!(store.sources(None, 16).await?[0].cursor.next_sequence, 0);
    store
        .db
        .execute_unprepared("DROP TRIGGER fail_prompt")
        .await?;
    journal.append(request("one", "session")).await?;
    assert_eq!(store.attempts(None, 16).await?.len(), 1);
    assert_eq!(store.sources(None, 16).await?[0].cursor.next_sequence, 1);
    Ok(())
}

#[tokio::test]
async fn unknown_scopes_notifications_and_orphan_responses_cannot_create_tasks() -> Result<()> {
    let store = store().await?;
    let journal = journal(&store, "controller", &root("session")).await?;
    for scope in ["controller", "unresolved", "operation"] {
        let mut raw = request(scope, "session");
        raw["native_scope"] = json!(scope);
        journal.append(raw).await?;
        journal.append(response(scope)).await?;
    }
    let mut notification = request("notification", "synthetic-agent-view");
    notification["phase"] = json!("notification");
    journal.append(notification).await?;
    journal.append(response("orphan")).await?;
    assert!(store.attempts(None, 16).await?.is_empty());
    assert_eq!(store.sources(None, 16).await?[0].cursor.next_sequence, 8);
    Ok(())
}

#[tokio::test]
async fn terminal_response_uses_exact_operation_even_when_query_scope_becomes_unknown() -> Result<()>
{
    let store = store().await?;
    let first = journal(&store, "controller", &root("session")).await?;
    first.append(request("one", "session")).await?;
    let mut unknown_root = root("unused");
    unknown_root.namespace = "controller-default".into();
    let fallback = journal(&store, "controller", &unknown_root).await?;
    let mut raw = response("one");
    raw["native_scope"] = json!("unresolved");
    raw["payload"] = json!({"error_code":-32603});
    fallback.append(raw).await?;
    assert_eq!(
        active(&store, &root("session")).await?.phase,
        AttemptPhase::Settling
    );
    assert!(store.active_attempt(&unknown_root).await?.is_none());
    Ok(())
}
