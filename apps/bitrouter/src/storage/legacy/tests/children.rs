//! Isolate each child predicate while its session root remains Legacy.
use super::*;
use sea_orm::TransactionTrait;

const FIELDS: [&str; 3] = ["installation_id", "tenant_id", "creator_principal_id"];

async fn one_field(db: &DatabaseConnection, table: &str, column: &str) -> Result<()> {
    let update = Query::update()
        .table(Alias::new(table))
        .value(Alias::new(column), "marked")
        .to_owned();
    ensure!(
        db.execute(db.get_database_backend().build(&update))
            .await?
            .rows_affected()
            > 0
    );
    Ok(())
}

#[tokio::test]
async fn marked_children_of_legacy_root_are_excluded_before_decode_and_delete() -> Result<()> {
    for field in FIELDS {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let canonical = CanonicalStore::new(db.clone());
        let recorder = recorded(&db, "child-boundary").await?;
        let id = identity("child-boundary");
        let key = id.key()?;
        let session = sessions::Entity::find_by_id(&key)
            .one(&db)
            .await?
            .context("session")?;
        let cp = canonical.freeze_checkpoint(&id, session.head).await?;
        canonical
            .submit_assessment(&id, label("first", &cp.checkpoint_id))
            .await?;
        canonical
            .observe_checkpoint_resources(&id, &cp.checkpoint_id)
            .await?;
        // The Legacy root stays readable throughout; child rows alone change scope.
        one_field(&db, "acp_resource_observations", field).await?;
        poison(
            &db,
            "acp_resource_observations",
            "checkpoint_id",
            &cp.checkpoint_id,
            "observation_json",
        )
        .await?;
        ensure!(
            canonical
                .checkpoint_resource_history(&id, &cp.checkpoint_id)
                .await?
                .is_empty()
        );
        one_field(&db, "acp_assessment_revisions", field).await?;
        poison(
            &db,
            "acp_assessment_revisions",
            "checkpoint_id",
            &cp.checkpoint_id,
            "revision_json",
        )
        .await?;
        ensure!(canonical.assessment_history(&id).await?.is_empty());
        ensure!(canonical.effective_assessment(&id).await.is_err());
        one_field(&db, "acp_effective_assessments", field).await?;
        let before_revisions = revisions::Entity::find().all(&db).await?;
        let before_head = heads::Entity::find_by_id(&key).one(&db).await?;
        // A marked head is absent to Legacy reads, but cannot be overwritten by
        // the old head upsert. Its occupied key aborts the whole new revision.
        ensure!(
            canonical
                .effective_assessment(&id)
                .await?
                .current_revision
                .is_none()
        );
        ensure!(
            canonical
                .submit_assessment(&id, label("collision", &cp.checkpoint_id))
                .await
                .is_err()
        );
        ensure!(revisions::Entity::find().all(&db).await? == before_revisions);
        ensure!(heads::Entity::find_by_id(&key).one(&db).await? == before_head);
        one_field(&db, "acp_checkpoints", field).await?;
        poison(
            &db,
            "acp_checkpoints",
            "checkpoint_id",
            &cp.checkpoint_id,
            "manifest_json",
        )
        .await?;
        ensure!(canonical.checkpoints(&id).await?.is_empty());
        ensure!(
            canonical
                .checkpoint_content(&id, &cp.checkpoint_id)
                .await
                .is_err()
        );
        one_field(&db, "acp_capture_events", field).await?;
        poison(
            &db,
            "acp_capture_events",
            "connection_id",
            recorder.connection_id(),
            "event_json",
        )
        .await?;
        let transcript = canonical.transcript(&id).await?;
        ensure!(
            transcript.events.is_empty()
                && transcript.replay_events.is_empty()
                && transcript.setup_evidence.is_empty()
        );
        ensure!(
            canonical
                .list("fixture-owner", "fixture-source")
                .await?
                .len()
                == 1
        );
        // A Legacy member pointing at a marked checkpoint cannot authorize
        // cross-family mutation. Even the root tombstone must roll back.
        ensure!(canonical.delete(&id).await.is_err());
        ensure!(sessions::Entity::find_by_id(&key).one(&db).await? == Some(session));
        one_field(&db, "acp_checkpoint_members", field).await?;
        let before_cp = checkpoints::Entity::find_by_id(&cp.checkpoint_id)
            .one(&db)
            .await?;
        let before_members = members::Entity::find().all(&db).await?;
        let before_resources = resources::Entity::find().all(&db).await?;
        let before_events = events::Entity::find()
            .order_by_asc(events::Column::Sequence)
            .all(&db)
            .await?;
        canonical.delete(&id).await?;
        ensure!(
            sessions::Entity::find_by_id(&key)
                .one(&db)
                .await?
                .context("tombstone")?
                .deleted
        );
        ensure!(
            checkpoints::Entity::find_by_id(&cp.checkpoint_id)
                .one(&db)
                .await?
                == before_cp
        );
        ensure!(members::Entity::find().all(&db).await? == before_members);
        ensure!(resources::Entity::find().all(&db).await? == before_resources);
        ensure!(revisions::Entity::find().all(&db).await? == before_revisions);
        ensure!(heads::Entity::find_by_id(&key).one(&db).await? == before_head);
        ensure!(
            events::Entity::find()
                .order_by_asc(events::Column::Sequence)
                .all(&db)
                .await?
                == before_events
        );
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn marked_roots_and_evolution_conflicts_never_mutate_or_decode() -> Result<()> {
    for field in FIELDS {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let canonical = CanonicalStore::new(db.clone());
        let recorder = recorded(&db, "parent-boundary").await?;
        let id = identity("parent-boundary");
        let key = id.key()?;
        one_field(&db, "acp_canonical_sessions", field).await?;
        let session = sessions::Entity::find_by_id(&key).one(&db).await?;
        let event_rows = events::Entity::find()
            .order_by_asc(events::Column::Sequence)
            .all(&db)
            .await?;
        ensure!(
            canonical
                .list("fixture-owner", "fixture-source")
                .await?
                .is_empty()
        );
        canonical.delete(&id).await?;
        ensure!(sessions::Entity::find_by_id(&key).one(&db).await? == session);
        ensure!(
            events::Entity::find()
                .order_by_asc(events::Column::Sequence)
                .all(&db)
                .await?
                == event_rows
        );
        // The real recorder's session insert hits the occupied marked key; no
        // self-update and no foreign session head/replay byte may change.
        ensure!(
            recorder
                .record(event(
                    CaptureKind::Notification,
                    None,
                    "session/update",
                    json!({"sessionId":"parent-boundary"})
                ))
                .await
                .is_err()
        );
        ensure!(sessions::Entity::find_by_id(&key).one(&db).await? == session);
        ensure!(
            events::Entity::find()
                .order_by_asc(events::Column::Sequence)
                .all(&db)
                .await?
                == event_rows
        );
        let other = recorded(&db, "connection-boundary").await?;
        let update = Query::update()
            .table(Alias::new("acp_capture_connections"))
            .value(Alias::new(field), "marked")
            .and_where(Expr::col(Alias::new("connection_id")).eq(other.connection_id()))
            .to_owned();
        db.execute(db.get_database_backend().build(&update)).await?;
        let connection = connections::Entity::find_by_id(other.connection_id())
            .one(&db)
            .await?;
        ensure!(
            other
                .record(event(
                    CaptureKind::Notification,
                    None,
                    "session/update",
                    json!({"sessionId":"connection-boundary"})
                ))
                .await
                .is_err()
        );
        ensure!(
            connections::Entity::find_by_id(other.connection_id())
                .one(&db)
                .await?
                == connection
        );
        let store = EvolutionStore::new(db.clone(), "fixture-owner")?;
        store
            .initialize("isolated", "key", None, &json!({"private":true}))
            .await?;
        let record_id = store.id("isolated", "key")?;
        // Mark only this row; unrelated coverage records remain real Legacy data.
        let update = Query::update()
            .table(Alias::new("checkpoint_evolution"))
            .value(Alias::new(field), "marked")
            .and_where(Expr::col(Alias::new("record_id")).eq(&record_id))
            .to_owned();
        db.execute(db.get_database_backend().build(&update)).await?;
        poison(&db, "checkpoint_evolution", "record_id", &record_id, "body").await?;
        let row = records::Entity::find_by_id(&record_id).one(&db).await?;
        ensure!(
            store
                .get::<serde_json::Value>("isolated", "key")
                .await?
                .is_none()
        );
        ensure!(
            store
                .list::<serde_json::Value>("isolated")
                .await?
                .is_empty()
        );
        ensure!(
            store
                .initialize("isolated", "key", None, &json!({"replace":true}))
                .await
                .is_err()
        );
        ensure!(
            store
                .update::<serde_json::Value, ()>("isolated", "key", 0, |_| anyhow::bail!(
                    "must never decode or invoke"
                ))
                .await
                .is_err()
        );
        let tx = db.begin().await?;
        ensure!(
            store
                .get_current_in::<serde_json::Value>(&tx, "isolated", "key")
                .await?
                .is_none()
        );
        ensure!(
            store
                .lock::<serde_json::Value>(&tx, "isolated", "key")
                .await
                .is_err()
        );
        ensure!(
            store
                .save(
                    &tx,
                    row.clone().context("marked record")?,
                    &json!({"forged":true})
                )
                .await
                .is_err()
        );
        ensure!(records::Entity::find_by_id(&record_id).one(&tx).await? == row);
        tx.rollback().await?;
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn suppressed_inserts_never_acknowledge_capture_or_select_a_missing_revision() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let canonical = CanonicalStore::new(db.clone());
    let recorder = recorded(&db, "suppressed").await?;
    let id = identity("suppressed");
    let session = sessions::Entity::find_by_id(id.key()?)
        .one(&db)
        .await?
        .context("session")?;
    let cp = canonical.freeze_checkpoint(&id, session.head).await?;
    // A non-returning insert still has to prove that its row was inserted.
    // https://www.sqlite.org/lang_createtrigger.html#the_raise_function
    db.execute_unprepared("CREATE TRIGGER fixture_suppress_revision BEFORE INSERT ON acp_assessment_revisions BEGIN SELECT RAISE(IGNORE); END").await?;
    let error = canonical
        .submit_assessment(&id, label("suppressed", &cp.checkpoint_id))
        .await
        .err()
        .context("missing revision accepted")?;
    ensure!(format!("{error:#}").contains("revisions row was not inserted"));
    ensure!(revisions::Entity::find().all(&db).await?.is_empty());
    ensure!(heads::Entity::find().all(&db).await?.is_empty());
    db.execute_unprepared("DROP TRIGGER fixture_suppress_revision")
        .await?;
    ensure!(
        canonical
            .submit_assessment(&id, label("suppressed", &cp.checkpoint_id))
            .await?
            .selected_on_submission
    );
    let connection = connections::Entity::find_by_id(recorder.connection_id())
        .one(&db)
        .await?
        .context("connection")?;
    let before = events::Entity::find()
        .order_by_asc(events::Column::Sequence)
        .all(&db)
        .await?;
    db.execute_unprepared("CREATE TRIGGER fixture_suppress_event BEFORE INSERT ON acp_capture_events BEGIN SELECT RAISE(IGNORE); END").await?;
    ensure!(
        recorder
            .record(event(
                CaptureKind::Notification,
                None,
                "session/update",
                json!({"sessionId":"suppressed"})
            ))
            .await
            .is_err()
    );
    ensure!(sessions::Entity::find_by_id(id.key()?).one(&db).await? == Some(session));
    let after = connections::Entity::find_by_id(recorder.connection_id())
        .one(&db)
        .await?
        .context("connection")?;
    ensure!(after.head == connection.head && after.state == "interrupted");
    ensure!(
        events::Entity::find()
            .order_by_asc(events::Column::Sequence)
            .all(&db)
            .await?
            == before
    );
    db.close().await?;
    Ok(())
}
