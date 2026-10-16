use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;

use super::fixtures::{FixtureScope, PersistedHistory};
use super::{CanonicalStore, SessionIdentity};
use crate::storage::capture::{CaptureDirection, CaptureEvent, CaptureKind, sessions};

pub(super) fn identity(id: &str) -> SessionIdentity {
    SessionIdentity {
        owner: "local".into(),
        source: "test-agent".into(),
        native_session_id: id.into(),
    }
}

pub(super) fn scope() -> FixtureScope {
    FixtureScope {
        owner: "local".into(),
        source: "test-agent".into(),
        controller_instance_id: Some("controller".into()),
        route_scope_id: Some("principal".into()),
    }
}

pub(super) async fn store() -> anyhow::Result<CanonicalStore> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(CanonicalStore::new(db))
}

pub(super) fn event(
    kind: CaptureKind,
    call_id: Option<u64>,
    method: &str,
    payload: serde_json::Value,
) -> CaptureEvent {
    CaptureEvent {
        direction: CaptureDirection::Client,
        kind,
        call_id,
        method: method.into(),
        payload,
    }
}

pub(super) async fn new_session(recorder: &PersistedHistory, session: &str) -> anyhow::Result<()> {
    recorder
        .append(
            session,
            event(
                CaptureKind::Request,
                Some(1),
                "session/new",
                json!({"cwd":"/workspace"}),
            ),
        )
        .await?;
    recorder
        .append(
            session,
            event(
                CaptureKind::Response,
                Some(1),
                "session/new",
                json!({"result":{"sessionId":session}}),
            ),
        )
        .await?;
    Ok(())
}

pub(super) async fn seed_request(
    store: &CanonicalStore,
    id: &str,
    principal: &str,
    session: &str,
    cost: i64,
    status: &str,
) -> anyhow::Result<()> {
    use sea_orm::{ActiveModelTrait, Set};
    crate::metering::entities::requests::ActiveModel {
        request_id: Set(id.into()),
        user_id: Set("owner".into()),
        api_key_id: Set("key".into()),
        launch_id: Set(None),
        route_scope_id: Set(Some(principal.into())),
        agent_harness: Set(None),
        controller_instance_id: Set(Some("controller".into())),
        acp_session_id: Set(Some(session.into())),
        native_root_session_id: Set(Some("parent".into())),
        native_agent_thread_id: Set(Some(session.into())),
        native_parent_agent_thread_id: Set(None),
        native_turn_id: Set(None),
        route_lease_id: Set(None),
        session_identity_json: Set(None),
        router_id: Set(None),
        binding_digest: Set(None),
        original_selector: Set(None),
        model_id: Set("coding-model".into()),
        provider_id: Set("provider".into()),
        prompt_tokens: Set(0),
        completion_tokens: Set(0),
        reasoning_tokens: Set(0),
        cache_read_tokens: Set(0),
        cache_write_tokens: Set(0),
        uncached_input_tokens: Set(0),
        output_tokens: Set(0),
        usage_origin: Set(String::new()),
        raw_usage_json: Set(None),
        charge_status: Set(status.into()),
        charge_evidence_json: Set(None),
        reconciliation_status: Set(String::new()),
        reconciliation_attempts: Set(0),
        reconciliation_last_error: Set(None),
        reconciliation_last_attempt_at: Set(None),
        authoritative_settled_at: Set(None),
        authoritative_receipt_json: Set(None),
        estimated_charge_micro_usd: Set(cost),
        streamed: Set(0),
        latency_ms: Set(0),
        generation_time_ms: Set(0),
        error: Set(None),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
    }
    .insert(&store.db)
    .await?;
    Ok(())
}

#[tokio::test]
async fn request_union_preserves_unknown_cost_and_principal_scope() -> anyhow::Result<()> {
    let store = store().await?;
    // Multiple capture connections with the same metering namespace must not
    // count settled requests more than once.
    for _ in 0..2 {
        let recorder = store.fixture_history(scope()).await?;
        new_session(recorder.as_ref(), "parent").await?;
        recorder
            .append(
                "child",
                event(
                    CaptureKind::Request,
                    Some(2),
                    "session/load",
                    json!({"sessionId":"child"}),
                ),
            )
            .await?;
        recorder
            .append(
                "child",
                event(
                    CaptureKind::Response,
                    Some(2),
                    "session/load",
                    json!({"result":{}}),
                ),
            )
            .await?;
    }
    // Both stored connections reported the same native new-session identity.
    sessions::Entity::update_many()
        .col_expr(
            sessions::Column::HistoryOrigin,
            Expr::value("native_id_reused"),
        )
        .filter(sessions::Column::SessionKey.eq(identity("parent").key()?))
        .exec(&store.db)
        .await?;
    seed_request(&store, "root", "principal", "parent", 5, "computed").await?;
    seed_request(&store, "child", "principal", "child", 7, "computed").await?;
    seed_request(&store, "unknown", "principal", "parent", 0, "unknown").await?;
    seed_request(
        &store,
        "foreign",
        "other-principal",
        "parent",
        999,
        "computed",
    )
    .await?;
    let parent = store.transcript(&identity("parent")).await?;
    let child = store.transcript(&identity("child")).await?;
    assert_eq!(parent.requests.len(), 3);
    assert_eq!(parent.known_cost_micro_usd, 12);
    assert_eq!(parent.unpriced_requests, 1);
    assert_eq!(child.requests.len(), 1);
    assert_eq!(child.known_cost_micro_usd, 7);
    assert!(
        parent
            .requests
            .iter()
            .all(|request| request.request_id != "foreign")
    );
    let ids: std::collections::BTreeSet<_> = parent
        .requests
        .iter()
        .chain(&child.requests)
        .map(|request| &request.request_id)
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(
        parent
            .requests
            .iter()
            .any(|request| request.charge_micro_usd.is_none())
    );
    Ok(())
}

/// Persist an explicit fork boundary; no controller call state is involved.
pub(crate) async fn fork(
    store: &CanonicalStore,
    history: &PersistedHistory,
    parent: &str,
    child: &str,
    call: u64,
) -> anyhow::Result<()> {
    let parent_key = identity(parent).key()?;
    let boundary = sessions::Entity::find_by_id(&parent_key)
        .one(&store.db)
        .await?
        .map(|row| row.head);
    let reused = sessions::Entity::find_by_id(identity(child).key()?)
        .one(&store.db)
        .await?
        .is_some();
    history
        .append(
            parent,
            event(
                CaptureKind::Request,
                Some(call),
                "session/fork",
                json!({"sessionId":parent}),
            ),
        )
        .await?;
    history
        .append(
            child,
            event(
                CaptureKind::Response,
                Some(call),
                "session/fork",
                json!({"result":{"sessionId":child}}),
            ),
        )
        .await?;
    sessions::Entity::update_many()
        .col_expr(
            sessions::Column::HistoryOrigin,
            Expr::value(if reused {
                "native_id_reused"
            } else {
                "partial"
            }),
        )
        .col_expr(sessions::Column::ParentKey, Expr::value(parent_key))
        .col_expr(sessions::Column::ParentWatermark, Expr::value(boundary))
        .filter(sessions::Column::SessionKey.eq(identity(child).key()?))
        .exec(&store.db)
        .await?;
    Ok(())
}

#[tokio::test]
async fn persisted_projection_keeps_setup_replay_and_scoped_deletion() -> anyhow::Result<()> {
    use crate::storage::capture::{connections, events};
    use sea_orm::{ActiveModelTrait, Set, TransactionTrait};

    let store = store().await?;
    let history = store.fixture_history(scope()).await?;
    new_session(&history, "s").await?;
    history
        .append(
            "s",
            event(
                CaptureKind::Request,
                Some(2),
                "session/load",
                json!({"sessionId":"s"}),
            ),
        )
        .await?;
    // This is an already persisted replay record: it has a connection cursor,
    // but no canonical session cursor. Offline readers must keep it separate.
    let replay = event(
        CaptureKind::Notification,
        None,
        "session/update",
        json!({"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk",
            "content":{"type":"text","text":"replayed"}}}),
    );
    let tx = store.db.begin().await?;
    connections::Entity::update_many()
        .col_expr(connections::Column::Head, Expr::value(4_i64))
        .filter(connections::Column::ConnectionId.eq(history.connection_id()))
        .exec(&tx)
        .await?;
    events::ActiveModel {
        connection_id: Set(history.connection_id().into()),
        sequence: Set(4),
        session_key: Set(Some(identity("s").key()?)),
        session_sequence: Set(None),
        replay: Set(true),
        event_json: Set(serde_json::to_string(&replay)?),
        captured_at: Set(chrono::Utc::now().to_rfc3339()),
    }
    .insert(&tx)
    .await?;
    tx.commit().await?;
    history
        .append(
            "s",
            event(
                CaptureKind::Response,
                Some(2),
                "session/load",
                json!({"result":{}}),
            ),
        )
        .await?;
    history
        .append(
            "s",
            event(
                CaptureKind::Disconnected,
                None,
                "controller/disconnect",
                json!({"clean":true}),
            ),
        )
        .await?;

    let transcript = store.transcript(&identity("s")).await?;
    assert_eq!(transcript.session.head, 3);
    assert_eq!(
        transcript
            .events
            .iter()
            .map(|row| row.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(transcript.setup_evidence.len(), 1);
    assert_eq!(
        transcript.setup_evidence[0].response_node_id,
        transcript.events[0].node_id
    );
    assert_eq!(
        transcript.setup_evidence[0].event.payload,
        json!({"cwd":"/workspace"})
    );
    assert_eq!(transcript.replay_events.len(), 1);
    assert_eq!(
        transcript.replay_events[0].event_json,
        serde_json::to_string(&replay)?
    );
    assert_eq!(transcript.connections[0].state, "complete");
    assert_eq!(
        transcript.gaps,
        vec!["history_across_load_or_resume_is_not_verified"]
    );

    let mut other_scope = scope();
    other_scope.owner = "other".into();
    let other = store.fixture_history(other_scope).await?;
    new_session(&other, "s").await?;
    seed_request(&store, "retained", "principal", "s", 5, "computed").await?;
    store.delete(&identity("s")).await?;
    assert!(store.transcript(&identity("s")).await.is_err());
    assert!(store.list("local", "test-agent").await?.is_empty());
    assert_eq!(store.list("other", "test-agent").await?.len(), 1);
    assert!(
        events::Entity::find()
            .filter(events::Column::SessionKey.eq(identity("s").key()?))
            .all(&store.db)
            .await?
            .is_empty()
    );
    assert!(
        crate::metering::entities::requests::Entity::find_by_id("retained")
            .one(&store.db)
            .await?
            .is_some()
    );
    Ok(())
}
