//! Real Legacy consumers over a mixed graph. Marked rows are exclusion fixtures,
//! not an implementation of owned capture or an authority constructor.
use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::{Alias, Expr, Query};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
};
use serde_json::json;

use crate::acp_trajectory::CanonicalStore;
use crate::acp_trajectory::checkpoint::entities::{
    checkpoints, heads, members, resources, revisions,
};
use crate::acp_trajectory::checkpoint::types::{
    AssessmentContent, AssessmentSource, CriterionScore, RevisionInput,
};
use crate::core::evolution::jobs::JudgeJobs;
use crate::gateway::observability::inventory::GatewayInventory;
use crate::harness::acp::capture::CapturePort;
use crate::harness::trajectory::{Recorder, RecordingScope};
use crate::storage::capture::{
    CaptureDirection, CaptureEvent, CaptureKind, connections, events, sessions,
};
use crate::storage::evolution::{EvolutionStore, records};
use crate::storage::session_identity::SessionIdentity;

fn identity(name: &str) -> SessionIdentity {
    SessionIdentity {
        owner: "fixture-owner".into(),
        source: "fixture-source".into(),
        native_session_id: name.into(),
    }
}

fn event(
    kind: CaptureKind,
    call: Option<u64>,
    method: &str,
    payload: serde_json::Value,
) -> CaptureEvent {
    CaptureEvent {
        direction: CaptureDirection::Client,
        kind,
        call_id: call,
        method: method.into(),
        payload,
    }
}

async fn recorded(db: &DatabaseConnection, name: &str) -> Result<std::sync::Arc<Recorder>> {
    let recorder = Recorder::new(
        db.clone(),
        RecordingScope {
            owner: "fixture-owner".into(),
            source: "fixture-source".into(),
            controller_instance_id: Some(format!("controller-{name}")),
            route_scope_id: Some("fixture-principal".into()),
        },
    )
    .await?;
    GatewayInventory::new(db.clone())
        .register_capture(
            recorder.connection_id(),
            "fixture-principal",
            &format!("controller-{name}"),
        )
        .await?;
    recorder
        .record(event(
            CaptureKind::Request,
            Some(1),
            "session/new",
            json!({"cwd":"/fixture"}),
        ))
        .await?;
    recorder
        .record(event(
            CaptureKind::Response,
            Some(1),
            "session/new",
            json!({"result":{"sessionId":name}}),
        ))
        .await?;
    recorder.record(event(CaptureKind::Notification, None, "session/update", json!({"sessionId":name,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"retained content"}}}))).await?;
    Ok(recorder)
}

async fn mark(
    db: &DatabaseConnection,
    table: &'static str,
    key: &'static str,
    id: &str,
    creator_only: bool,
) -> Result<()> {
    let mut update = Query::update();
    update
        .table(Alias::new(table))
        .value(Alias::new("creator_principal_id"), "marked-creator")
        .and_where(Expr::col(Alias::new(key)).eq(id));
    if !creator_only {
        update
            .value(Alias::new("installation_id"), "marked-installation")
            .value(Alias::new("tenant_id"), "marked-tenant");
    }
    db.execute(db.get_database_backend().build(&update)).await?;
    Ok(())
}

async fn poison(
    db: &DatabaseConnection,
    table: &'static str,
    key: &'static str,
    id: &str,
    column: &'static str,
) -> Result<()> {
    let statement = Query::update()
        .table(Alias::new(table))
        .value(Alias::new(column), "invalid marked payload: never decode")
        .and_where(Expr::col(Alias::new(key)).eq(id))
        .to_owned();
    db.execute(db.get_database_backend().build(&statement))
        .await?;
    Ok(())
}

pub(crate) async fn mixed_namespace(db: &DatabaseConnection) -> Result<()> {
    let canonical = CanonicalStore::new(db.clone());
    let legacy = recorded(db, "legacy").await?;
    let foreign = recorded(db, "marked").await?;
    let store = EvolutionStore::new(db.clone(), "fixture-owner")?;
    let jobs = JudgeJobs::new(store.clone());
    let legacy_key = identity("legacy").key()?;
    let foreign_key = identity("marked").key()?;
    let mut frozen = Vec::new();
    for name in ["legacy", "marked"] {
        let id = identity(name);
        let row = sessions::Entity::find_by_id(id.key()?)
            .one(db)
            .await?
            .context("session")?;
        let cp = canonical.freeze_checkpoint(&id, row.head).await?;
        canonical
            .submit_assessment(&id, label(&format!("label-{name}"), &cp.checkpoint_id))
            .await?;
        canonical
            .observe_checkpoint_resources(&id, &cp.checkpoint_id)
            .await?;
        jobs.enqueue(&id, &cp.checkpoint_id, "fixture/judge", None)
            .await?;
        frozen.push(cp);
    }
    let legacy_json = serde_json::to_value(canonical.transcript(&identity("legacy")).await?)?;
    // Mark a complete second graph, then corrupt bodies to prove SQL exclusion
    // precedes decode. Creator-only metadata is also non-Legacy.
    mark(
        db,
        "acp_capture_connections",
        "connection_id",
        foreign.connection_id(),
        false,
    )
    .await?;
    mark(
        db,
        "acp_canonical_sessions",
        "session_key",
        &foreign_key,
        false,
    )
    .await?;
    mark(
        db,
        "acp_capture_events",
        "connection_id",
        foreign.connection_id(),
        false,
    )
    .await?;
    let foreign_cp = &frozen[1].checkpoint_id;
    for table in [
        "acp_checkpoints",
        "acp_checkpoint_members",
        "acp_resource_observations",
        "acp_assessment_revisions",
    ] {
        mark(db, table, "checkpoint_id", foreign_cp, false).await?;
    }
    mark(
        db,
        "acp_effective_assessments",
        "session_key",
        &foreign_key,
        false,
    )
    .await?;
    mark(
        db,
        "checkpoint_evolution",
        "session_key",
        &foreign_key,
        false,
    )
    .await?;
    poison(
        db,
        "acp_capture_events",
        "connection_id",
        foreign.connection_id(),
        "event_json",
    )
    .await?;
    poison(
        db,
        "acp_checkpoints",
        "checkpoint_id",
        foreign_cp,
        "manifest_json",
    )
    .await?;
    poison(
        db,
        "acp_resource_observations",
        "checkpoint_id",
        foreign_cp,
        "observation_json",
    )
    .await?;
    poison(
        db,
        "acp_assessment_revisions",
        "checkpoint_id",
        foreign_cp,
        "revision_json",
    )
    .await?;
    poison(
        db,
        "checkpoint_evolution",
        "session_key",
        &foreign_key,
        "body",
    )
    .await?;
    // Deliberately tie a marked judge row to the *Legacy* deletion key. The
    // cross-family DELETE must still leave that row untouched.
    store
        .initialize(
            "judge_job",
            "marked-cross-edge",
            Some(legacy_key.clone()),
            &json!({"private":"marked"}),
        )
        .await?;
    let cross = store.id("judge_job", "marked-cross-edge")?;
    mark(db, "checkpoint_evolution", "record_id", &cross, true).await?;
    poison(db, "checkpoint_evolution", "record_id", &cross, "body").await?;

    let foreign_connection = connections::Entity::find_by_id(foreign.connection_id())
        .one(db)
        .await?;
    let foreign_session = sessions::Entity::find_by_id(&foreign_key).one(db).await?;
    let foreign_events = events::Entity::find()
        .filter(events::Column::ConnectionId.eq(foreign.connection_id()))
        .order_by_asc(events::Column::Sequence)
        .all(db)
        .await?;
    let foreign_checkpoint = checkpoints::Entity::find_by_id(foreign_cp).one(db).await?;
    let foreign_members = members::Entity::find()
        .filter(members::Column::CheckpointId.eq(foreign_cp))
        .all(db)
        .await?;
    let foreign_resources = resources::Entity::find()
        .filter(resources::Column::CheckpointId.eq(foreign_cp))
        .all(db)
        .await?;
    let foreign_revisions = revisions::Entity::find()
        .filter(revisions::Column::CheckpointId.eq(foreign_cp))
        .all(db)
        .await?;
    let foreign_head = heads::Entity::find_by_id(&foreign_key).one(db).await?;
    let foreign_jobs = records::Entity::find()
        .filter(records::Column::SessionKey.eq(&foreign_key))
        .order_by_asc(records::Column::RecordId)
        .all(db)
        .await?;
    let cross_before = records::Entity::find_by_id(&cross).one(db).await?;

    ensure!(
        canonical
            .list("fixture-owner", "fixture-source")
            .await?
            .len()
            == 1
    );
    ensure!(serde_json::to_value(canonical.transcript(&identity("legacy")).await?)? == legacy_json);
    ensure!(canonical.transcript(&identity("marked")).await.is_err());
    ensure!(
        canonical
            .checkpoint_content(&identity("marked"), foreign_cp)
            .await
            .is_err()
    );
    ensure!(
        canonical
            .assessment_history(&identity("legacy"))
            .await?
            .len()
            == 1
    );
    ensure!(
        canonical
            .checkpoint_resource_history(&identity("legacy"), &frozen[0].checkpoint_id)
            .await?
            .len()
            == 1
    );
    let remaining = jobs.list().await?;
    ensure!(remaining.len() == 1);
    let legacy_job_id = store.id("judge_job", &remaining[0].job_id)?;
    ensure!(
        records::Entity::find_by_id(&legacy_job_id)
            .one(db)
            .await?
            .is_some()
    );
    ensure!(
        store
            .get::<serde_json::Value>("judge_job", "marked-cross-edge")
            .await?
            .is_none()
    );
    ensure!(
        store
            .initialize(
                "judge_job",
                "marked-cross-edge",
                Some(legacy_key.clone()),
                &json!({"replacement":true})
            )
            .await
            .is_err()
    );
    ensure!(
        GatewayInventory::new(db.clone())
            .register_capture(
                foreign.connection_id(),
                "fixture-principal",
                "controller-marked"
            )
            .await
            .is_err()
    );
    // A live recorder cannot append or mark an ownership-tagged connection failed.
    ensure!(
        foreign
            .record(event(
                CaptureKind::Notification,
                None,
                "session/update",
                json!({"sessionId":"marked"})
            ))
            .await
            .is_err()
    );
    canonical.delete(&identity("marked")).await?;
    canonical.delete(&identity("legacy")).await?;
    ensure!(canonical.transcript(&identity("legacy")).await.is_err());
    ensure!(jobs.list().await?.is_empty());
    ensure!(
        records::Entity::find_by_id(&legacy_job_id)
            .one(db)
            .await?
            .is_none()
    );
    ensure!(
        legacy
            .record(event(
                CaptureKind::Notification,
                None,
                "session/update",
                json!({"sessionId":"legacy"})
            ))
            .await
            .is_err()
    );

    ensure!(
        connections::Entity::find_by_id(foreign.connection_id())
            .one(db)
            .await?
            == foreign_connection
    );
    ensure!(sessions::Entity::find_by_id(&foreign_key).one(db).await? == foreign_session);
    ensure!(
        events::Entity::find()
            .filter(events::Column::ConnectionId.eq(foreign.connection_id()))
            .order_by_asc(events::Column::Sequence)
            .all(db)
            .await?
            == foreign_events
    );
    ensure!(checkpoints::Entity::find_by_id(foreign_cp).one(db).await? == foreign_checkpoint);
    ensure!(
        members::Entity::find()
            .filter(members::Column::CheckpointId.eq(foreign_cp))
            .all(db)
            .await?
            == foreign_members
    );
    ensure!(
        resources::Entity::find()
            .filter(resources::Column::CheckpointId.eq(foreign_cp))
            .all(db)
            .await?
            == foreign_resources
    );
    ensure!(
        revisions::Entity::find()
            .filter(revisions::Column::CheckpointId.eq(foreign_cp))
            .all(db)
            .await?
            == foreign_revisions
    );
    ensure!(heads::Entity::find_by_id(&foreign_key).one(db).await? == foreign_head);
    ensure!(
        records::Entity::find()
            .filter(records::Column::SessionKey.eq(&foreign_key))
            .order_by_asc(records::Column::RecordId)
            .all(db)
            .await?
            == foreign_jobs
    );
    ensure!(records::Entity::find_by_id(&cross).one(db).await? == cross_before);
    Ok(())
}

#[tokio::test]
async fn sqlite_real_legacy_consumers_exclude_marked_graph_before_decode() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    mixed_namespace(&db).await
}

fn label(submission: &str, checkpoint: &str) -> RevisionInput {
    RevisionInput {
        submission_id: submission.into(),
        checkpoint_id: checkpoint.into(),
        expected_revision: None,
        source: AssessmentSource::Human,
        evaluator_id: "fixture-human".into(),
        evaluator_version: "1".into(),
        reason: "explicit fixture label".into(),
        assessment: Some(AssessmentContent {
            pipeline_config_digest: "a".repeat(64),
            selection_digest: "b".repeat(64),
            scores: [("fixture-criterion".into(), CriterionScore::Unknown)].into(),
            evidence: vec![],
            explanation: "fixture label".into(),
        }),
    }
}

mod children;

mod deletion;
