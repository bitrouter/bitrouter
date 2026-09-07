use super::*;
use crate::session_evidence::execution::input_runs::InputOutcome;
use crate::session_evidence::types::AttemptPhase;

async fn append_turn(service: &ControllerEvidence, id: &str) -> Result<()> {
    let source = service.store.source(id).await?.context("native source")?;
    let path = Path::new(
        source
            .descriptor
            .locator
            .strip_prefix("spool:")
            .context("spool locator")?,
    );
    let mut rows = Vec::new();
    let mut start = 0;
    while start < source.cursor.next_sequence {
        let end = (start + RECORD_PAGE_SIZE).min(source.cursor.next_sequence);
        rows.extend(
            service
                .store
                .records(&SourceRange {
                    source_id: id.into(),
                    generation: "spool/1".into(),
                    start,
                    end,
                })
                .await?
                .into_iter()
                .map(|record| record.input.raw),
        );
        start = end;
    }
    for (method, status) in [
        ("turn/started", "inProgress"),
        ("turn/completed", "completed"),
    ] {
        rows.push(json!({"sequence":rows.len(),"method":method,"direction":"server","phase":"notification",
            "payload":{"threadId":"native","turn":{"id":"turn","status":status}}}));
    }
    write_rows(path, rows).await?;
    import_spool(
        &service.store,
        path.parent().context("spool directory")?,
        path,
        source.descriptor.clone(),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn input_execution_requires_its_own_connection_and_survives_reopen_without_settling()
-> Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let evidence = prompt(
        service,
        "prompt",
        Event::CodexAccepted {
            thread_id: "native".into(),
            turn_id: "turn".into(),
            role: "prompt".into(),
        },
    )
    .await?;
    let selected = BTreeMap::from([("attempt".into(), evidence)]);
    let foreign = codex_source(service, &service.spool.join("foreign"), "native").await?;
    append_turn(service, &foreign).await?;
    let own = codex_source(service, &service.spool, "native").await?;
    let before = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert_eq!(before["attempt"].bindings.len(), 1);
    assert!(before["attempt"].bindings[0].execution.records.is_empty());
    assert_eq!(before["attempt"].bindings[0].execution.outcome, None);
    append_turn(service, &own).await?;
    let completed = service.native_inputs(&selected, &BTreeSet::new()).await?;
    let binding = &completed["attempt"].bindings[0];
    assert_eq!(binding.execution.outcome, Some(InputOutcome::Completed));
    let attempt = service
        .store
        .active_attempt(&binding.origin.session)
        .await?
        .context("attempt")?;
    assert_eq!(attempt.phase, AttemptPhase::Settling);
    assert!(
        attempt.members.is_empty()
            && attempt.latest_manifest.is_none()
            && attempt.effective_manifest.is_none()
    );

    drop(handle);
    let reopened = fixture(directory.path(), Harness::Codex).await?;
    let restored = reopened
        .service
        .native_inputs(&selected, &BTreeSet::new())
        .await?;
    assert_eq!(
        serde_json::to_value(&completed)?,
        serde_json::to_value(&restored)?
    );
    let db = crate::db::connect(&format!(
        "sqlite:{}",
        directory.path().join("router/evidence.db").display()
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "DELETE FROM native_evidence_records WHERE id = ?",
        [binding.execution.terminations[0]
            .record
            .record_id
            .clone()
            .into()],
    ))
    .await?;
    let missing = reopened
        .service
        .native_inputs(&selected, &BTreeSet::new())
        .await?;
    assert!(missing["attempt"].bindings.is_empty());
    assert!(
        missing["attempt"]
            .gaps
            .contains("native_input_evidence_invalid")
    );
    Ok(())
}

#[tokio::test]
async fn repeated_producers_share_an_aggregate_materialization_budget() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let evidence = prompt(
        service,
        "prompt",
        Event::CodexAccepted {
            thread_id: "native".into(),
            turn_id: "turn".into(),
            role: "prompt".into(),
        },
    )
    .await?;
    let native = codex_source(service, &service.spool, "native").await?;
    append_turn(service, &native).await?;
    let complete = service
        .native_inputs(
            &BTreeMap::from([("attempt".into(), evidence.clone())]),
            &BTreeSet::new(),
        )
        .await?;
    let bytes = serde_json::to_vec(&complete["attempt"].bindings[0].execution)?.len();
    let proven = evidence
        .observations
        .into_iter()
        .find(|proven| native_inputs::target(&proven.observation.event).is_some())
        .context("producer")?;
    let source = service
        .store
        .source(&proven.observation.origin.request.range.source_id)
        .await?
        .context("controller")?;
    let registration = service
        .store
        .records(&SourceRange {
            source_id: source.id.clone(),
            generation: "controller/1".into(),
            start: 0,
            end: 1,
        })
        .await?
        .into_iter()
        .next()
        .context("registration")?;
    let group = Group {
        source: source.clone(),
        spool: service.spool.clone(),
        registration: RecordRef::from_record(&registration)?,
        prompts: vec![("attempt".into(), proven); 3],
        inspected: vec![],
        gaps: BTreeSet::new(),
        bindings: vec![],
    };
    let mut groups = BTreeMap::from([(source.id.clone(), group)]);
    let (sources, gaps) = service.input_source_inventory(&groups).await?;
    assert!(gaps.is_empty());
    let mut group = groups.remove(&source.id).context("group")?;
    let mut record_budget = MAX_RECORDS as u64;
    let mut execution_budget = bytes * 2;
    let error = service
        .corroborate_inputs(
            &mut group,
            &sources,
            &mut record_budget,
            &mut execution_budget,
        )
        .await
        .err()
        .context("materialization should reject third clone")?;
    assert!(
        error
            .to_string()
            .contains("native execution materialization limit")
    );
    assert_eq!(group.bindings.len(), 2);
    assert_eq!(execution_budget, 0);
    // Final output uses the same depleted budget, not a new allowance per task.
    assert!(reserve_execution(&group.bindings[0].execution, &mut execution_budget).is_err());
    Ok(())
}
