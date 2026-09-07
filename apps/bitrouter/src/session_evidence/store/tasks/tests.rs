use super::*;
use crate::session_evidence::journal::Journal;
use crate::session_evidence::types::Harness;
use serde_json::json;

fn root(session: &str) -> AcpSessionKey {
    AcpSessionKey {
        namespace: "native-profile".into(),
        harness: Harness::ClaudeCode,
        session_id: session.into(),
    }
}

#[tokio::test]
async fn incompatible_or_damaged_legacy_attempts_are_not_reinterpreted_or_rewritten() -> Result<()>
{
    for case in [
        "extra-member",
        "manifest",
        "ready",
        "mixed",
        "digest",
        "agent",
    ] {
        let store = store().await?;
        let legacy_root = json!({"namespace":"profile","harness":"claude_code","native_id":"public","agent_id":null});
        let mut value = json!({"id":"legacy","task_id":"task","root":legacy_root,"members":[legacy_root],
            "phase":"collecting","revision":0,"latest_manifest":null,"effective_manifest":null,
            "started_at":"2026-09-08T00:00:00Z"});
        match case {
            "extra-member" => {
                let mut other = legacy_root.clone();
                other["native_id"] = json!("another");
                value["members"]
                    .as_array_mut()
                    .context("members")?
                    .push(other);
            }
            "manifest" => value["latest_manifest"] = json!("a".repeat(64)),
            "ready" => value["phase"] = json!("ready"),
            "mixed" => {
                value["session"] =
                    json!({"namespace":"profile","harness":"claude_code","session_id":"public"})
            }
            "agent" => value["root"]["agent_id"] = json!("child"),
            _ => {}
        }
        store
            .insert_object(&store.db, "attempt", "legacy", 0, &value)
            .await?;
        if case == "digest" {
            object_entity::Entity::update_many()
                .col_expr(object_entity::Column::Digest, Expr::value("damaged"))
                .filter(object_entity::Column::Id.eq(store.object_id("attempt", "legacy")?))
                .exec(&store.db)
                .await?;
        }
        assert!(store.attempt("legacy").await.is_err(), "{case}");
        let unchanged = store
            .object(&store.db, "attempt", "legacy")
            .await?
            .context("legacy row")?;
        assert_eq!(
            serde_json::from_str::<Value>(&unchanged.object_json)?,
            value
        );
    }
    Ok(())
}

#[tokio::test]
async fn task_session_candidates_cross_pages_and_isolate_corruption_and_foreign_scopes()
-> Result<()> {
    let store = store().await?;
    let mut expected = BTreeSet::new();
    for index in 0..20 {
        let id = format!("session-{index}");
        let session = root(&id);
        journal(&store, &format!("controller-{index}"), &session)
            .await?
            .append(request("one", &id))
            .await?;
        expected.insert(session);
    }
    let first = object_entity::Entity::find()
        .filter(object_entity::Column::Owner.eq(&store.owner_key))
        .filter(object_entity::Column::Kind.eq("active_task"))
        .order_by_asc(object_entity::Column::Id)
        .one(&store.db)
        .await?
        .context("first task")?;
    let invalid: ActiveTask = decode_task_object(first.clone())?;
    expected.remove(&invalid.session);
    object_entity::Entity::update_many()
        .col_expr(object_entity::Column::ObjectJson, Expr::value("{}"))
        .filter(object_entity::Column::Id.eq(first.id))
        .exec(&store.db)
        .await?;
    for (namespace, harness) in [
        ("another-profile", Harness::ClaudeCode),
        ("native-profile", Harness::Codex),
    ] {
        let session = AcpSessionKey {
            namespace: namespace.into(),
            harness,
            session_id: "foreign".into(),
        };
        journal(
            &store,
            &format!("controller-{namespace}-{harness:?}"),
            &session,
        )
        .await?
        .append(request("one", "foreign"))
        .await?;
    }
    let bob = EvidenceStore::new(store.db.clone(), "bob")?;
    journal(&bob, "bob", &root("bob"))
        .await?
        .append(request("one", "bob"))
        .await?;
    let (sessions, gaps) = store
        .task_sessions(
            Harness::ClaudeCode,
            &BTreeSet::from(["native-profile".into()]),
        )
        .await?;
    assert_eq!(sessions, expected);
    assert_eq!(gaps, BTreeSet::from(["native_task_state_invalid".into()]));
    for session in sessions {
        assert!(store.active_attempt(&session).await?.is_some());
    }
    Ok(())
}

#[tokio::test]
async fn legacy_task_identity_is_verified_after_reopen_without_inheriting_native_membership()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite:{}?mode=rwc",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db, "alice")?;
    let session = root("public");
    let log = journal(&store, "origin-controller", &session).await?;
    log.append(request("one", "public")).await?;
    log.append(response("one")).await?;
    let original = active(&store, &session).await?;
    let task = store
        .active_task(&store.db, &session)
        .await?
        .context("task")?;
    let operation = store
        .prompt_operation(&store.db, &task.origin_operation)
        .await?
        .context("operation")?;
    let legacy_root = json!({"namespace":session.namespace,"harness":session.harness,
        "native_id":session.session_id,"agent_id":null});
    let legacy_key = canonical_digest(&legacy_root)?;
    // These are the d85c8e00 shapes, including its assumed singleton member.
    let legacy_task = json!({"id":legacy_key,"revision":task.revision,"root":legacy_root,
        "attempt_id":task.attempt_id,"origin_operation":task.origin_operation,
        "operations":task.operations,"open_operations":task.open_operations,"last_response":task.last_response});
    let legacy_attempt = json!({"id":original.id,"task_id":original.task_id,"root":legacy_root,
        "members":[legacy_root],"phase":original.phase,"revision":original.revision,
        "latest_manifest":null,"effective_manifest":null,"started_at":original.started_at});
    let legacy_operation = json!({"id":operation.id,"revision":operation.revision,
        "controller_id":operation.controller_id,"operation_id":operation.operation_id,"root":legacy_root,
        "attempt_id":operation.attempt_id,"request":operation.request,"response":operation.response});
    for (kind, old_key, new_key, value) in [
        (
            "active_task",
            task.id.as_str(),
            legacy_key.as_str(),
            &legacy_task,
        ),
        (
            "attempt",
            original.id.as_str(),
            original.id.as_str(),
            &legacy_attempt,
        ),
        (
            "prompt_operation",
            operation.id.as_str(),
            operation.id.as_str(),
            &legacy_operation,
        ),
    ] {
        object_entity::Entity::delete_by_id(store.object_id(kind, old_key)?)
            .exec(&store.db)
            .await?;
        store
            .insert_object(
                &store.db,
                kind,
                new_key,
                value["revision"].as_i64().context("revision")?,
                value,
            )
            .await?;
    }
    drop(log);
    drop(store);
    let reopened = EvidenceStore::new(crate::db::connect(&url).await?, "alice")?;
    assert_eq!(active(&reopened, &session).await?, original);
    assert!(
        reopened
            .attempt(&original.id)
            .await?
            .context("legacy attempt")?
            .members
            .is_empty()
    );
    let unchanged = reopened
        .object(&reopened.db, "attempt", &original.id)
        .await?
        .context("original row")?;
    assert_eq!(
        serde_json::from_str::<Value>(&unchanged.object_json)?,
        legacy_attempt
    );
    let continued = journal(&reopened, "replacement-controller", &session).await?;
    continued.append(request("two", "public")).await?;
    continued.append(response("two")).await?;
    let result = active(&reopened, &session).await?;
    assert_eq!(result.id, original.id);
    assert_eq!(result.session, session);
    assert!(result.members.is_empty());
    assert_eq!(reopened.attempts(None, 16).await?.len(), 1);
    let stored = reopened
        .object(&reopened.db, "attempt", &original.id)
        .await?
        .context("updated row")?;
    assert!(
        serde_json::from_str::<Value>(&stored.object_json)?
            .get("root")
            .is_none()
    );
    // Upgrading mutable state never substitutes for the original ACP proof.
    record_entity::Entity::delete_by_id(&operation.request.record_id)
        .exec(&reopened.db)
        .await?;
    assert!(reopened.active_attempt(&session).await.is_err());
    Ok(())
}

#[tokio::test]
async fn conflicting_old_and_new_session_keys_cannot_select_an_arbitrary_task() -> Result<()> {
    let store = store().await?;
    let session = root("public");
    let journal = journal(&store, "controller", &session).await?;
    journal.append(request("one", "public")).await?;
    let mut duplicate = store
        .active_task(&store.db, &session)
        .await?
        .context("task")?;
    duplicate.id = legacy_session_id(&session)?;
    store
        .insert_object(
            &store.db,
            "active_task",
            &duplicate.id,
            i64::try_from(duplicate.revision)?,
            &duplicate,
        )
        .await?;
    assert!(store.active_attempt(&session).await.is_err());
    assert!(journal.append(response("one")).await.is_err());
    Ok(())
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
async fn tasks_stored_before_workspace_checkpoints_remain_readable_and_writable() -> Result<()> {
    let store = store().await?;
    let node = root("session");
    let journal = journal(&store, "controller", &node).await?;
    journal.append(request("origin", "session")).await?;
    journal.append(response("origin")).await?;
    let original = active(&store, &node).await?;
    let task = store.active_task(&store.db, &node).await?.context("task")?;
    // Frozen e46946a4 object shape, with an actual pre-field digest. A round
    // trip of today's type would not exercise compatibility with this row.
    let legacy = json!({"id":task.id,"revision":task.revision,"root": {"namespace":task.session.namespace,"harness":task.session.harness,"native_id":task.session.session_id,"agent_id":null},
        "attempt_id":task.attempt_id,"origin_operation":task.origin_operation,
        "operations":task.operations,"open_operations":task.open_operations});
    let row = store
        .object(&store.db, "active_task", &task.id)
        .await?
        .context("stored task")?;
    object_entity::Entity::update_many()
        .col_expr(
            object_entity::Column::ObjectJson,
            Expr::value(serde_json::to_string(&legacy)?),
        )
        .col_expr(
            object_entity::Column::Digest,
            Expr::value(canonical_digest(&legacy)?),
        )
        .filter(object_entity::Column::Id.eq(row.id))
        .exec(&store.db)
        .await?;
    assert_eq!(active(&store, &node).await?, original);
    assert!(
        store
            .workspace_evidence(&node)
            .await?
            .gaps
            .contains("workspace_baseline_unavailable")
    );
    journal.append(request("continued", "session")).await?;
    journal.append(response("continued")).await?;
    let continued = active(&store, &node).await?;
    assert_eq!(continued.id, original.id);
    assert_eq!(continued.phase, AttemptPhase::Settling);
    assert_eq!(
        store
            .active_task(&store.db, &node)
            .await?
            .context("continued task")?
            .last_response,
        Some(PromptOperation::key("controller", "continued")?)
    );
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
    for workspace in [false, true] {
        concurrent_prompts(workspace).await?;
    }
    Ok(())
}

async fn concurrent_prompts(workspace: bool) -> Result<()> {
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
    let mut event = request("one", "session");
    let mut result = response("one");
    if workspace {
        let artifact =
            crate::session_evidence::workspace::capture(None, BTreeSet::new(), BTreeSet::new())
                .await?;
        let id = first_store.save_workspace(artifact).await?;
        event["workspace_artifact"] = json!(id);
        result["workspace_artifact"] = json!(id);
    }
    let (a, b) = tokio::join!(first.append(event.clone()), second.append(event));
    a?;
    b?;
    assert_eq!(first_store.attempts(None, 16).await?.len(), 1);
    assert!(second_store.has_unobserved_prompts(&node, "second").await?);
    second.append(result.clone()).await?;
    assert_eq!(
        active(&second_store, &node).await?.phase,
        AttemptPhase::Collecting
    );
    assert!(second_store.has_unobserved_prompts(&node, "second").await?);
    // The origin can still finish its RPC; another controller never invents
    // that response merely because its own prompt ended.
    first.append(result).await?;
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
    let transaction = reader.read_snapshot().await?;
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

async fn journal(store: &EvidenceStore, controller: &str, root: &AcpSessionKey) -> Result<Journal> {
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

async fn active(store: &EvidenceStore, root: &AcpSessionKey) -> Result<Attempt> {
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
