use anyhow::Context;
use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    StopReason,
};
use agent_client_protocol::{Agent, Client, ConnectTo, UntypedMessage};
use bitrouter_sdk::acp::capture::{CaptureDirection, CaptureEvent, CaptureKind, CapturePort};
use bitrouter_sdk::acp::client::{AcpClient, ClientOptions};
use bitrouter_sdk::acp::controller::{Controller, ControllerConfig, ControllerIdentity};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;

use super::{CanonicalStore, RecordingScope, SessionIdentity, connections, events};

fn identity(id: &str) -> SessionIdentity {
    SessionIdentity {
        owner: "local".into(),
        source: "test-agent".into(),
        native_session_id: id.into(),
    }
}

fn scope() -> RecordingScope {
    RecordingScope {
        owner: "local".into(),
        source: "test-agent".into(),
        controller_instance_id: Some("controller".into()),
        route_scope_id: Some("principal".into()),
    }
}

async fn store() -> anyhow::Result<CanonicalStore> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(CanonicalStore::new(db))
}

fn event(
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

async fn new_session(recorder: &dyn CapturePort, session: &str) -> anyhow::Result<()> {
    recorder
        .record(event(
            CaptureKind::Request,
            Some(1),
            "session/new",
            json!({"cwd":"/workspace"}),
        ))
        .await?;
    recorder
        .record(event(
            CaptureKind::Response,
            Some(1),
            "session/new",
            json!({"result":{"sessionId":session}}),
        ))
        .await?;
    Ok(())
}

struct CaptureAgent;

impl ConnectTo<Client> for CaptureAgent {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        Agent.builder().name("capture-fixture")
            .on_receive_request(async |request: InitializeRequest, responder, _connection| {
                responder.respond(InitializeResponse::new(request.protocol_version)
                    .agent_capabilities(AgentCapabilities::new().load_session(true)))
            }, agent_client_protocol::on_receive_request!())
            .on_receive_request(async |_request: NewSessionRequest, responder, connection| {
                // Notifications may arrive before the native new-session result.
                connection.send_notification(UntypedMessage::new("session/update", json!({
                    "sessionId":"native", "update":{"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"ready"}}
                }))?)?;
                responder.respond(NewSessionResponse::new("native"))
            }, agent_client_protocol::on_receive_request!())
            .on_receive_request(async |request: PromptRequest, responder, connection| {
                for i in 0..300 {
                    connection.send_notification(UntypedMessage::new("session/update", json!({
                        "sessionId":request.session_id, "update":{"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":format!("chunk-{i}")}}
                    }))?)?;
                }
                connection.send_notification(UntypedMessage::new("session/update", json!({
                    "sessionId":request.session_id, "update":{"sessionUpdate":"tool_call", "toolCallId":"test", "title":"Run tests", "status":"in_progress", "rawInput":{"command":"cargo test"}}
                }))?)?;
                connection.send_notification(UntypedMessage::new("session/update", json!({
                    "sessionId":request.session_id, "update":{"sessionUpdate":"tool_call_update", "toolCallId":"test", "status":"completed", "rawOutput":{"exitCode":0,"stdout":"passed"}}
                }))?)?;
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            }, agent_client_protocol::on_receive_request!())
            .on_receive_request(async |request: LoadSessionRequest, responder, connection| {
                connection.send_notification(UntypedMessage::new("session/update", json!({
                    "sessionId":request.session_id, "update":{"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"ready"}}
                }))?)?;
                responder.respond(LoadSessionResponse::new())
            }, agent_client_protocol::on_receive_request!())
            .connect_to(client).await
    }
}

async fn connect(
    store: &CanonicalStore,
) -> anyhow::Result<(
    AcpClient,
    tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
)> {
    let recorder = store.recorder(scope()).await?;
    let controller = Controller::new(
        CaptureAgent,
        ControllerConfig::new(ControllerIdentity::new("fixture", "fixture", "1")),
    )
    .capture(recorder);
    let (manager, server) = agent_client_protocol::Channel::duplex();
    let task = tokio::spawn(async move { controller.run(server).await });
    let client = AcpClient::connect(
        manager,
        ClientOptions {
            turn_timeout: Some(Duration::from_secs(10)),
            terminal_auth: false,
        },
    )
    .await?;
    Ok((client, task))
}

#[tokio::test]
async fn controller_durably_records_before_lossy_ui_and_keeps_replay_separate() -> anyhow::Result<()>
{
    tokio::time::timeout(Duration::from_secs(30), async {
        let store = store().await?;
        let (client, task) = connect(&store).await?;
        let ids = client
            .new_session(PathBuf::from("/workspace"), vec![])
            .await?;
        // Deliberately never subscribe to the lossy broadcast streams.
        client.prompt(&ids.acp_session_id, "repeat").await?;
        client.prompt(&ids.acp_session_id, "repeat").await?;
        client.shutdown().await?;
        task.await??;
        let first = store.transcript(&identity("native")).await?;
        assert_eq!(first.session.history_origin, "new");
        assert_eq!(first.setup_evidence.len(), 1);
        assert_eq!(
            first.setup_evidence[0].event.payload.get("cwd"),
            Some(&json!("/workspace"))
        );
        assert!(
            first.setup_evidence[0]
                .event
                .payload
                .get("mcpServers")
                .is_none()
        );
        assert!(
            first
                .events
                .iter()
                .any(|node| node.node_id == first.setup_evidence[0].response_node_id)
        );
        assert!(first.gaps.is_empty(), "{:?}", first.gaps);
        assert_eq!(
            first
                .events
                .iter()
                .filter(|node| node.event.method == "session/prompt"
                    && node.event.kind == CaptureKind::Request)
                .count(),
            2
        );
        assert_eq!(
            first
                .events
                .iter()
                .filter(|node| node
                    .event
                    .payload
                    .pointer("/update/content/text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|text| text.starts_with("chunk-")))
                .count(),
            600
        );
        assert!(first.events.iter().any(|node| {
            node.event.payload.pointer("/update/rawOutput/exitCode") == Some(&json!(0))
        }));
        let first_ids: Vec<_> = first
            .events
            .iter()
            .map(|node| node.node_id.clone())
            .collect();
        let (client, task) = connect(&store).await?;
        client
            .load_session("native", PathBuf::from("/workspace"), vec![])
            .await?;
        client.shutdown().await?;
        task.await??;
        let second = store.transcript(&identity("native")).await?;
        assert_eq!(second.replay_events.len(), 1);
        assert_eq!(second.events.len(), first.events.len() + 2);
        assert_eq!(
            second
                .events
                .iter()
                .take(first_ids.len())
                .map(|node| node.node_id.clone())
                .collect::<Vec<_>>(),
            first_ids
        );
        assert!(second.gaps.iter().any(|gap| gap.contains("load_or_resume")));
        assert_eq!(store.list("local", "test-agent").await?.len(), 1);
        anyhow::Ok(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn append_failure_latches_incomplete_and_does_not_advance_watermark() -> anyhow::Result<()> {
    let store = store().await?;
    let recorder = store.recorder(scope()).await?;
    new_session(recorder.as_ref(), "s").await?;
    let old = store.transcript(&identity("s")).await?.session.head;
    // A schema/storage failure after successful capture, without a model call.
    use sea_orm_migration::prelude::{Alias, Table};
    use sea_orm_migration::{MigrationTrait, SchemaManager};
    SchemaManager::new(&store.db)
        .drop_table(
            Table::drop()
                .table(Alias::new("acp_capture_events"))
                .to_owned(),
        )
        .await?;
    let failed = recorder
        .record(event(
            CaptureKind::Notification,
            None,
            "session/update",
            json!({"sessionId":"s"}),
        ))
        .await;
    assert!(failed.is_err());
    crate::db::migration::m20240101_000018_create_acp_capture::Migration
        .up(&SchemaManager::new(&store.db))
        .await?;
    assert!(
        recorder
            .record(event(
                CaptureKind::Disconnected,
                None,
                "controller/disconnect",
                json!({"clean":true})
            ))
            .await
            .is_err()
    );
    assert!(old > 0);
    assert!(
        store
            .transcript(&identity("s"))
            .await?
            .gaps
            .iter()
            .any(|gap| gap == "canonical_sequence_gap")
    );
    assert!(
        connections::Entity::find()
            .all(&store.db)
            .await?
            .iter()
            .all(|row| row.state == "interrupted")
    );
    assert_eq!(
        super::sessions::Entity::find_by_id(identity("s").key()?)
            .one(&store.db)
            .await?
            .context("session missing")?
            .head,
        old
    );
    Ok(())
}

#[tokio::test]
async fn fork_boundary_and_owner_source_isolation_are_preserved() -> anyhow::Result<()> {
    let store = store().await?;
    let recorder = store.recorder(scope()).await?;
    new_session(recorder.as_ref(), "parent").await?;
    let boundary = store.transcript(&identity("parent")).await?.session.head;
    recorder
        .record(event(
            CaptureKind::Request,
            Some(2),
            "session/fork",
            json!({"sessionId":"parent"}),
        ))
        .await?;
    recorder
        .record(event(
            CaptureKind::Response,
            Some(2),
            "session/fork",
            json!({"result":{"sessionId":"child"}}),
        ))
        .await?;
    let child = store.transcript(&identity("child")).await?;
    assert_eq!(child.session.parent_key, Some(identity("parent").key()?));
    assert_eq!(child.session.parent_watermark, Some(boundary));
    for (owner, source) in [("other", "test-agent"), ("local", "other")] {
        let mut other = scope();
        other.owner = owner.into();
        other.source = source.into();
        let recorder = store.recorder(other).await?;
        new_session(recorder.as_ref(), "parent").await?;
        assert_eq!(store.list(owner, source).await?.len(), 1);
    }
    assert_eq!(store.list("local", "test-agent").await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn delete_removes_content_and_fences_an_active_recorder() -> anyhow::Result<()> {
    let store = store().await?;
    let recorder = store.recorder(scope()).await?;
    new_session(recorder.as_ref(), "s").await?;
    recorder
        .record(event(
            CaptureKind::Request,
            Some(2),
            "session/prompt",
            json!({"sessionId":"s","prompt":[{"type":"text","text":"private text"}]}),
        ))
        .await?;
    store.delete(&identity("s")).await?;
    assert!(store.transcript(&identity("s")).await.is_err());
    assert!(
        recorder
            .record(event(
                CaptureKind::Response,
                Some(2),
                "session/prompt",
                json!({"result":{"stopReason":"end_turn"}})
            ))
            .await
            .is_err()
    );
    assert!(
        events::Entity::find()
            .filter(events::Column::SessionKey.eq(identity("s").key()?))
            .all(&store.db)
            .await?
            .is_empty()
    );
    assert!(store.list("local", "test-agent").await?.is_empty());
    Ok(())
}

async fn seed_request(
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
        let recorder = store.recorder(scope()).await?;
        new_session(recorder.as_ref(), "parent").await?;
        recorder
            .record(event(
                CaptureKind::Request,
                Some(2),
                "session/load",
                json!({"sessionId":"child"}),
            ))
            .await?;
        recorder
            .record(event(
                CaptureKind::Response,
                Some(2),
                "session/load",
                json!({"result":{}}),
            ))
            .await?;
    }
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

#[tokio::test]
async fn concurrent_connections_assign_one_canonical_order() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!("sqlite://{}", directory.path().join("capture.db").display());
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let store = CanonicalStore::new(db);
    let initial = store.recorder(scope()).await?;
    new_session(initial.as_ref(), "s").await?;
    let mut tasks = Vec::new();
    for worker in 0..4 {
        let recorder = store.recorder(scope()).await?;
        tasks.push(tokio::spawn(async move {
            for item in 0..20 {
                recorder
                    .record(event(
                        CaptureKind::Notification,
                        None,
                        "session/update",
                        json!({"sessionId":"s", "update":{"worker":worker,"item":item}}),
                    ))
                    .await?;
            }
            anyhow::Ok(())
        }));
    }
    for task in tasks {
        task.await??;
    }
    let transcript = store.transcript(&identity("s")).await?;
    assert_eq!(transcript.session.head, 81);
    assert_eq!(transcript.events.len(), 81);
    assert!(
        !transcript
            .gaps
            .iter()
            .any(|gap| gap == "canonical_sequence_gap")
    );
    // A new-session response reusing a previously observed native ID must not
    // relabel that mixed history as a complete fresh session.
    let reused = store.recorder(scope()).await?;
    new_session(reused.as_ref(), "s").await?;
    assert!(
        store
            .transcript(&identity("s"))
            .await?
            .gaps
            .iter()
            .any(|gap| gap == "native_session_id_reused_for_new_session")
    );
    Ok(())
}
