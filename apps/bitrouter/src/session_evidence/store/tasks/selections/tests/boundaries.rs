use super::*;

// Build the durable prefix directly in one transaction so testing the real
// 1,024-attempt boundary does not repeatedly verify every growing prefix.
// Admissions below still use Journal and the production transaction path.
async fn seed_chain(store: &EvidenceStore, count: usize) -> Result<()> {
    store
        .seed_attempt_chain(root("public"), "fixture", count)
        .await
}

impl EvidenceStore {
    pub(crate) async fn seed_attempt_chain(
        &self,
        session: AcpSessionKey,
        controller: &str,
        count: usize,
    ) -> Result<()> {
        let store = self;
        let mut source = store
            .register(SourceDescriptor {
                namespace: session.namespace.clone(),
                harness: session.harness,
                format: SourceFormat::Acp,
                locator: format!("controller:{controller}"),
                node: None,
            })
            .await?;
        let transaction = store.db.begin().await?;
        let mut previous: Option<(ActiveTask, Attempt)> = None;
        for index in 0..count {
            let operation_id = format!("prompt-{index}");
            let id = PromptOperation::key(controller, &operation_id)?;
            let attempt_id = canonical_digest(&("attempt", &id))?;
            let mut selection_id = None;
            if let Some((old, attempt)) = &previous {
                let request_id = format!("select-{index}");
                let mut raw = select(&request_id, cursor(old, attempt), TaskSelectionMode::Retry);
                raw["payload"]["sessionId"] = json!(session.session_id);
                let request = serde_json::from_value(raw["payload"].clone())?;
                let record = append_raw(store, &transaction, &mut source, raw).await?;
                let selection = Selection {
                    id: canonical_digest(&(&session, request_id))?,
                    revision: 1,
                    session: session.clone(),
                    request,
                    record: RecordRef::from_record(&record)?,
                    consumed_by: Some(id.clone()),
                };
                let archived = ArchivedTask {
                    id: old.attempt_id.clone(),
                    revision: 0,
                    task: old.clone(),
                    selection: selection.id.clone(),
                };
                store
                    .insert_object(&transaction, "task_selection", &selection.id, 1, &selection)
                    .await?;
                store
                    .insert_object(&transaction, "task_archive", &archived.id, 0, &archived)
                    .await?;
                selection_id = Some(selection.id);
            }
            let request = append_raw(
                store,
                &transaction,
                &mut source,
                request(&operation_id, &session.session_id),
            )
            .await?;
            let response =
                append_raw(store, &transaction, &mut source, response(&operation_id)).await?;
            let task_origin = previous.as_ref().map(|(old, _)| {
                old.task_origin
                    .clone()
                    .unwrap_or_else(|| old.origin_operation.clone())
            });
            let attempt = Attempt {
                id: attempt_id.clone(),
                task_id: canonical_digest(&("task", task_origin.as_ref().unwrap_or(&id)))?,
                session: session.clone(),
                members: BTreeSet::new(),
                execution_snapshot: None,
                phase: AttemptPhase::Settling,
                revision: 1,
                latest_manifest: None,
                effective_manifest: None,
                started_at: "2026-09-07T00:00:00Z".into(),
            };
            let operation = PromptOperation {
                id: id.clone(),
                revision: 1,
                controller_id: controller.into(),
                operation_id,
                session: session.clone(),
                attempt_id: attempt_id.clone(),
                request: Boundary::new(&source, &request.input)?,
                response: Some(Boundary::new(&source, &response.input)?),
            };
            let task = ActiveTask {
                id: session.id()?,
                revision: previous.as_ref().map_or(1, |(old, _)| old.revision + 2),
                session: session.clone(),
                attempt_id,
                origin_operation: id.clone(),
                operations: BTreeSet::from([id.clone()]),
                open_operations: BTreeSet::new(),
                last_response: Some(id),
                task_origin,
                selection: selection_id,
            };
            task.validate()?;
            attempt.validate()?;
            operation.validate()?;
            store
                .insert_object(
                    &transaction,
                    "attempt",
                    &attempt.id,
                    i64::try_from(attempt.revision)?,
                    &attempt,
                )
                .await?;
            store
                .insert_object(
                    &transaction,
                    "prompt_operation",
                    &operation.id,
                    1,
                    &operation,
                )
                .await?;
            previous = Some((task, attempt));
        }
        let (task, _) = previous.context("nonempty fixture")?;
        store
            .insert_object(
                &transaction,
                "active_task",
                &task.id,
                i64::try_from(task.revision)?,
                &task,
            )
            .await?;
        transaction.commit().await?;
        Ok(())
    }
}

async fn append_raw(
    store: &EvidenceStore,
    db: &impl ConnectionTrait,
    source: &mut RegisteredSource,
    raw: Value,
) -> Result<StoredRecord> {
    let input = RecordInput {
        generation: "controller/1".into(),
        sequence: source.cursor.next_sequence,
        byte_start: None,
        byte_end: None,
        producer_version: Some("fixture/1".into()),
        raw,
    };
    let cursor = SourceCursor {
        generation: input.generation.clone(),
        offset: 0,
        next_sequence: input.sequence + 1,
        anchor_digest: canonical_digest(&(&source.cursor.anchor_digest, &input))?,
    };
    *source = store
        .append_on(db, source, std::slice::from_ref(&input), cursor)
        .await?;
    Ok(StoredRecord {
        id: input.id(&source.id)?,
        source_id: source.id.clone(),
        digest: canonical_digest(&input)?,
        input,
    })
}

#[tokio::test]
async fn the_last_supported_switch_stays_readable_and_overflow_rolls_back() -> Result<()> {
    let store = store().await?;
    let session = root("public");
    seed_chain(&store, MAX_GRAPH_ITEMS - 1).await?;
    let journal = journal(&store, "fixture", &session).await?;
    let expected = store
        .task_status(&session)
        .await?
        .current
        .context("cursor")?;
    journal
        .append(select("last", expected.clone(), TaskSelectionMode::Retry))
        .await?;
    journal.append(request("last", "public")).await?;
    journal.append(response("last")).await?;
    let mut full = store
        .task_status(&session)
        .await?
        .current
        .context("full chain")?;
    assert_ne!(full.attempt_id, expected.attempt_id);
    assert_eq!(full.task_id, expected.task_id);
    let mut source = store.sources(None, 1).await?.remove(0);
    for mode in [TaskSelectionMode::Retry, TaskSelectionMode::NewTask] {
        let failure = journal
            .append(select("overflow", full.clone(), mode))
            .await
            .err()
            .context("capacity rejection")?;
        assert!(failure.downcast_ref::<TaskSelectionRejected>().is_some());
        assert_eq!(store.source(&source.id).await?.context("source")?, source);
        let status = store.task_status(&session).await?;
        assert_eq!(status.current, Some(full.clone()));
        assert!(status.pending.is_none());
        crate::dashboard::tasks::tests::rejected_selection_keeps_the_current_task_usable(
            crate::session_evidence::service::tasks::selection_error(failure),
            status,
        )
        .await?;
    }

    // Refusing another attempt does not prevent more work in this attempt.
    journal
        .append(request("continue-current", "public"))
        .await?;
    journal.append(response("continue-current")).await?;
    let continued = store
        .task_status(&session)
        .await?
        .current
        .context("continued task")?;
    assert_eq!(continued.attempt_id, full.attempt_id);
    full = continued;
    source = store
        .source(&source.id)
        .await?
        .context("continued source")?;

    // An older writer could have accepted an over-capacity reservation. The
    // consuming prompt must still reject it atomically, without an orphaned
    // attempt, archive, raw prompt or advanced source cursor.
    let raw = select("imported-pending", full.clone(), TaskSelectionMode::Retry);
    let request = serde_json::from_value(raw["payload"].clone())?;
    let transaction = store.db.begin().await?;
    let record = append_raw(&store, &transaction, &mut source, raw).await?;
    let selection = Selection {
        id: canonical_digest(&(&session, "imported-pending"))?,
        revision: 0,
        session: session.clone(),
        request,
        record: RecordRef::from_record(&record)?,
        consumed_by: None,
    };
    let pending = PendingSelection {
        id: session.id()?,
        revision: 0,
        selection: selection.id.clone(),
    };
    store
        .insert_object(&transaction, "task_selection", &selection.id, 0, &selection)
        .await?;
    store
        .insert_object(
            &transaction,
            "pending_task_selection",
            &pending.id,
            0,
            &pending,
        )
        .await?;
    transaction.commit().await?;
    assert!(
        journal
            .append(super::request("overflow-prompt", "public"))
            .await
            .is_err()
    );
    assert_eq!(store.source(&source.id).await?.context("source")?, source);
    let status = store.task_status(&session).await?;
    assert_eq!(status.current, Some(full.clone()));
    assert_eq!(
        status.pending.context("preserved reservation")?.request_id,
        "imported-pending"
    );
    assert!(
        store
            .object(&store.db, "task_archive", &full.attempt_id)
            .await?
            .is_none()
    );
    let operation = PromptOperation::key("fixture", "overflow-prompt")?;
    assert!(
        store
            .prompt_operation(&store.db, &operation)
            .await?
            .is_none()
    );
    assert!(
        store
            .attempt(&canonical_digest(&("attempt", &operation))?)
            .await?
            .is_none()
    );
    let oldest = store
        .prompt_operation(&store.db, &PromptOperation::key("fixture", "prompt-0")?)
        .await?
        .context("oldest operation")?;
    record_entity::Entity::delete_by_id(oldest.request.record_id)
        .exec(&store.db)
        .await?;
    let damaged = journal
        .append(select("damaged-full-chain", full, TaskSelectionMode::Retry))
        .await
        .err()
        .context("damaged proof")?;
    assert!(damaged.downcast_ref::<TaskSelectionRejected>().is_none());
    let classified = crate::session_evidence::service::tasks::selection_error(damaged);
    assert_eq!(
        classified.data.context("unknown outcome")?["outcome"],
        "unknown"
    );
    Ok(())
}
