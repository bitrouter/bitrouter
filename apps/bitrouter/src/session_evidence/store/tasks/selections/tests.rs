use super::super::tests::{journal, request, response, root, store};
use super::*;
use serde_json::json;

mod boundaries;

fn select(id: &str, expected: TaskCursor, mode: TaskSelectionMode) -> Value {
    json!({"method":"_bitrouter/task/select","operation_id":id,"phase":"request","native_scope":"session",
        "observed_at":"2026-09-08T00:00:00Z","payload":TaskSelectRequest {session_id:"public".into(), request_id:id.into(), expected, mode}})
}

#[tokio::test]
async fn multi_generation_retry_reads_reject_missing_intermediate_evidence() -> Result<()> {
    for damaged in ["selection", "response", "archive"] {
        let store = store().await?;
        let session = root("public");
        let journal = journal(&store, "controller", &session).await?;
        journal.append(request("a", "public")).await?;
        journal.append(response("a")).await?;
        let mut intermediate_selection = None;
        let mut intermediate_attempt = None;
        for operation in ["b", "c"] {
            let expected = store
                .task_status(&session)
                .await?
                .current
                .context("cursor")?;
            journal
                .append(select(operation, expected, TaskSelectionMode::Retry))
                .await?;
            journal.append(request(operation, "public")).await?;
            journal.append(response(operation)).await?;
            if operation == "b" {
                let task = store
                    .active_task(&store.db, &session)
                    .await?
                    .context("intermediate task")?;
                intermediate_selection = task.selection;
                intermediate_attempt = Some(task.attempt_id);
            }
        }
        assert!(store.active_attempt(&session).await?.is_some());
        match damaged {
            "selection" => {
                let selection = store
                    .selection_on(
                        &store.db,
                        &intermediate_selection.context("middle selection")?,
                    )
                    .await?
                    .context("selection")?;
                record_entity::Entity::delete_by_id(selection.record.record_id)
                    .exec(&store.db)
                    .await?;
            }
            "response" => {
                let operation = store
                    .prompt_operation(&store.db, &PromptOperation::key("controller", "b")?)
                    .await?
                    .context("middle prompt")?;
                record_entity::Entity::delete_by_id(
                    operation.response.context("response")?.record_id,
                )
                .exec(&store.db)
                .await?;
            }
            _ => {
                object_entity::Entity::delete_by_id(store.object_id(
                    "task_archive",
                    &intermediate_attempt.context("middle attempt")?,
                )?)
                .exec(&store.db)
                .await?;
            }
        }
        assert!(store.active_attempt(&session).await.is_err(), "{damaged}");
        assert!(store.task_status(&session).await.is_err(), "{damaged}");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires a dedicated PostgreSQL database in BITROUTER_TEST_POSTGRES_URL"]
async fn postgres_selection_and_prompt_wait_on_the_same_task_row() -> Result<()> {
    let url =
        std::env::var("BITROUTER_TEST_POSTGRES_URL").context("dedicated PostgreSQL fixture URL")?;
    ensure!(
        url.starts_with("postgres://") || url.starts_with("postgresql://"),
        "PostgreSQL fixture required"
    );
    let admin = crate::db::connect(&url).await?;
    crate::db::run_migrations(&admin).await?;
    let owner = uuid::Uuid::new_v4().to_string();
    let store = EvidenceStore::new(crate::db::connect(&url).await?, &owner)?;
    let other = EvidenceStore::new(crate::db::connect(&url).await?, &owner)?;
    let session = root("public");
    let first = journal(&store, "first", &session).await?;
    let second = journal(&other, "second", &session).await?;
    first.append(request("one", "public")).await?;
    first.append(response("one")).await?;
    let expected = store
        .task_status(&session)
        .await?
        .current
        .context("cursor")?;
    commit_waiting_on_task(
        &admin,
        &store,
        &session,
        first.append(select(
            "selected",
            expected.clone(),
            TaskSelectionMode::Retry,
        )),
        second.append(request("two", "public")),
    )
    .await?;
    let next = store.task_status(&session).await?;
    assert!(next.pending.is_none());
    let next = next.current.context("new attempt")?;
    assert_ne!(next.attempt_id, expected.attempt_id);
    assert_eq!(next.task_id, expected.task_id);

    // Prompt admission and an older response must acquire active-task before
    // attempt. The former attempt-first response path could deadlock here.
    commit_waiting_on_task(
        &admin,
        &store,
        &session,
        first.append(request("three", "public")),
        second.append(response("two")),
    )
    .await?;
    let task = store
        .active_task(&store.db, &session)
        .await?
        .context("task")?;
    assert_eq!(
        task.open_operations,
        BTreeSet::from([PromptOperation::key("first", "three")?])
    );
    first.append(response("three")).await?;
    let expected = store
        .task_status(&session)
        .await?
        .current
        .context("cursor")?;

    // Both controllers see no selection before blocking. After the first
    // commits, the second must recheck the durable key and return success.
    commit_waiting_on_task(
        &admin,
        &store,
        &session,
        first.append(select(
            "same-key",
            expected.clone(),
            TaskSelectionMode::Retry,
        )),
        second.append(select("same-key", expected, TaskSelectionMode::Retry)),
    )
    .await?;
    assert_eq!(
        store
            .task_status(&session)
            .await?
            .pending
            .context("pending")?
            .request_id,
        "same-key"
    );
    second.append(request("four", "public")).await?;
    second.append(response("four")).await?;
    assert!(store.task_status(&session).await?.pending.is_none());
    assert_eq!(store.attempts(None, 16).await?.len(), 3);
    Ok(())
}

async fn commit_waiting_on_task(
    admin: &DatabaseConnection,
    store: &EvidenceStore,
    session: &AcpSessionKey,
    first: impl std::future::Future<Output = Result<()>>,
    second: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    let transaction = admin.begin().await?;
    transaction
        .execute(sea_orm::Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE native_evidence_objects SET revision = revision WHERE id = $1",
            [store.object_id("active_task", &session.id()?)?.into()],
        ))
        .await?;
    let holder = transaction
        .query_one(sea_orm::Statement::from_string(
            DbBackend::Postgres,
            "SELECT pg_backend_pid() AS pid".to_owned(),
        ))
        .await?
        .context("holder pid")?
        .try_get::<i32>("", "pid")?;
    let mut first = Box::pin(first);
    let mut second = Box::pin(second);
    // Verify real lock waits, rather than relying on sleeps to create a race.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            tokio::select! {
                result = &mut first => { result?; anyhow::bail!("first operation bypassed the locked task row"); }
                rows = admin.query_one(sea_orm::Statement::from_sql_and_values(DbBackend::Postgres,
                    "SELECT count(*) AS count FROM pg_stat_activity WHERE datname = current_database() AND $1 = ANY(pg_blocking_pids(pid))", [holder.into()])) => {
                    if rows?.context("lock wait count")?.try_get::<i64>("", "count")? >= 1 { return anyhow::Ok(()); }
                }
            }
            tokio::task::yield_now().await;
        }
    }).await??;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            tokio::select! {
                result = &mut first => { result?; anyhow::bail!("first operation completed through the held lock"); }
                result = &mut second => { result?; anyhow::bail!("second operation completed through the held lock"); }
                rows = admin.query_one(sea_orm::Statement::from_string(DbBackend::Postgres,
                    "SELECT count(*) AS count FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'".to_owned())) => {
                    if rows?.context("lock wait count")?.try_get::<i64>("", "count")? >= 2 { return anyhow::Ok(()); }
                }
            }
            tokio::task::yield_now().await;
        }
    }).await??;
    transaction.commit().await?;
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(first, second)
    })
    .await?;
    first?;
    second?;
    Ok(())
}

#[tokio::test]
async fn selections_preserve_retry_task_identity_and_archive_each_original_attempt() -> Result<()> {
    let store = store().await?;
    let session = root("public");
    let journal = journal(&store, "controller", &session).await?;
    assert!(store.task_status(&session).await?.current.is_none());
    journal.append(request("original", "public")).await?;
    journal.append(response("original")).await?;
    let mut previous = store
        .active_attempt(&session)
        .await?
        .context("first attempt")?;
    for (index, mode) in [
        TaskSelectionMode::Retry,
        TaskSelectionMode::Retry,
        TaskSelectionMode::NewTask,
    ]
    .into_iter()
    .enumerate()
    {
        let old_state = store
            .active_task(&store.db, &session)
            .await?
            .context("old state")?;
        let expected = store
            .task_status(&session)
            .await?
            .current
            .context("cursor")?;
        let id = format!("select-{index}");
        let selection = select(&id, expected.clone(), mode);
        journal.append(selection.clone()).await?;
        journal.append(selection.clone()).await?;
        assert_eq!(
            store
                .task_status(&session)
                .await?
                .pending
                .context("pending")?
                .request_id,
            id
        );
        assert_eq!(
            store
                .active_attempt(&session)
                .await?
                .context("not switched yet")?,
            previous
        );
        let operation = format!("prompt-{index}");
        journal.append(request(&operation, "public")).await?;
        let next = store
            .active_attempt(&session)
            .await?
            .context("new attempt")?;
        assert_ne!(next.id, previous.id);
        assert_eq!(
            next.task_id == previous.task_id,
            mode == TaskSelectionMode::Retry
        );
        assert_eq!(next.phase, AttemptPhase::Collecting);
        assert!(next.members.is_empty());
        assert!(next.latest_manifest.is_none());
        assert!(store.task_status(&session).await?.pending.is_none());
        let archived: ArchivedTask = decode_object(
            store
                .object(&store.db, "task_archive", &previous.id)
                .await?
                .context("archive")?,
        )?;
        assert_eq!(
            serde_json::to_value(&archived.task)?,
            serde_json::to_value(old_state)?
        );
        assert_eq!(
            store.task_attempt(&store.db, &archived.task).await?,
            previous
        );
        // Lost responses may replay across a switch. Original operations and
        // selections remain idempotent, without selecting another attempt.
        journal.append(selection).await?;
        journal.append(request("original", "public")).await?;
        journal.append(response("original")).await?;
        assert_eq!(
            store
                .active_attempt(&session)
                .await?
                .context("same new attempt")?,
            next
        );
        journal.append(response(&operation)).await?;
        previous = store
            .active_attempt(&session)
            .await?
            .context("completed RPC")?;
        assert_eq!(previous.phase, AttemptPhase::Settling);
        assert!(previous.effective_manifest.is_none());
    }
    assert_eq!(store.attempts(None, 16).await?.len(), 4);
    Ok(())
}

#[tokio::test]
async fn busy_stale_and_conflicting_selections_roll_back_the_raw_observation() -> Result<()> {
    let store = store().await?;
    let session = root("public");
    let journal = journal(&store, "controller", &session).await?;
    journal.append(request("one", "public")).await?;
    let busy = store
        .task_status(&session)
        .await?
        .current
        .context("busy cursor")?;
    let source = store.sources(None, 16).await?.remove(0);
    assert!(
        journal
            .append(select("busy", busy.clone(), TaskSelectionMode::Retry))
            .await
            .is_err()
    );
    assert_eq!(
        store
            .source(&source.id)
            .await?
            .context("unchanged source")?,
        source
    );
    journal.append(response("one")).await?;
    assert!(
        journal
            .append(select("stale", busy, TaskSelectionMode::NewTask))
            .await
            .is_err()
    );
    let current = store
        .task_status(&session)
        .await?
        .current
        .context("current cursor")?;
    journal
        .append(select(
            "selected",
            current.clone(),
            TaskSelectionMode::Retry,
        ))
        .await?;
    assert!(
        journal
            .append(select(
                "another",
                current.clone(),
                TaskSelectionMode::NewTask
            ))
            .await
            .is_err()
    );
    assert!(
        journal
            .append(select("selected", current, TaskSelectionMode::NewTask))
            .await
            .is_err()
    );
    assert_eq!(
        store
            .task_status(&session)
            .await?
            .pending
            .context("original choice")?
            .mode,
        TaskSelectionMode::Retry
    );
    Ok(())
}

#[tokio::test]
async fn selection_survives_reopen_and_a_different_controller_consumes_it_once() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite:{}?mode=rwc",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db.clone(), "alice")?;
    let session = root("public");
    let first = journal(&store, "first", &session).await?;
    first.append(request("one", "public")).await?;
    first.append(response("one")).await?;
    let original = store
        .task_status(&session)
        .await?
        .current
        .context("original")?;
    let choice = select("selected", original.clone(), TaskSelectionMode::Retry);
    first.append(choice.clone()).await?;
    drop(first);
    drop(store);
    db.close().await?;
    let store = EvidenceStore::new(crate::db::connect(&url).await?, "alice")?;
    let resumed = journal(&store, "resumed", &session).await?;
    resumed.append(choice).await?;
    resumed.append(request("two", "public")).await?;
    let current = store
        .task_status(&session)
        .await?
        .current
        .context("resumed")?;
    assert_eq!(current.task_id, original.task_id);
    assert_ne!(current.attempt_id, original.attempt_id);
    assert!(store.task_status(&session).await?.pending.is_none());
    let task = store
        .active_task(&store.db, &session)
        .await?
        .context("task")?;
    let selection = store
        .selection_on(&store.db, task.selection.as_ref().context("selection")?)
        .await?
        .context("selection body")?;
    record_entity::Entity::delete_by_id(selection.record.record_id)
        .exec(&store.db)
        .await?;
    assert!(store.task_status(&session).await.is_err());
    assert!(store.active_attempt(&session).await.is_err());
    Ok(())
}

#[tokio::test]
async fn failed_selection_consumption_rolls_back_archive_pointer_and_prompt_together() -> Result<()>
{
    let store = store().await?;
    let session = root("public");
    let journal = journal(&store, "controller", &session).await?;
    journal.append(request("one", "public")).await?;
    journal.append(response("one")).await?;
    let expected = store
        .task_status(&session)
        .await?
        .current
        .context("cursor")?;
    journal
        .append(select(
            "selected",
            expected.clone(),
            TaskSelectionMode::NewTask,
        ))
        .await?;
    let source = store.sources(None, 16).await?.remove(0);
    let operation = PromptOperation::key("controller", "two")?;
    let id = canonical_digest(&("attempt", operation))?;
    store
        .create_attempt(&Attempt {
            id: id.clone(),
            task_id: "conflict".into(),
            session: session.clone(),
            members: BTreeSet::new(),
            phase: AttemptPhase::Collecting,
            revision: 0,
            latest_manifest: None,
            effective_manifest: None,
            started_at: "2026-09-08T00:00:00Z".into(),
        })
        .await?;
    assert!(journal.append(request("two", "public")).await.is_err());
    assert_eq!(
        store
            .source(&source.id)
            .await?
            .context("unchanged source")?,
        source
    );
    assert_eq!(
        store.task_status(&session).await?.current,
        Some(expected.clone())
    );
    assert!(store.task_status(&session).await?.pending.is_some());
    assert!(
        store
            .object(&store.db, "task_archive", &expected.attempt_id)
            .await?
            .is_none()
    );
    object_entity::Entity::delete_by_id(store.object_id("attempt", &id)?)
        .exec(&store.db)
        .await?;
    journal.append(request("two", "public")).await?;
    assert_ne!(
        store
            .task_status(&session)
            .await?
            .current
            .context("new cursor")?
            .attempt_id,
        expected.attempt_id
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_controllers_cannot_overwrite_a_selection_from_the_same_cursor() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite:{}?mode=rwc",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db, "alice")?;
    let other = EvidenceStore::new(crate::db::connect(&url).await?, "alice")?;
    let session = root("public");
    let first = journal(&store, "first", &session).await?;
    let second = journal(&other, "second", &session).await?;
    first.append(request("one", "public")).await?;
    first.append(response("one")).await?;
    let cursor = store
        .task_status(&session)
        .await?
        .current
        .context("cursor")?;
    let (a, b) = tokio::join!(
        first.append(select("a", cursor.clone(), TaskSelectionMode::Retry)),
        second.append(select("b", cursor, TaskSelectionMode::NewTask))
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let selection = store
        .task_status(&session)
        .await?
        .pending
        .context("winner")?;
    assert_eq!(selection.request_id, if a.is_ok() { "a" } else { "b" });
    let (a, b) = tokio::join!(
        first.append(request("next-a", "public")),
        second.append(request("next-b", "public"))
    );
    a?;
    b?;
    assert!(store.task_status(&session).await?.pending.is_none());
    assert_eq!(store.attempts(None, 16).await?.len(), 2);
    let task = store
        .active_task(&store.db, &session)
        .await?
        .context("new task")?;
    assert_eq!(task.open_operations.len(), 2);
    Ok(())
}
