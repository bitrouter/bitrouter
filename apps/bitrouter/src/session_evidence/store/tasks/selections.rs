//! Durable next-prompt selections. Switching closes an application's prompt
//! membership ledger, not native/background execution or evaluation settlement.

use super::*;
use crate::session_evidence::types::RecordRef;
use bitrouter_sdk::acp::controller::tasks::{
    PendingTaskSelection, TaskCursor, TaskSelectRequest, TaskSelectionMode, TaskStatusResponse,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    id: String,
    revision: u64,
    session: AcpSessionKey,
    request: TaskSelectRequest,
    record: RecordRef,
    consumed_by: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingSelection {
    id: String,
    revision: u64,
    selection: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchivedTask {
    id: String,
    revision: u64,
    task: ActiveTask,
    selection: String,
}

fn cursor(task: &ActiveTask, attempt: &Attempt) -> TaskCursor {
    TaskCursor {
        task_id: attempt.task_id.clone(),
        attempt_id: attempt.id.clone(),
        revision: task.revision,
    }
}

impl EvidenceStore {
    pub(super) async fn lock_active_task(
        &self,
        db: &impl ConnectionTrait,
        session: &AcpSessionKey,
    ) -> Result<Option<ActiveTask>> {
        // Both prompt admission and selection lock this same session row.
        // A journal-source lock only serializes one controller. Use a locking
        // read so PostgreSQL/MySQL cannot reserve an already superseded cursor.
        let keys = [
            self.object_id("active_task", &session.id()?)?,
            self.object_id("active_task", &legacy_session_id(session)?)?,
        ];
        let rows = object_entity::Entity::find()
            .filter(object_entity::Column::Owner.eq(&self.owner_key))
            .filter(object_entity::Column::Id.is_in(keys))
            .order_by_asc(object_entity::Column::Id)
            .lock_exclusive()
            .all(db)
            .await?;
        ensure!(rows.len() <= 1, "conflicting ACP task identities");
        let task = rows
            .into_iter()
            .next()
            .map(decode_task_object::<ActiveTask>)
            .transpose()?;
        if let Some(task) = &task {
            task.validate()?;
            ensure!(&task.session == session, "locked task session mismatch");
        }
        Ok(task)
    }

    async fn selection_on(&self, db: &impl ConnectionTrait, id: &str) -> Result<Option<Selection>> {
        let Some(row) = self.object(db, "task_selection", id).await? else {
            return Ok(None);
        };
        let selection: Selection = decode_object(row)?;
        selection.session.validate()?;
        identifier(&selection.request.request_id)?;
        digest_identifier(&selection.request.expected.task_id)?;
        digest_identifier(&selection.request.expected.attempt_id)?;
        ensure!(
            selection.id == canonical_digest(&(&selection.session, &selection.request.request_id))?
                && selection.request.session_id == selection.session.session_id
                && selection.revision == u64::from(selection.consumed_by.is_some()),
            "invalid task selection identity"
        );
        if let Some(id) = &selection.consumed_by {
            digest_identifier(id)?;
        }
        selection.record.validate()?;
        let source = source_entity::Entity::find_by_id(&selection.record.range.source_id)
            .filter(source_entity::Column::Owner.eq(&self.owner_key))
            .one(db)
            .await?
            .context("selection source missing")?;
        let source = decode_source(source)?;
        ensure!(
            source.descriptor.namespace == selection.session.namespace
                && source.descriptor.harness == selection.session.harness
                && source.descriptor.format == SourceFormat::Acp
                && source.descriptor.node.is_none()
                && source
                    .descriptor
                    .locator
                    .strip_prefix("controller:")
                    .is_some_and(|id| !id.is_empty()),
            "selection source scope mismatch"
        );
        let records = range_records(db, &self.owner_key, &selection.record.range).await?;
        let record = records.first().context("selection record missing")?;
        ensure!(
            records.len() == 1 && RecordRef::from_record(record)? == selection.record,
            "selection original record changed"
        );
        let raw = &record.input.raw;
        ensure!(
            raw.get("method").and_then(Value::as_str) == Some("_bitrouter/task/select")
                && raw.get("phase").and_then(Value::as_str) == Some("request")
                && raw.get("native_scope").and_then(Value::as_str) == Some("session")
                && raw.get("operation_id").and_then(Value::as_str)
                    == Some(selection.request.request_id.as_str())
                && serde_json::from_value::<TaskSelectRequest>(
                    raw.get("payload")
                        .cloned()
                        .context("selection payload missing")?
                )? == selection.request,
            "selection boundary provenance mismatch"
        );
        Ok(Some(selection))
    }

    async fn pending_selection(
        &self,
        db: &impl ConnectionTrait,
        session: &AcpSessionKey,
    ) -> Result<Option<Selection>> {
        let Some(row) = self
            .object(db, "pending_task_selection", &session.id()?)
            .await?
        else {
            return Ok(None);
        };
        let pending: PendingSelection = decode_object(row)?;
        ensure!(pending.revision == 0, "invalid pending selection revision");
        let selection = self
            .selection_on(db, &pending.selection)
            .await?
            .context("pending selection missing")?;
        ensure!(
            &selection.session == session && selection.consumed_by.is_none(),
            "invalid pending selection scope"
        );
        Ok(Some(selection))
    }

    pub(super) async fn queue_task_selection(
        &self,
        db: &impl ConnectionTrait,
        source: &RegisteredSource,
        record: &RecordInput,
    ) -> Result<()> {
        ensure!(
            record.raw.get("phase").and_then(Value::as_str) == Some("request")
                && record.raw.get("native_scope").and_then(Value::as_str) == Some("session"),
            "task selection requires a confirmed session"
        );
        let request: TaskSelectRequest = serde_json::from_value(
            record
                .raw
                .get("payload")
                .cloned()
                .context("selection request missing")?,
        )?;
        identifier(&request.request_id)?;
        ensure!(
            record.raw.get("operation_id").and_then(Value::as_str)
                == Some(request.request_id.as_str()),
            "selection request id mismatch"
        );
        let session = AcpSessionKey {
            namespace: source.descriptor.namespace.clone(),
            harness: source.descriptor.harness,
            session_id: request.session_id.clone(),
        };
        session.validate()?;
        let id = canonical_digest(&(&session, &request.request_id))?;
        if let Some(old) = self.selection_on(db, &id).await? {
            ensure!(
                old.request == request,
                "task selection id was reused for different work"
            );
            return Ok(());
        }
        let task = self
            .lock_active_task(db, &session)
            .await?
            .context("task selection needs an existing task")?;
        if let Some(old) = self.selection_on(db, &id).await? {
            ensure!(
                old.request == request,
                "task selection id was reused for different work"
            );
            return Ok(());
        }
        let attempt = self.task_attempt_with_capacity(db, &task, 1).await?;
        ensure!(
            cursor(&task, &attempt) == request.expected,
            "task changed; refresh before selecting"
        );
        ensure!(
            task.open_operations.is_empty(),
            "a prompt is still outstanding"
        );
        ensure!(
            self.pending_selection(db, &session).await?.is_none(),
            "a task selection is already pending"
        );
        let raw = StoredRecord {
            id: record.id(&source.id)?,
            source_id: source.id.clone(),
            digest: canonical_digest(record)?,
            input: record.clone(),
        };
        let selection = Selection {
            id: id.clone(),
            revision: 0,
            session: session.clone(),
            request,
            record: RecordRef::from_record(&raw)?,
            consumed_by: None,
        };
        self.insert_object(db, "task_selection", &id, 0, &selection)
            .await?;
        let pending = PendingSelection {
            id: session.id()?,
            revision: 0,
            selection: id,
        };
        self.insert_object(db, "pending_task_selection", &pending.id, 0, &pending)
            .await
    }

    pub(super) async fn consume_task_selection(
        &self,
        db: &impl ConnectionTrait,
        old: &ActiveTask,
        old_attempt: &Attempt,
        operation: &PromptOperation,
        started_at: &str,
    ) -> Result<bool> {
        let Some(mut selection) = self.pending_selection(db, &old.session).await? else {
            return Ok(false);
        };
        self.task_attempt_with_capacity(db, old, 1).await?;
        ensure!(
            old.open_operations.is_empty()
                && selection.request.expected == cursor(old, old_attempt),
            "pending task selection is stale"
        );
        let task_origin = match selection.request.mode {
            TaskSelectionMode::NewTask => None,
            TaskSelectionMode::Retry => Some(
                old.task_origin
                    .clone()
                    .unwrap_or_else(|| old.origin_operation.clone()),
            ),
        };
        let attempt = Attempt {
            id: operation.attempt_id.clone(),
            task_id: match selection.request.mode {
                TaskSelectionMode::NewTask => canonical_digest(&("task", &operation.id))?,
                TaskSelectionMode::Retry => old_attempt.task_id.clone(),
            },
            session: old.session.clone(),
            members: BTreeSet::new(),
            phase: AttemptPhase::Collecting,
            revision: 0,
            latest_manifest: None,
            effective_manifest: None,
            started_at: started_at.into(),
        };
        attempt.validate()?;
        let archived = ArchivedTask {
            id: old.attempt_id.clone(),
            revision: 0,
            task: old.clone(),
            selection: selection.id.clone(),
        };
        self.insert_object(db, "task_archive", &archived.id, 0, &archived)
            .await?;
        let task = ActiveTask {
            id: old.id.clone(),
            revision: old
                .revision
                .checked_add(1)
                .context("task revision overflow")?,
            session: old.session.clone(),
            attempt_id: attempt.id.clone(),
            origin_operation: operation.id.clone(),
            operations: BTreeSet::from([operation.id.clone()]),
            open_operations: BTreeSet::from([operation.id.clone()]),
            last_response: None,
            task_origin,
            selection: Some(selection.id.clone()),
        };
        task.validate()?;
        operation.validate()?;
        self.insert_object(db, "attempt", &attempt.id, 0, &attempt)
            .await?;
        self.insert_object(db, "prompt_operation", &operation.id, 0, operation)
            .await?;
        self.replace_task_object(db, "active_task", &task.id, old.revision, &task)
            .await?;
        selection.consumed_by = Some(operation.id.clone());
        selection.revision = 1;
        self.replace_task_object(db, "task_selection", &selection.id, 0, &selection)
            .await?;
        let deleted = object_entity::Entity::delete_by_id(
            self.object_id("pending_task_selection", &old.session.id()?)?,
        )
        .filter(object_entity::Column::Owner.eq(&self.owner_key))
        .exec(db)
        .await?;
        ensure!(deleted.rows_affected == 1, "pending selection disappeared");
        Ok(true)
    }

    pub(super) async fn selected_task_origin(
        &self,
        db: &impl ConnectionTrait,
        task: &ActiveTask,
    ) -> Result<(String, Option<ActiveTask>)> {
        let Some(id) = &task.selection else {
            ensure!(task.task_origin.is_none(), "unselected retry origin");
            return Ok((task.origin_operation.clone(), None));
        };
        let selection = self
            .selection_on(db, id)
            .await?
            .context("task selection missing")?;
        ensure!(
            selection.session == task.session
                && selection.consumed_by.as_ref() == Some(&task.origin_operation),
            "task selection consumption mismatch"
        );
        let archived: ArchivedTask = decode_object(
            self.object(db, "task_archive", &selection.request.expected.attempt_id)
                .await?
                .context("prior task archive missing")?,
        )?;
        archived.task.validate()?;
        ensure!(
            archived.revision == 0
                && archived.id == archived.task.attempt_id
                && archived.selection == selection.id
                && archived.task.session == task.session
                && archived.task.revision == selection.request.expected.revision
                && archived.task.open_operations.is_empty(),
            "invalid prior task archive"
        );
        ensure!(
            canonical_digest(&(
                "task",
                archived
                    .task
                    .task_origin
                    .as_ref()
                    .unwrap_or(&archived.task.origin_operation)
            ))? == selection.request.expected.task_id,
            "prior task identity does not match the selection"
        );
        match selection.request.mode {
            TaskSelectionMode::NewTask => {
                ensure!(
                    task.task_origin.is_none(),
                    "new task retained an old task origin"
                );
                Ok((task.origin_operation.clone(), Some(archived.task)))
            }
            TaskSelectionMode::Retry => {
                let origin = task.task_origin.as_ref().context("retry origin missing")?;
                ensure!(
                    origin
                        == archived
                            .task
                            .task_origin
                            .as_ref()
                            .unwrap_or(&archived.task.origin_operation)
                        && canonical_digest(&("task", origin))?
                            == selection.request.expected.task_id,
                    "retry task origin mismatch"
                );
                Ok((origin.clone(), Some(archived.task)))
            }
        }
    }

    pub(crate) async fn task_status(&self, session: &AcpSessionKey) -> Result<TaskStatusResponse> {
        let transaction = self.read_snapshot().await?;
        let current = match self.active_task(&transaction, session).await? {
            Some(task) => Some((task.clone(), self.task_attempt(&transaction, &task).await?)),
            None => None,
        };
        let pending = self.pending_selection(&transaction, session).await?;
        if let Some(selection) = &pending {
            let (task, attempt) = current.as_ref().context("pending selection has no task")?;
            ensure!(
                selection.request.expected == cursor(task, attempt)
                    && task.open_operations.is_empty(),
                "pending selection no longer matches its task"
            );
        }
        let status = TaskStatusResponse {
            current: current
                .as_ref()
                .map(|(task, attempt)| cursor(task, attempt)),
            phase: current.as_ref().map(|(_, attempt)| {
                match attempt.phase {
                    AttemptPhase::Collecting => "collecting",
                    AttemptPhase::Settling => "settling",
                    AttemptPhase::Ready => "ready",
                    AttemptPhase::Partial => "partial",
                }
                .into()
            }),
            pending: pending.map(|selection| PendingTaskSelection {
                request_id: selection.request.request_id,
                mode: selection.request.mode,
            }),
        };
        transaction.commit().await?;
        Ok(status)
    }
}

#[cfg(test)]
mod tests;
