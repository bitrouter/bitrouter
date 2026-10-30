use super::*;

fn row(kind: &str, id: &str, revision: i64, body: &str) -> Model {
    Model {
        record_id: id.into(),
        scope_id: "tenant-qualified-unit".into(),
        kind: kind.into(),
        session_key: Some("actual-session-edge".into()),
        revision,
        body: body.into(),
        installation_id: Some("installation".into()),
        tenant_id: Some("tenant".into()),
        creator_principal_id: Some("creator".into()),
    }
}

// These are digest/limit unit tests, not fabricated positive storage evidence.
fn source_digest(rows: &[Model]) -> Result<String> {
    let check = || Ok(());
    let budget = OperationBudget::with_limits(&check, 4096, 8 * 1024 * 1024);
    let mut sources = Sources::default();
    for row in rows {
        sources.record(row, &budget)?;
    }
    let ordered: Vec<_> = sources.0.into_iter().collect();
    canonical_digest(&ordered)
}

#[test]
fn source_fence_is_order_independent_but_tracks_revision_removal_and_namespace() -> Result<()> {
    let unit = row(UNIT, "unit", 3, r#"{"pending":0}"#);
    let step = row(STEP, "step", 2, r#"{"ack":{"head":5}}"#);
    let before = source_digest(&[unit.clone(), step.clone()])?;
    assert_eq!(before, source_digest(&[step.clone(), unit.clone()])?);
    assert_eq!(
        before,
        source_digest(&[unit.clone(), step.clone(), step.clone()])?
    );
    assert_ne!(before, source_digest(std::slice::from_ref(&unit))?);
    for replacement in [
        Model {
            revision: 3,
            ..step.clone()
        },
        Model {
            body: r#"{"ack":{"head":6}}"#.into(),
            ..step.clone()
        },
        Model {
            tenant_id: Some("other".into()),
            ..step.clone()
        },
        Model {
            creator_principal_id: Some("other".into()),
            ..step.clone()
        },
        Model {
            kind: STEP_PENDING.into(),
            ..step.clone()
        },
        Model {
            session_key: Some("other".into()),
            ..step.clone()
        },
    ] {
        assert_ne!(before, source_digest(&[unit.clone(), replacement])?);
    }
    Ok(())
}

#[test]
fn contradictory_source_reuse_and_cumulative_bytes_refuse_input() -> Result<()> {
    let check = || Ok(());
    let budget = OperationBudget::with_limits(&check, 4096, 1024);
    let mut sources = Sources::default();
    let first = row(UNIT, "unit", 0, "{}");
    sources.record(&first, &budget)?;
    assert!(
        sources
            .record(
                &Model {
                    revision: 1,
                    ..first
                },
                &budget
            )
            .is_err()
    );
    let narrow = OperationBudget::with_limits(&check, 4096, 0);
    assert!(
        Sources::default()
            .record(&row(UNIT, "unit", 0, "{}"), &narrow)
            .is_err()
    );
    Ok(())
}
