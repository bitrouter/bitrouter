use super::*;
use crate::session_evidence::adapter_bridge::AdapterIdentity;
use crate::session_evidence::journal::Journal;
use serde_json::json;

fn adapter(harness: Harness) -> Result<AdapterIdentity> {
    let pins: Value = serde_json::from_str(include_str!("../../../adapter_bridge/pins.json"))?;
    let key = if harness == Harness::Codex {
        "codex"
    } else {
        "claude"
    };
    serde_json::from_value(json!({"package":pins[key]["package"],"version":pins[key]["version"],"moduleDigest":pins[key]["moduleDigest"]})).map_err(Into::into)
}

async fn journal(
    store: &EvidenceStore,
    controller: &str,
    session: &AcpSessionKey,
) -> Result<Journal> {
    let adapter = adapter(session.harness)?;
    Journal::new(
        store.clone(),
        SourceDescriptor {
            namespace: session.namespace.clone(),
            harness: session.harness,
            format: SourceFormat::Acp,
            locator: format!("controller:{controller}"),
            node: None,
        },
        format!("{}@{}", adapter.package, adapter.version),
    )
    .await
}

fn request(id: &str, session: &AcpSessionKey) -> Value {
    json!({"operation_id":id,"method":"session/prompt","phase":"request","native_scope":"session","observed_at":"2026-09-08T00:00:00Z","payload":{"sessionId":session.session_id,"prompt":[{"type":"text","text":"same prompt"}]}})
}

fn notification(origin: &PromptOrigin, sequence: u32, event: Event) -> Result<Value> {
    Ok(
        json!({"operation_id":format!("notification-{sequence}"),"method":adapter_bridge::METHOD,"phase":"notification","native_scope":"session","payload":Observation { schema:1, session_id:origin.session.session_id.clone(), origin:origin.clone(), adapter:adapter(origin.session.harness)?, sequence, event }}),
    )
}

#[tokio::test]
async fn indexed_producer_bindings_survive_reopen_and_recheck_their_original_records() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db, "alice")?;
    let session = AcpSessionKey {
        namespace: "profile".into(),
        harness: Harness::Codex,
        session_id: "acp-conversation".into(),
    };
    let source = journal(&store, "controller", &session).await?;
    source.append(request("prompt", &session)).await?;
    let attempt_id = store.active_attempt(&session).await?.context("attempt")?.id;
    let origin = store
        .prompt_origin("controller", "prompt")
        .await?
        .context("origin")?;
    source
        .append(notification(&origin, 0, Event::Started)?)
        .await?;
    let accepted = source
        .append_record(notification(
            &origin,
            1,
            Event::CodexAccepted {
                thread_id: "native-thread".into(),
                turn_id: "native-turn".into(),
                role: "prompt".into(),
            },
        )?)
        .await?;
    source
        .append(notification(
            &origin,
            2,
            Event::Finished {
                outcome: "returned".into(),
                notification_failures: 0,
            },
        )?)
        .await?;
    let before = store.prompt_bridge_evidence(&session, &attempt_id).await?;
    assert!(before.gaps.is_empty());
    assert_eq!(before.observations.len(), 3);
    assert!(
        store
            .active_attempt(&session)
            .await?
            .context("attempt")?
            .members
            .is_empty()
    );
    drop(source);
    drop(store);
    let reopened = EvidenceStore::new(crate::db::connect(&url).await?, "alice")?;
    let after = reopened
        .prompt_bridge_evidence(&session, &attempt_id)
        .await?;
    assert_eq!(
        serde_json::to_value(&before)?,
        serde_json::to_value(&after)?
    );
    let foreign = EvidenceStore::new(reopened.db.clone(), "bob")?;
    assert!(
        foreign
            .prompt_origin("controller", "prompt")
            .await?
            .is_none()
    );
    assert!(
        foreign
            .prompt_bridge_evidence(&session, &attempt_id)
            .await?
            .observations
            .is_empty()
    );
    record_entity::Entity::delete_many()
        .filter(record_entity::Column::Id.eq(&accepted.id))
        .exec(&reopened.db)
        .await?;
    assert!(
        reopened
            .prompt_bridge_evidence(&session, &attempt_id)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn foreign_and_corrupt_origins_stay_raw_and_cannot_fill_a_missing_sequence() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db, "alice")?;
    let session = AcpSessionKey {
        namespace: "profile".into(),
        harness: Harness::ClaudeCode,
        session_id: "acp-conversation".into(),
    };
    let source = journal(&store, "controller", &session).await?;
    source.append(request("prompt", &session)).await?;
    let attempt_id = store.active_attempt(&session).await?.context("attempt")?.id;
    let origin = store
        .prompt_origin("controller", "prompt")
        .await?
        .context("origin")?;
    source
        .append(notification(&origin, 0, Event::Started)?)
        .await?;
    let another = journal(&store, "other-controller", &session).await?;
    let foreign = another
        .append_record(notification(
            &origin,
            1,
            Event::ClaudeEnqueued {
                command_id: "command".into(),
            },
        )?)
        .await?;
    let mut corrupt = origin.clone();
    corrupt.request.record_digest = "a".repeat(64);
    let invalid = source
        .append_record(notification(
            &corrupt,
            1,
            Event::ClaudeEnqueued {
                command_id: "command".into(),
            },
        )?)
        .await?;
    source
        .append(notification(
            &origin,
            2,
            Event::Finished {
                outcome: "returned".into(),
                notification_failures: 0,
            },
        )?)
        .await?;
    let evidence = store.prompt_bridge_evidence(&session, &attempt_id).await?;
    assert_eq!(evidence.observations.len(), 2);
    assert!(evidence.gaps.contains("native_bridge_sequence_gap"));
    for record in [foreign, invalid] {
        assert_eq!(
            store
                .records(&RecordRef::from_record(&record)?.range)
                .await?
                .len(),
            1
        );
    }
    source
        .append(notification(
            &origin,
            1,
            Event::ClaudeEnqueued {
                command_id: "command".into(),
            },
        )?)
        .await?;
    let evidence = store.prompt_bridge_evidence(&session, &attempt_id).await?;
    assert_eq!(evidence.observations.len(), 3);
    assert!(!evidence.gaps.contains("native_bridge_sequence_gap"));
    // A repeated producer sequence is not proof of another execution.
    source
        .append(notification(
            &origin,
            1,
            Event::ClaudeEnqueued {
                command_id: "different-command".into(),
            },
        )?)
        .await?;
    assert!(
        store
            .prompt_bridge_evidence(&session, &attempt_id)
            .await?
            .gaps
            .contains("native_bridge_sequence_conflict")
    );
    record_entity::Entity::delete_many()
        .filter(record_entity::Column::Id.eq(&origin.request.record_id))
        .exec(&store.db)
        .await?;
    assert!(store.prompt_origin("controller", "prompt").await.is_err());
    assert!(
        store
            .prompt_bridge_evidence(&session, &attempt_id)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn damaged_bridge_index_preserves_raw_notifications_and_durable_gap() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("evidence.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db, "alice")?;
    let session = AcpSessionKey {
        namespace: "profile".into(),
        harness: Harness::Codex,
        session_id: "acp".into(),
    };
    let source = journal(&store, "controller", &session).await?;
    source.append(request("prompt", &session)).await?;
    let attempt = store.active_attempt(&session).await?.context("attempt")?;
    let origin = store
        .prompt_origin("controller", "prompt")
        .await?
        .context("origin")?;
    source
        .append(notification(&origin, 0, Event::Started)?)
        .await?;
    let key = PromptOperation::key("controller", "prompt")?;
    let old = store
        .object(&store.db, "prompt_bridge", &key)
        .await?
        .context("index")?;
    object_entity::Entity::update_many()
        .col_expr(object_entity::Column::Digest, Expr::value("damaged"))
        .filter(object_entity::Column::Id.eq(&old.id))
        .exec(&store.db)
        .await?;
    let accepted = source
        .append_record(notification(
            &origin,
            1,
            Event::CodexAccepted {
                thread_id: "thread".into(),
                turn_id: "turn".into(),
                role: "prompt".into(),
            },
        )?)
        .await?;
    let finished = source
        .append_record(notification(
            &origin,
            2,
            Event::Finished {
                outcome: "returned".into(),
                notification_failures: 0,
            },
        )?)
        .await?;
    assert!(
        store
            .prompt_bridge_evidence(&session, &attempt.id)
            .await
            .is_err()
    );
    // Repairing a derived row alone cannot erase the missed observations.
    object_entity::Entity::update_many()
        .col_expr(object_entity::Column::Digest, Expr::value(old.digest))
        .filter(object_entity::Column::Id.eq(old.id))
        .exec(&store.db)
        .await?;
    drop(source);
    drop(store);
    let reopened = EvidenceStore::new(crate::db::connect(&url).await?, "alice")?;
    for record in [accepted, finished] {
        let stored = reopened
            .records(&RecordRef::from_record(&record)?.range)
            .await?;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].digest, record.digest);
    }
    let evidence = reopened
        .prompt_bridge_evidence(&session, &attempt.id)
        .await?;
    assert!(evidence.gaps.contains("native_bridge_index_failed"));
    assert!(
        evidence
            .gaps
            .contains("native_bridge_prompt_outcome_unobserved")
    );
    assert_eq!(evidence.observations.len(), 1);
    Ok(())
}

#[tokio::test]
async fn bridge_read_rejects_an_attempt_switched_after_status_was_read() -> Result<()> {
    use bitrouter_sdk::acp::controller::tasks::{TaskSelectRequest, TaskSelectionMode};

    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = EvidenceStore::new(db, "alice")?;
    let session = AcpSessionKey {
        namespace: "profile".into(),
        harness: Harness::Codex,
        session_id: "acp".into(),
    };
    let source = journal(&store, "controller", &session).await?;
    source.append(request("first", &session)).await?;
    source.append(json!({"operation_id":"first", "method":"session/prompt", "phase":"response", "native_scope":"session", "payload":{"stopReason":"end_turn"}})).await?;
    let expected = store
        .task_status(&session)
        .await?
        .current
        .context("cursor")?;
    source.append(json!({"operation_id":"selection", "method":"_bitrouter/task/select", "phase":"request", "native_scope":"session", "payload":TaskSelectRequest {
        session_id:session.session_id.clone(), request_id:"selection".into(), expected:expected.clone(), mode:TaskSelectionMode::Retry,
    }})).await?;
    source.append(request("second", &session)).await?;
    let current = store
        .active_attempt(&session)
        .await?
        .context("new attempt")?;
    assert_ne!(current.id, expected.attempt_id);
    assert!(
        store
            .prompt_bridge_evidence(&session, &expected.attempt_id)
            .await
            .is_err()
    );
    let evidence = store.prompt_bridge_evidence(&session, &current.id).await?;
    assert!(evidence.gaps.contains("native_bridge_unobserved"));
    Ok(())
}
