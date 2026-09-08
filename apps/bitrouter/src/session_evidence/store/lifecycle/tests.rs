use super::*;
use crate::session_evidence::journal::Journal;
use serde_json::json;

async fn store() -> Result<EvidenceStore> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    EvidenceStore::new(db, "local")
}

fn descriptor(harness: Harness, namespace: &str, controller: &str) -> SourceDescriptor {
    SourceDescriptor {
        harness,
        namespace: namespace.into(),
        format: SourceFormat::Acp,
        locator: format!("controller:{controller}"),
        node: None,
    }
}

fn event(method: &str, phase: &str, id: &str) -> Value {
    json!({"method":method,"phase":phase,"operation_id":id,"payload":{"sessionId":"session"}})
}

#[tokio::test]
async fn both_harnesses_persist_cross_profile_lifecycle_boundaries_with_raw_records() -> Result<()>
{
    for harness in [Harness::Codex, Harness::ClaudeCode] {
        let store = store().await?;
        let a = Journal::new(
            store.clone(),
            descriptor(harness, "a", "controller"),
            "fixture/1".into(),
        )
        .await?;
        let b = Journal::new(
            store.clone(),
            descriptor(harness, "b", "controller"),
            "fixture/1".into(),
        )
        .await?;
        for method in [
            "session/new",
            "session/load",
            "session/resume",
            "session/fork",
            "session/close",
            "session/delete",
        ] {
            let request = a.append_record(event(method, "request", method)).await?;
            let pending = store.lifecycle_operation("controller", method).await?;
            assert_eq!(
                pending.request.context("committed request")?.record,
                request
            );
            assert!(pending.response.is_none());
            let response = b.append_record(event(method, "response", method)).await?;
            let finished = store.lifecycle_operation("controller", method).await?;
            assert_eq!(
                finished.request.context("original request")?.record,
                request
            );
            assert_eq!(
                finished.response.context("original response")?.record,
                response
            );
        }
        assert!(
            store
                .lifecycle_operation("other-controller", "session/new")
                .await?
                .request
                .is_none()
        );
        let foreign = EvidenceStore::new(store.db.clone(), "another-owner")?;
        assert!(
            foreign
                .lifecycle_operation("controller", "session/new")
                .await?
                .request
                .is_none()
        );
    }
    Ok(())
}

#[tokio::test]
async fn conflicting_live_boundary_rolls_back_record_cursor_and_index_together() -> Result<()> {
    let store = store().await?;
    let desc = descriptor(Harness::ClaudeCode, "a", "controller");
    let journal = Journal::new(store.clone(), desc.clone(), "fixture/1".into()).await?;
    journal
        .append(event("session/new", "request", "operation"))
        .await?;
    let source = store.register(desc.clone()).await?;
    assert!(
        journal
            .append(event("session/fork", "response", "operation"))
            .await
            .is_err()
    );
    assert_eq!(store.register(desc.clone()).await?, source);
    assert!(
        store
            .lifecycle_operation("controller", "operation")
            .await?
            .response
            .is_none()
    );
    let response = journal
        .append_record(event("session/new", "response", "operation"))
        .await?;
    let completed = store.register(desc.clone()).await?;
    assert!(
        journal
            .append(event("session/new", "response", "operation"))
            .await
            .is_err()
    );
    assert_eq!(store.register(desc).await?, completed);
    assert_eq!(
        store
            .lifecycle_operation("controller", "operation")
            .await?
            .response
            .context("first response")?
            .record,
        response
    );
    Ok(())
}

#[tokio::test]
async fn historical_indexing_can_read_response_before_request_and_replay_without_changes()
-> Result<()> {
    let store = store().await?;
    let a = Journal::new(
        store.clone(),
        descriptor(Harness::ClaudeCode, "a", "controller"),
        "fixture/1".into(),
    )
    .await?;
    let b = Journal::new(
        store.clone(),
        descriptor(Harness::ClaudeCode, "b", "controller"),
        "fixture/1".into(),
    )
    .await?;
    let request = a
        .append_record(event("session/load", "request", "load"))
        .await?;
    let response = b
        .append_record(event("session/load", "response", "load"))
        .await?;
    // Simulate an older database with raw observations but no lifecycle index.
    object_entity::Entity::delete_many()
        .filter(object_entity::Column::Kind.is_in(["lifecycle_request", "lifecycle_response"]))
        .exec(&store.db)
        .await?;
    for record in [&response, &request, &response, &request] {
        assert!(
            store
                .index_lifecycle_range(&RecordRef::from_record(record)?.range)
                .await?
                .is_empty()
        );
        if record.id == response.id {
            assert!(
                store
                    .lifecycle_operation("controller", "load")
                    .await?
                    .response
                    .is_some()
            );
        }
    }
    let found = store.lifecycle_operation("controller", "load").await?;
    assert_eq!(found.request.context("recovered request")?.record, request);
    assert_eq!(
        found.response.context("recovered response")?.record,
        response
    );
    Ok(())
}

#[tokio::test]
async fn lifecycle_reads_recheck_each_half_and_its_source() -> Result<()> {
    for fault in ["request", "response", "source", "index"] {
        let store = store().await?;
        let journal = Journal::new(
            store.clone(),
            descriptor(Harness::Codex, "a", "controller"),
            "fixture/1".into(),
        )
        .await?;
        let request = journal
            .append_record(event("session/load", "request", "load"))
            .await?;
        let response = journal
            .append_record(event("session/load", "response", "load"))
            .await?;
        match fault {
            "request" | "response" => {
                let id = if fault == "request" {
                    request.id
                } else {
                    response.id
                };
                record_entity::Entity::update_many()
                    .col_expr(
                        record_entity::Column::Digest,
                        Expr::value(canonical_digest(&"corrupt")?),
                    )
                    .filter(record_entity::Column::Id.eq(id))
                    .exec(&store.db)
                    .await?;
            }
            "source" => {
                source_entity::Entity::delete_by_id(request.source_id)
                    .exec(&store.db)
                    .await?;
            }
            _ => {
                object_entity::Entity::update_many()
                    .col_expr(object_entity::Column::ObjectJson, Expr::value("{}"))
                    .filter(object_entity::Column::Kind.eq("lifecycle_request"))
                    .exec(&store.db)
                    .await?;
            }
        }
        assert!(
            store
                .lifecycle_operation("controller", "load")
                .await
                .is_err(),
            "{fault}"
        );
    }
    Ok(())
}
