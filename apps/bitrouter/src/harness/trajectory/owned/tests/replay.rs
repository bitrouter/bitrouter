//! Correlation, not a possibly corrupted stored edge, determines replay closure.
use super::*;

async fn capture_rows(
    db: &impl ConnectionTrait,
) -> Result<(
    Vec<connections::Model>,
    Vec<sessions::Model>,
    Vec<events::Model>,
)> {
    Ok((
        connections::Entity::find()
            .order_by_asc(connections::Column::ConnectionId)
            .all(db)
            .await?,
        sessions::Entity::find()
            .order_by_asc(sessions::Column::SessionKey)
            .all(db)
            .await?,
        events::Entity::find()
            .order_by_asc(events::Column::ConnectionId)
            .order_by_asc(events::Column::Sequence)
            .all(db)
            .await?,
    ))
}

async fn refuses_without_repair(
    db: &DatabaseConnection,
    association: &Association,
    last: &Append,
) -> Result<()> {
    let before = capture_rows(db).await?;
    let tx = db.begin().await?;
    let error = association
        .append_in(&tx, last, &OperationBudget::new(&|| Ok(())))
        .await
        .err()
        .context("corrupted replay unexpectedly succeeded")?;
    assert_eq!(error.to_string(), "capture replay session edge changed");
    // Rollback must not hide tentative event/session/connection repairs.
    assert_eq!(capture_rows(&tx).await?, before);
    tx.rollback().await?;
    assert_eq!(capture_rows(db).await?, before);
    Ok(())
}

async fn accepts_without_repair(
    db: &DatabaseConnection,
    association: &Association,
    last: &Append,
) -> Result<()> {
    let before = capture_rows(db).await?;
    let tx = db.begin().await?;
    assert!(
        association
            .append_in(&tx, last, &OperationBudget::new(&|| Ok(())))
            .await?
            .duplicate
    );
    assert_eq!(capture_rows(&tx).await?, before);
    tx.rollback().await?;
    assert_eq!(capture_rows(db).await?, before);
    Ok(())
}

#[tokio::test]
async fn duplicate_append_refuses_null_required_edge_with_or_without_session_row() -> Result<()> {
    for missing_session in [false, true] {
        let (db, association, last) = recorded(LearningMode::Observe).await?;
        let changed = db.execute_unprepared("UPDATE acp_capture_events SET session_key = NULL, session_sequence = NULL WHERE sequence = 8").await?;
        assert_eq!(changed.rows_affected(), 1);
        if missing_session {
            assert_eq!(
                db.execute_unprepared("DELETE FROM acp_canonical_sessions")
                    .await?
                    .rows_affected(),
                1
            );
        }
        refuses_without_repair(&db, &association, &last).await?;
    }
    Ok(())
}

#[tokio::test]
async fn duplicate_append_refuses_same_controller_equal_head_sibling_edge() -> Result<()> {
    let (db, association, native_last) = recorded(LearningMode::Observe).await?;
    let mut last = None;
    for (index, mut observation) in observations().into_iter().skip(1).enumerate() {
        observation.call_id = observation.call_id.map(|id| id + 3);
        if observation.payload.get("sessionId").is_some() {
            observation.payload["sessionId"] = json!("sibling");
        }
        if observation.method == "session/new" && observation.kind == Kind::Response {
            observation.payload["result"]["sessionId"] = json!("sibling");
        }
        last = Some(append(&db, &association, index + 9, observation).await?);
    }
    let last = last.context("sibling prompt response")?;
    let rows = capture_rows(&db).await?;
    assert_eq!(rows.1.len(), 2);
    assert_eq!(rows.1[0].head, rows.1[1].head);
    accepts_without_repair(&db, &association, &last).await?;
    let native_key = association.session_key("native")?;
    let sibling_key = association.session_key("sibling")?;
    // Both terminal slots are occupied. Swap only their keys, using the
    // nullable key inside this transaction to preserve the unique index.
    let tx = db.begin().await?;
    for (sequence, key) in [
        (native_last.sequence, None),
        (last.sequence, Some(native_key.clone())),
        (native_last.sequence, Some(sibling_key.clone())),
    ] {
        let changed = events::Entity::update_many()
            .filter(events::Column::ConnectionId.eq(association.connection()))
            .filter(events::Column::Sequence.eq(sequence))
            .col_expr(
                events::Column::SessionKey,
                sea_orm::sea_query::Expr::value(key),
            )
            .exec(&tx)
            .await?;
        assert_eq!(changed.rows_affected, 1);
    }
    tx.commit().await?;
    let head = rows.1[0].head;
    let mut expected = rows;
    for (sequence, from, to) in [
        (
            native_last.sequence,
            native_key.clone(),
            sibling_key.clone(),
        ),
        (last.sequence, sibling_key, native_key),
    ] {
        let row = expected
            .2
            .iter_mut()
            .find(|row| row.sequence == sequence)
            .context("terminal capture event")?;
        assert_eq!(row.session_key.as_deref(), Some(from.as_str()));
        assert_eq!(row.session_sequence, Some(head));
        row.session_key = Some(to);
    }
    // Payloads, session sequences, both heads and correlation remain exact.
    assert_eq!(capture_rows(&db).await?, expected);
    refuses_without_repair(&db, &association, &last).await
}

#[tokio::test]
async fn duplicate_disconnect_refuses_an_edge_where_correlation_requires_null() -> Result<()> {
    let (db, association, _) = recorded(LearningMode::Observe).await?;
    let last = append(
        &db,
        &association,
        9,
        event(
            Kind::Disconnected,
            Direction::Controller,
            None,
            "controller/disconnect",
            json!({"clean":true}),
        ),
    )
    .await?;
    accepts_without_repair(&db, &association, &last).await?;
    let mut expected = capture_rows(&db).await?;
    let tx = db.begin().await?;
    let head = association
        .session_in(&tx, "native", &OperationBudget::new(&|| Ok(())))
        .await?
        .head;
    tx.rollback().await?;
    let key = association.session_key("native")?;
    let unused = head.checked_add(1).context("session sequence overflow")?;
    assert!(!expected.2.iter().any(|row| {
        row.session_key.as_deref() == Some(key.as_str()) && row.session_sequence == Some(unused)
    }));
    let changed = events::Entity::update_many()
        .filter(events::Column::ConnectionId.eq(association.connection()))
        .filter(events::Column::Sequence.eq(last.sequence))
        .col_expr(
            events::Column::SessionKey,
            sea_orm::sea_query::Expr::value(key.clone()),
        )
        .col_expr(
            events::Column::SessionSequence,
            sea_orm::sea_query::Expr::value(unused),
        )
        .exec(&db)
        .await?;
    assert_eq!(changed.rows_affected, 1);
    let row = expected
        .2
        .iter_mut()
        .find(|row| row.sequence == last.sequence)
        .context("disconnect capture event")?;
    assert!(row.session_key.is_none());
    assert!(row.session_sequence.is_none());
    row.session_key = Some(key);
    row.session_sequence = Some(unused);
    assert_eq!(capture_rows(&db).await?, expected);
    refuses_without_repair(&db, &association, &last).await
}
