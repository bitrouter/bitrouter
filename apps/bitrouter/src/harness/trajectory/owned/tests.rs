//! Storage-only invariants; authenticated/process claims use the facade tests.
mod replay;

use super::*;
use crate::{
    acp_trajectory::{CanonicalStore, checkpoint::owned::freeze_in},
    harness::acp::capture::CapturePort,
    storage::capture::{CaptureDirection as Direction, CaptureKind as Kind},
};
use entities::{checkpoints, members, records};
use sea_orm::{ConnectionTrait, DatabaseConnection, QueryOrder, TransactionTrait};
use serde_json::json;

const AT: &str = "2026-10-25T10:00:00.000+00:00";
fn association(tenant: &str, mode: LearningMode) -> Result<Association> {
    let (scope, provenance) = Scope::for_test(
        "installation".try_into()?,
        tenant.try_into()?,
        "principal".try_into()?,
    );
    Association::new(
        scope,
        provenance,
        "controller".into(),
        "fixture".into(),
        "revision".into(),
        "route".into(),
        mode,
    )
}
fn event(
    kind: Kind,
    direction: Direction,
    call: Option<u64>,
    method: &str,
    payload: serde_json::Value,
) -> CaptureEvent {
    CaptureEvent {
        direction,
        kind,
        call_id: call,
        method: method.into(),
        payload,
    }
}
fn observations() -> Vec<CaptureEvent> {
    vec![
        event(
            Kind::Connected,
            Direction::Controller,
            None,
            "controller/connect",
            json!({}),
        ),
        event(
            Kind::Request,
            Direction::Client,
            Some(1),
            "session/new",
            json!({"cwd":"/fixture"}),
        ),
        event(
            Kind::Response,
            Direction::Agent,
            Some(1),
            "session/new",
            json!({"result":{"sessionId":"native"}}),
        ),
        event(
            Kind::Request,
            Direction::Client,
            Some(2),
            "session/set_config_option",
            json!({"sessionId":"native","configId":"model","value":"route"}),
        ),
        event(
            Kind::Response,
            Direction::Agent,
            Some(2),
            "session/set_config_option",
            json!({"result":{"configOptions":[{"id":"model","type":"select","currentValue":"route","options":[{"value":"route","name":"Route"}]}]}}),
        ),
        event(
            Kind::Request,
            Direction::Client,
            Some(3),
            "session/prompt",
            json!({"sessionId":"native","prompt":[{"type":"text","text":"private task"}]}),
        ),
        event(
            Kind::Notification,
            Direction::Agent,
            None,
            "session/update",
            json!({"sessionId":"native","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"private output"}}}),
        ),
        event(
            Kind::Response,
            Direction::Agent,
            Some(3),
            "session/prompt",
            json!({"result":{"stopReason":"end_turn"}}),
        ),
    ]
}
async fn create(db: &DatabaseConnection, association: &Association) -> Result<()> {
    let tx = db.begin().await?;
    association
        .create_in(&tx, AT, &OperationBudget::new(&|| Ok(())))
        .await?;
    tx.commit().await?;
    Ok(())
}
async fn append(
    db: &DatabaseConnection,
    association: &Association,
    sequence: usize,
    value: CaptureEvent,
) -> Result<Append> {
    let append = Append::new(i64::try_from(sequence)?, AT.into(), value)?;
    let tx = db.begin().await?;
    association
        .append_in(&tx, &append, &OperationBudget::new(&|| Ok(())))
        .await?;
    tx.commit().await?;
    Ok(append)
}
async fn recorded(mode: LearningMode) -> Result<(DatabaseConnection, Association, Append)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let association = association("tenant", mode)?;
    create(&db, &association).await?;
    let mut last = None;
    for (i, event) in observations().into_iter().enumerate() {
        last = Some(append(&db, &association, i + 1, event).await?);
    }
    Ok((db, association, last.context("last observation")?))
}
async fn raw(
    db: &impl ConnectionTrait,
) -> Result<(
    Vec<checkpoints::Model>,
    Vec<members::Model>,
    Vec<records::Model>,
)> {
    Ok((
        checkpoints::Entity::find()
            .order_by_asc(checkpoints::Column::CheckpointId)
            .all(db)
            .await?,
        members::Entity::find()
            .order_by_asc(members::Column::CheckpointId)
            .all(db)
            .await?,
        records::Entity::find()
            .order_by_asc(records::Column::RecordId)
            .all(db)
            .await?,
    ))
}

#[tokio::test]
async fn bounded_stopped_prefix_replay_is_exact_inside_active_transaction() -> Result<()> {
    let (db, association, last) = recorded(LearningMode::Observe).await?;
    let tx = db.begin().await?;
    let checkpoint = freeze_in(
        &tx,
        &association,
        "native",
        &OperationBudget::new(&|| Ok(())),
    )
    .await?
    .context("stopped prefix")?;
    assert_eq!(checkpoint.watermark, 6);
    assert_eq!(checkpoint.observations, 7);
    let stored = raw(&tx).await?;
    assert_eq!((stored.0.len(), stored.1.len(), stored.2.len()), (1, 1, 1));
    assert!(!stored.0[0].manifest_json.contains("private task"));
    assert!(!stored.0[0].manifest_json.contains("private output"));
    tx.commit().await?;
    let tx = db.begin().await?;
    assert!(
        association
            .append_in(&tx, &last, &OperationBudget::new(&|| Ok(())))
            .await?
            .duplicate
    );
    assert_eq!(
        freeze_in(
            &tx,
            &association,
            "native",
            &OperationBudget::new(&|| Ok(()))
        )
        .await?,
        Some(checkpoint)
    );
    assert_eq!(raw(&tx).await?, stored);
    tx.rollback().await?;
    assert_eq!(raw(&db).await?, stored);
    Ok(())
}

#[tokio::test]
async fn missing_or_malformed_checkpoint_closure_refuses_before_any_repair() -> Result<()> {
    for damage in ["member", "cursor", "manifest"] {
        let (db, association, _) = recorded(LearningMode::Explore).await?;
        let tx = db.begin().await?;
        assert!(
            freeze_in(
                &tx,
                &association,
                "native",
                &OperationBudget::new(&|| Ok(()))
            )
            .await?
            .is_some()
        );
        tx.commit().await?;
        let sql = match damage {
            "member" => "DELETE FROM acp_checkpoint_members",
            "cursor" => "DELETE FROM checkpoint_evolution",
            _ => "UPDATE acp_checkpoints SET manifest_json = '{}'",
        };
        db.execute_unprepared(sql).await?;
        let before = raw(&db).await?;
        let tx = db.begin().await?;
        assert!(
            freeze_in(
                &tx,
                &association,
                "native",
                &OperationBudget::new(&|| Ok(()))
            )
            .await
            .is_err(),
            "{damage}"
        );
        assert_eq!(raw(&tx).await?, before, "tentative repair at {damage}");
        tx.rollback().await?;
        assert_eq!(raw(&db).await?, before);
    }
    Ok(())
}

#[tokio::test]
async fn checkpoint_member_failure_rolls_back_real_checkpoint_insert() -> Result<()> {
    let (db, association, _) = recorded(LearningMode::Observe).await?;
    db.execute_unprepared("CREATE TRIGGER reject_owned_member BEFORE INSERT ON acp_checkpoint_members BEGIN SELECT RAISE(ABORT, 'fixture member failure'); END").await?;
    let tx = db.begin().await?;
    assert!(
        freeze_in(
            &tx,
            &association,
            "native",
            &OperationBudget::new(&|| Ok(()))
        )
        .await
        .is_err()
    );
    let inside = raw(&tx).await?;
    assert_eq!((inside.0.len(), inside.1.len(), inside.2.len()), (1, 0, 0));
    tx.rollback().await?;
    assert_eq!(raw(&db).await?, (vec![], vec![], vec![]));
    Ok(())
}

#[tokio::test]
async fn off_and_unmatched_prompt_never_schedule_paid_or_passive_work() -> Result<()> {
    let (db, association, _) = recorded(LearningMode::Off).await?;
    let tx = db.begin().await?;
    assert!(
        freeze_in(
            &tx,
            &association,
            "not-even-an-existing-id",
            &OperationBudget::new(&|| Ok(()))
        )
        .await?
        .is_none()
    );
    assert_eq!(raw(&tx).await?, (vec![], vec![], vec![]));
    tx.rollback().await?;
    let (db, association, _) = recorded(LearningMode::Observe).await?;
    append(
        &db,
        &association,
        9,
        event(
            Kind::Request,
            Direction::Client,
            Some(4),
            "session/prompt",
            json!({"sessionId":"native","prompt":[]}),
        ),
    )
    .await?;
    let tx = db.begin().await?;
    assert!(
        freeze_in(
            &tx,
            &association,
            "native",
            &OperationBudget::new(&|| Ok(()))
        )
        .await?
        .is_none()
    );
    assert_eq!(raw(&tx).await?, (vec![], vec![], vec![]));
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn owned_capture_and_legacy_real_consumers_cannot_cross_namespaces() -> Result<()> {
    let (db, owned, _) = recorded(LearningMode::Observe).await?;
    let foreign = association("other-tenant", LearningMode::Observe)?;
    create(&db, &foreign).await?;
    db.execute_unprepared(&format!(
        "UPDATE acp_capture_connections SET metadata_json = '{}' WHERE connection_id = '{}'",
        "x".repeat(MAX_METADATA_BYTES + 1),
        foreign.connection()
    ))
    .await?;
    let legacy = super::super::Recorder::new(
        db.clone(),
        super::super::RecordingScope {
            owner: "principal".into(),
            source: "fixture".into(),
            controller_instance_id: None,
            route_scope_id: None,
        },
    )
    .await?;
    for event in observations() {
        legacy.record(event).await?;
    }
    let rows = CanonicalStore::new(db.clone())
        .list("principal", "fixture")
        .await?;
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0].session_key, owned.session_key("native")?);
    let tx = db.begin().await?;
    assert!(
        freeze_in(&tx, &owned, "native", &OperationBudget::new(&|| Ok(())))
            .await?
            .is_some()
    );
    assert!(
        foreign
            .connection_in(&tx, &OperationBudget::new(&|| Ok(())))
            .await
            .is_err()
    );
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn lost_append_closure_and_foreign_native_ids_cannot_be_repaired_or_adopted() -> Result<()> {
    let (db, association, last) = recorded(LearningMode::Observe).await?;
    db.execute_unprepared("DELETE FROM acp_canonical_sessions")
        .await?;
    let tx = db.begin().await?;
    let before = events::Entity::find().all(&tx).await?;
    assert!(
        association
            .append_in(&tx, &last, &OperationBudget::new(&|| Ok(())))
            .await
            .is_err()
    );
    assert_eq!(events::Entity::find().all(&tx).await?, before);
    assert!(sessions::Entity::find().all(&tx).await?.is_empty());
    tx.rollback().await?;
    let state = protocol::State::initial(&association);
    let connected = Append::new(1, AT.into(), observations().remove(0))?;
    let state = state.next(&connected)?.0;
    for method in [
        "session/load",
        "session/resume",
        "session/fork",
        "session/prompt",
    ] {
        let input = Append::new(
            2,
            AT.into(),
            event(
                Kind::Request,
                Direction::Client,
                Some(1),
                method,
                json!({"sessionId":"foreign"}),
            ),
        )?;
        assert!(state.next(&input).is_err(), "{method}");
    }
    Ok(())
}

#[tokio::test]
async fn live_capture_bound_leaves_real_response_and_disconnect_headroom() -> Result<()> {
    let (db, association, _) = recorded(LearningMode::Observe).await?;
    append(
        &db,
        &association,
        9,
        event(
            Kind::Request,
            Direction::Client,
            Some(4),
            "session/prompt",
            json!({"sessionId":"native","prompt":[]}),
        ),
    )
    .await?;
    let mut sequence = 10;
    loop {
        let update = Append::new(
            sequence,
            AT.into(),
            event(
                Kind::Notification,
                Direction::Agent,
                None,
                "session/update",
                json!({"sessionId":"native","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x".repeat(220 * 1024)}}}),
            ),
        )?;
        let tx = db.begin().await?;
        let before = association
            .connection_in(&tx, &OperationBudget::new(&|| Ok(())))
            .await?
            .0;
        if association
            .append_in(&tx, &update, &OperationBudget::new(&|| Ok(())))
            .await
            .is_err()
        {
            assert_eq!(
                association
                    .connection_in(&tx, &OperationBudget::new(&|| Ok(())))
                    .await?
                    .0,
                before,
                "reserve failure mutated connection"
            );
            tx.rollback().await?;
            break;
        }
        tx.commit().await?;
        sequence += 1;
        assert!(sequence < 100, "capture byte bound was ineffective");
    }
    let response = Append::new(
        sequence,
        AT.into(),
        event(
            Kind::Response,
            Direction::Agent,
            Some(4),
            "session/prompt",
            json!({"result":{"stopReason":"end_turn"}}),
        ),
    )?;
    let tx = db.begin().await?;
    association
        .check_cleanup_in(&tx, &response, &OperationBudget::new(&|| Ok(())))
        .await?;
    association
        .append_in(&tx, &response, &OperationBudget::new(&|| Ok(())))
        .await?;
    let close = Append::new(
        sequence + 1,
        AT.into(),
        event(
            Kind::Disconnected,
            Direction::Controller,
            None,
            "controller/disconnect",
            json!({"clean":true}),
        ),
    )?;
    association
        .check_cleanup_in(&tx, &close, &OperationBudget::new(&|| Ok(())))
        .await?;
    association
        .append_in(&tx, &close, &OperationBudget::new(&|| Ok(())))
        .await?;
    assert!(
        association
            .connection_in(&tx, &OperationBudget::new(&|| Ok(())))
            .await?
            .1
            .complete
    );
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn late_notification_is_not_a_new_stopped_prompt_boundary() -> Result<()> {
    let (db, association, _) = recorded(LearningMode::Observe).await?;
    append(&db, &association, 9, event(Kind::Notification, Direction::Agent, None, "session/update", json!({"sessionId":"native","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"late"}}}))).await?;
    let tx = db.begin().await?;
    assert!(
        freeze_in(
            &tx,
            &association,
            "native",
            &OperationBudget::new(&|| Ok(()))
        )
        .await?
        .is_none()
    );
    assert_eq!(raw(&tx).await?, (vec![], vec![], vec![]));
    tx.rollback().await?;
    Ok(())
}
