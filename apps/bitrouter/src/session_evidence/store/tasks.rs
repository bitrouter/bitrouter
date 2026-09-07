//! Application task identity follows confirmed ACP session scopes. Prompt RPC
//! boundaries are durable operation membership, never native completion proof.

use sea_orm::{AccessMode, DatabaseTransaction, DbBackend, IsolationLevel};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::*;
use crate::session_evidence::types::{AcpSessionKey, Harness, NodeKey, SourceFormat};

/// Read the pre-separation mutable task objects without carrying their assumed
/// native membership forward. Their raw boundaries are still verified by the
/// task reader. Immutable manifests and their serialized bytes are untouched.
pub(super) fn decode_task_object<T: serde::de::DeserializeOwned + Serialize>(
    row: object_entity::Model,
) -> Result<T> {
    let kind = row.kind.clone();
    ensure!(
        matches!(
            kind.as_str(),
            "attempt" | "active_task" | "prompt_operation"
        ),
        "unexpected task object kind"
    );
    let mut value: Value = decode_object(row)?;
    if let Some(root) = value.get("root").cloned() {
        ensure!(
            value.get("session").is_none(),
            "mixed task identity formats"
        );
        let root: NodeKey = serde_json::from_value(root)?;
        root.validate()?;
        ensure!(
            root.agent_id.is_none(),
            "legacy ACP task has an agent identity"
        );
        if kind == "attempt" {
            let members: BTreeSet<NodeKey> = serde_json::from_value(
                value
                    .get("members")
                    .cloned()
                    .context("legacy attempt members missing")?,
            )?;
            ensure!(
                members == BTreeSet::from([root.clone()])
                    && matches!(
                        value.get("phase").and_then(Value::as_str),
                        Some("collecting" | "settling")
                    )
                    && value.get("latest_manifest") == Some(&Value::Null)
                    && value.get("effective_manifest") == Some(&Value::Null),
                "legacy native attempt requires explicit migration"
            );
            value["members"] = serde_json::json!([]);
        }
        let fields = value.as_object_mut().context("invalid task object")?;
        fields.remove("root");
        fields.insert(
            "session".into(),
            serde_json::to_value(AcpSessionKey {
                namespace: root.namespace,
                harness: root.harness,
                session_id: root.native_id,
            })?,
        );
    }
    Ok(serde_json::from_value(value)?)
}

fn legacy_session_id(session: &AcpSessionKey) -> Result<String> {
    // This is only the old mutable object's lookup key, never native identity
    // evidence. New objects use AcpSessionKey::id and a distinct JSON shape.
    NodeKey {
        namespace: session.namespace.clone(),
        harness: session.harness,
        native_id: session.session_id.clone(),
        agent_id: None,
    }
    .id()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveTask {
    id: String,
    revision: u64,
    session: AcpSessionKey,
    attempt_id: String,
    origin_operation: String,
    operations: BTreeSet<String>,
    open_operations: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_response: Option<String>,
}

impl ActiveTask {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.id == self.session.id()? || self.id == legacy_session_id(&self.session)?,
            "active task session mismatch"
        );
        digest_identifier(&self.attempt_id)?;
        ensure!(
            self.operations.len() <= MAX_GRAPH_ITEMS
                && self.operations.contains(&self.origin_operation)
                && self.open_operations.is_subset(&self.operations),
            "invalid bounded task operation set"
        );
        ensure!(self.last_response.as_ref().is_none_or(|id| self.operations.contains(id) && !self.open_operations.contains(id)), "invalid last prompt response");
        for operation in &self.operations {
            digest_identifier(operation)?;
        }
        Ok(())
    }
}

/// An exact reference to the observation committed with the task transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Boundary {
    range: SourceRange,
    record_id: String,
    record_digest: String,
    semantic_digest: String,
}

impl Boundary {
    fn new(source: &RegisteredSource, record: &RecordInput) -> Result<Self> {
        Ok(Self {
            range: SourceRange {
                source_id: source.id.clone(),
                generation: record.generation.clone(),
                start: record.sequence,
                end: record.sequence + 1,
            },
            record_id: record.id(&source.id)?,
            record_digest: canonical_digest(record)?,
            // Retransmission changes observation time and source position,
            // but may not change the operation's method, phase or payload.
            semantic_digest: canonical_digest(&(
                record.raw.get("method"),
                record.raw.get("phase"),
                record.raw.get("payload"),
            ))?,
        })
    }

    fn validate(&self) -> Result<()> {
        self.range.validate()?;
        ensure!(
            self.range.end - self.range.start == 1
                && self.record_id
                    == canonical_digest(&(
                        &self.range.source_id,
                        &self.range.generation,
                        self.range.start
                    ))?,
            "invalid task boundary record"
        );
        digest_identifier(&self.record_digest)?;
        digest_identifier(&self.semantic_digest)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptOperation {
    id: String,
    revision: u64,
    controller_id: String,
    operation_id: String,
    session: AcpSessionKey,
    attempt_id: String,
    request: Boundary,
    response: Option<Boundary>,
}

impl PromptOperation {
    fn key(controller: &str, operation: &str) -> Result<String> {
        identifier(controller)?;
        identifier(operation)?;
        canonical_digest(&(controller, operation))
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.id == Self::key(&self.controller_id, &self.operation_id)?,
            "prompt operation identity mismatch"
        );
        self.session.validate()?;
        digest_identifier(&self.attempt_id)?;
        self.request.validate()?;
        ensure!(
            self.revision == u64::from(self.response.is_some()),
            "invalid prompt operation revision"
        );
        if let Some(response) = &self.response {
            response.validate()?;
        }
        Ok(())
    }
}

impl EvidenceStore {
    /// Discover logical tasks from committed state, including a prompt whose
    /// observer was cancelled before updating its process-local caches. These
    /// are candidates: active_attempt verifies the original prompt boundaries.
    pub(crate) async fn task_sessions(
        &self,
        harness: Harness,
        namespaces: &BTreeSet<String>,
    ) -> Result<(BTreeSet<AcpSessionKey>, BTreeSet<String>)> {
        let mut sessions = BTreeSet::new();
        let mut gaps = BTreeSet::new();
        let mut after = None;
        let mut inspected = 0;
        loop {
            let mut query = object_entity::Entity::find()
                .filter(object_entity::Column::Owner.eq(&self.owner_key))
                .filter(object_entity::Column::Kind.eq("active_task"))
                .order_by_asc(object_entity::Column::Id)
                .limit(16);
            if let Some(id) = &after {
                query = query.filter(object_entity::Column::Id.gt(id));
            }
            let rows = query.all(&self.db).await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                if inspected == MAX_GRAPH_ITEMS {
                    gaps.insert("native_task_session_limit".into());
                    return Ok((sessions, gaps));
                }
                inspected += 1;
                after = Some(row.id.clone());
                let parsed = decode_task_object::<ActiveTask>(row).and_then(|task| {
                    task.validate()?;
                    Ok(task.session)
                });
                match parsed {
                    Ok(session)
                        if session.harness == harness
                            && namespaces.contains(&session.namespace) =>
                    {
                        sessions.insert(session);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%error, "ACP task session could not be read");
                        gaps.insert("native_task_state_invalid".into());
                    }
                }
            }
        }
        Ok((sessions, gaps))
    }

    /// Append the raw observation, its source cursor and the task transition
    /// in one transaction. A failed transition cannot leave a forwarded prompt
    /// without task membership. Generic native backfill does not create tasks.
    pub(crate) async fn append_observation(
        &self,
        source: &RegisteredSource,
        record: RecordInput,
        cursor: SourceCursor,
    ) -> Result<RegisteredSource> {
        ensure!(
            source.descriptor.format == SourceFormat::Acp && source.descriptor.node.is_none(),
            "task observations require a controller journal"
        );
        let transaction = self.db.begin().await?;
        let next = self
            .append_on(&transaction, source, std::slice::from_ref(&record), cursor)
            .await?;
        self.index_lifecycle_record(&transaction, source, &record)
            .await?;
        // append_on acquires SQLite's writer lock before any reads. Starting
        // with artifact SELECTs would introduce a read-to-write upgrade race.
        if let Some(workspace) = record.raw.get("workspace_artifact") {
            self.workspace_on(
                &transaction,
                workspace.as_str().context("invalid workspace reference")?,
            )
            .await?;
        }
        if let Some(id) = record.raw.get("native_checkpoint") {
            ensure!(
                record.raw.get("method").and_then(Value::as_str) == Some("session/prompt"),
                "native checkpoint requires a prompt boundary"
            );
            let controller = source
                .descriptor
                .locator
                .strip_prefix("controller:")
                .context("checkpoint controller missing")?;
            let operation = record
                .raw
                .get("operation_id")
                .and_then(Value::as_str)
                .context("checkpoint operation missing")?;
            let phase = record
                .raw
                .get("phase")
                .and_then(Value::as_str)
                .context("checkpoint phase missing")?;
            let session = if phase == "request" {
                ensure!(
                    record.raw.get("native_scope").and_then(Value::as_str) == Some("session"),
                    "checkpoint request scope is unconfirmed"
                );
                AcpSessionKey {
                    namespace: source.descriptor.namespace.clone(),
                    harness: source.descriptor.harness,
                    session_id: record
                        .raw
                        .pointer("/payload/sessionId")
                        .and_then(Value::as_str)
                        .context("checkpoint ACP session missing")?
                        .into(),
                }
            } else {
                ensure!(phase == "response", "invalid prompt checkpoint phase");
                self.prompt_operation(&transaction, &PromptOperation::key(controller, operation)?)
                    .await?
                    .context("checkpoint response has no original prompt")?
                    .session
            };
            let checkpoint = self
                .checkpoint_on(
                    &transaction,
                    id.as_str().context("invalid checkpoint reference")?,
                )
                .await?;
            ensure!(
                checkpoint.controller_id == controller
                    && checkpoint.operation_id == operation
                    && checkpoint.phase == phase
                    && checkpoint.session == session,
                "checkpoint does not belong to this prompt boundary"
            );
        }
        if record.raw.get("method").and_then(Value::as_str) == Some("session/prompt") {
            let controller = source
                .descriptor
                .locator
                .strip_prefix("controller:")
                .context("prompt controller id missing")?;
            let operation = record
                .raw
                .get("operation_id")
                .and_then(Value::as_str)
                .context("prompt operation id missing")?;
            let key = PromptOperation::key(controller, operation)?;
            let boundary = Boundary::new(source, &record)?;
            match record.raw.get("phase").and_then(Value::as_str) {
                Some("request")
                    if record.raw.get("native_scope").and_then(Value::as_str)
                        == Some("session") =>
                {
                    let session = record
                        .raw
                        .pointer("/payload/sessionId")
                        .and_then(Value::as_str)
                        .context("confirmed prompt session missing")?;
                    let session = AcpSessionKey {
                        namespace: source.descriptor.namespace.clone(),
                        harness: source.descriptor.harness,
                        session_id: session.into(),
                    };
                    session.validate()?;
                    let started_at = record
                        .raw
                        .get("observed_at")
                        .and_then(Value::as_str)
                        .context("prompt observation time missing")?;
                    let attempt_id = canonical_digest(&("attempt", &key))?;
                    self.begin_prompt(
                        &transaction,
                        PromptOperation {
                            id: key,
                            revision: 0,
                            controller_id: controller.into(),
                            operation_id: operation.into(),
                            session,
                            attempt_id,
                            request: boundary,
                            response: None,
                        },
                        started_at,
                    )
                    .await?;
                }
                Some("response") => {
                    self.finish_prompt(&transaction, &key, source, boundary)
                        .await?;
                }
                // Unknown scopes remain raw evidence. Notification ids and
                // historical imports cannot mint an application task.
                _ => {}
            }
        }
        transaction.commit().await?;
        Ok(next)
    }

    async fn begin_prompt(
        &self,
        db: &impl ConnectionTrait,
        mut operation: PromptOperation,
        started_at: &str,
    ) -> Result<()> {
        if let Some(old) = self.prompt_operation(db, &operation.id).await? {
            ensure!(
                old.session == operation.session
                    && old.request.semantic_digest == operation.request.semantic_digest,
                "prompt operation was reused for different work"
            );
            return Ok(());
        }
        let key = operation.session.id()?;
        let active = self.active_task(db, &operation.session).await?;
        let (mut task, mut attempt) = match active {
            Some(task) => {
                let attempt = self.task_attempt(db, &task).await?;
                (task, attempt)
            }
            None => {
                let attempt = Attempt {
                    id: operation.attempt_id.clone(),
                    task_id: canonical_digest(&("task", &operation.id))?,
                    session: operation.session.clone(),
                    members: BTreeSet::new(),
                    phase: AttemptPhase::Collecting,
                    revision: 0,
                    latest_manifest: None,
                    effective_manifest: None,
                    started_at: started_at.into(),
                };
                attempt.validate()?;
                let task = ActiveTask {
                    id: key,
                    revision: 0,
                    session: operation.session.clone(),
                    attempt_id: attempt.id.clone(),
                    origin_operation: operation.id.clone(),
                    operations: BTreeSet::from([operation.id.clone()]),
                    open_operations: BTreeSet::from([operation.id.clone()]),
                    last_response: None,
                };
                task.validate()?;
                operation.validate()?;
                self.insert_object(db, "attempt", &attempt.id, 0, &attempt)
                    .await?;
                self.insert_object(db, "active_task", &task.id, 0, &task)
                    .await?;
                self.insert_object(db, "prompt_operation", &operation.id, 0, &operation)
                    .await?;
                return Ok(());
            }
        };
        operation.attempt_id.clone_from(&attempt.id);
        operation.validate()?;
        task.operations.insert(operation.id.clone());
        task.open_operations.insert(operation.id.clone());
        task.validate()?;
        attempt.phase = AttemptPhase::Collecting;
        attempt.effective_manifest = None;
        self.advance_task(db, &mut task, &mut attempt).await?;
        self.insert_object(db, "prompt_operation", &operation.id, 0, &operation)
            .await
    }

    async fn finish_prompt(
        &self,
        db: &impl ConnectionTrait,
        key: &str,
        source: &RegisteredSource,
        response: Boundary,
    ) -> Result<()> {
        let Some(mut operation) = self.prompt_operation(db, key).await? else {
            // An unbound or pre-instrumentation request has no task claim.
            return Ok(());
        };
        ensure!(
            operation.session.harness == source.descriptor.harness,
            "prompt response harness mismatch"
        );
        if let Some(old) = &operation.response {
            ensure!(
                old.semantic_digest == response.semantic_digest,
                "prompt operation has conflicting responses"
            );
            return Ok(());
        }
        let mut task = self
            .active_task(db, &operation.session)
            .await?
            .context("prompt task disappeared")?;
        ensure!(
            task.attempt_id == operation.attempt_id && task.open_operations.contains(&operation.id),
            "prompt is outside the active attempt"
        );
        let mut attempt = self.task_attempt(db, &task).await?;
        task.open_operations.remove(&operation.id);
        task.last_response = Some(operation.id.clone());
        // ACP completion only ends this RPC. Native children, background work,
        // artifact capture and gateway settlement still decide readiness.
        // https://agentclientprotocol.com/protocol/v1/prompt-turn
        attempt.phase = if task.open_operations.is_empty() {
            AttemptPhase::Settling
        } else {
            AttemptPhase::Collecting
        };
        attempt.effective_manifest = None;
        operation.response = Some(response);
        operation.revision = 1;
        operation.validate()?;
        self.advance_task(db, &mut task, &mut attempt).await?;
        self.replace_task_object(db, "prompt_operation", &operation.id, 0, &operation)
            .await
    }

    async fn advance_task(
        &self,
        db: &impl ConnectionTrait,
        task: &mut ActiveTask,
        attempt: &mut Attempt,
    ) -> Result<()> {
        let previous_task = task.revision;
        let previous_attempt = attempt.revision;
        task.revision = task
            .revision
            .checked_add(1)
            .context("task revision overflow")?;
        attempt.revision = attempt
            .revision
            .checked_add(1)
            .context("attempt revision overflow")?;
        task.validate()?;
        attempt.validate()?;
        self.replace_task_object(db, "attempt", &attempt.id, previous_attempt, attempt)
            .await?;
        self.replace_task_object(db, "active_task", &task.id, previous_task, task)
            .await
    }

    async fn prompt_operation(
        &self,
        db: &impl ConnectionTrait,
        id: &str,
    ) -> Result<Option<PromptOperation>> {
        let operation: Option<PromptOperation> = self
            .object(db, "prompt_operation", id)
            .await?
            .map(decode_task_object)
            .transpose()?;
        if let Some(operation) = &operation {
            operation.validate()?;
            self.verify_boundary(db, operation, &operation.request, "request")
                .await?;
            if let Some(response) = &operation.response {
                self.verify_boundary(db, operation, response, "response")
                    .await?;
            }
        }
        Ok(operation)
    }

    async fn verify_boundary(
        &self,
        db: &impl ConnectionTrait,
        operation: &PromptOperation,
        boundary: &Boundary,
        phase: &str,
    ) -> Result<()> {
        let row = source_entity::Entity::find_by_id(&boundary.range.source_id)
            .filter(source_entity::Column::Owner.eq(&self.owner_key))
            .one(db)
            .await?
            .context("prompt boundary source missing")?;
        ensure!(
            row.owner == self.owner_key,
            "foreign prompt boundary source"
        );
        let source = decode_source(row)?;
        ensure!(
            source.descriptor.format == SourceFormat::Acp
                && source.descriptor.node.is_none()
                && source.descriptor.harness == operation.session.harness
                && source.descriptor.locator == format!("controller:{}", operation.controller_id),
            "prompt boundary source mismatch"
        );
        let records = range_records(db, &self.owner_key, &boundary.range).await?;
        let record = records.first().context("prompt boundary record missing")?;
        ensure!(
            records.len() == 1
                && record.id == boundary.record_id
                && record.digest == boundary.record_digest
                && record.input.raw.get("operation_id").and_then(Value::as_str)
                    == Some(operation.operation_id.as_str())
                && record.input.raw.get("method").and_then(Value::as_str) == Some("session/prompt")
                && record.input.raw.get("phase").and_then(Value::as_str) == Some(phase)
                && Boundary::new(&source, &record.input)?.semantic_digest
                    == boundary.semantic_digest,
            "prompt boundary provenance mismatch"
        );
        if phase == "request" {
            ensure!(
                source.descriptor.namespace == operation.session.namespace
                    && record.input.raw.get("native_scope").and_then(Value::as_str)
                        == Some("session")
                    && record
                        .input
                        .raw
                        .pointer("/payload/sessionId")
                        .and_then(Value::as_str)
                        == Some(operation.session.session_id.as_str()),
                "prompt boundary ACP identity mismatch"
            );
        }
        if let Some(workspace) = record.input.raw.get("workspace_artifact") {
            // The immutable reference was admitted with this raw record. Read
            // and verify artifact bodies when selecting a checkpoint, rather
            // than loading every historical filesystem image on every prompt.
            digest_identifier(workspace.as_str().context("invalid workspace reference")?)?;
        }
        if let Some(checkpoint) = record.input.raw.get("native_checkpoint") {
            digest_identifier(
                checkpoint
                    .as_str()
                    .context("invalid checkpoint reference")?,
            )?;
        }
        Ok(())
    }

    async fn active_task(
        &self,
        db: &impl ConnectionTrait,
        session: &AcpSessionKey,
    ) -> Result<Option<ActiveTask>> {
        session.validate()?;
        let current = self.object(db, "active_task", &session.id()?).await?;
        let legacy = self
            .object(db, "active_task", &legacy_session_id(session)?)
            .await?;
        ensure!(
            current.is_none() || legacy.is_none(),
            "conflicting ACP task identities"
        );
        let task: Option<ActiveTask> = current.or(legacy).map(decode_task_object).transpose()?;
        if let Some(task) = &task {
            task.validate()?;
            ensure!(&task.session == session, "active task identity mismatch");
        }
        Ok(task)
    }

    async fn task_attempt(&self, db: &impl ConnectionTrait, task: &ActiveTask) -> Result<Attempt> {
        let attempt: Attempt = decode_task_object(
            self.object(db, "attempt", &task.attempt_id)
                .await?
                .context("active attempt missing")?,
        )?;
        attempt.validate()?;
        ensure!(
            attempt.session == task.session,
            "active attempt session mismatch"
        );
        ensure!(
            attempt.id == canonical_digest(&("attempt", &task.origin_operation))?
                && attempt.task_id == canonical_digest(&("task", &task.origin_operation))?,
            "attempt origin identity mismatch"
        );
        let mut open = BTreeSet::new();
        for id in &task.operations {
            let operation = self
                .prompt_operation(db, id)
                .await?
                .context("task operation missing")?;
            ensure!(
                operation.attempt_id == attempt.id && operation.session == task.session,
                "task operation membership mismatch"
            );
            if operation.response.is_none() {
                open.insert(operation.id);
            }
        }
        ensure!(
            open == task.open_operations,
            "task operation state mismatch"
        );
        Ok(attempt)
    }

    pub(crate) async fn active_attempt(&self, session: &AcpSessionKey) -> Result<Option<Attempt>> {
        let transaction = self.read_snapshot().await?;
        let attempt = match self.active_task(&transaction, session).await? {
            Some(task) => self.task_attempt(&transaction, &task).await.map(Some),
            None => Ok(None),
        }?;
        transaction.commit().await?;
        Ok(attempt)
    }

    pub(crate) async fn prompt_session(
        &self,
        controller: &str,
        operation: &str,
    ) -> Result<Option<AcpSessionKey>> {
        let transaction = self.read_snapshot().await?;
        let session = self
            .prompt_operation(&transaction, &PromptOperation::key(controller, operation)?)
            .await?
            .map(|operation| operation.session);
        transaction.commit().await?;
        Ok(session)
    }

    pub(crate) async fn native_checkpoint_evidence(
        &self,
        session: &AcpSessionKey,
    ) -> Result<crate::session_evidence::checkpoint::NativeCheckpointEvidence> {
        use crate::session_evidence::checkpoint::NativeCheckpointEvidence;
        let transaction = self.read_snapshot().await?;
        let mut evidence = NativeCheckpointEvidence::default();
        if let Some(task) = self.active_task(&transaction, session).await? {
            self.task_attempt(&transaction, &task).await?;
            let origin = self
                .prompt_operation(&transaction, &task.origin_operation)
                .await?
                .context("checkpoint origin missing")?;
            if let Some((id, checkpoint)) = self
                .boundary_checkpoint(&transaction, &origin, &origin.request, "request")
                .await?
            {
                evidence.baseline = Some(id);
                evidence.gaps.extend(checkpoint.gaps);
            } else {
                evidence
                    .gaps
                    .insert("native_checkpoint_baseline_unavailable".into());
            }
            if let Some(id) = &task.last_response {
                let operation = self
                    .prompt_operation(&transaction, id)
                    .await?
                    .context("checkpoint prompt result missing")?;
                let boundary = operation
                    .response
                    .as_ref()
                    .context("checkpoint response missing")?;
                if let Some((id, checkpoint)) = self
                    .boundary_checkpoint(&transaction, &operation, boundary, "response")
                    .await?
                {
                    evidence.latest_prompt_result = Some(id);
                    evidence.gaps.extend(checkpoint.gaps);
                }
            }
            if evidence.latest_prompt_result.is_none() {
                evidence
                    .gaps
                    .insert("native_checkpoint_result_unavailable".into());
            }
        }
        transaction.commit().await?;
        Ok(evidence)
    }

    async fn boundary_checkpoint(
        &self,
        db: &impl ConnectionTrait,
        operation: &PromptOperation,
        boundary: &Boundary,
        phase: &str,
    ) -> Result<
        Option<(
            String,
            crate::session_evidence::checkpoint::NativeCheckpoint,
        )>,
    > {
        let records = range_records(db, &self.owner_key, &boundary.range).await?;
        let raw = &records
            .first()
            .context("checkpoint boundary record missing")?
            .input
            .raw;
        let Some(id) = raw.get("native_checkpoint") else {
            return Ok(None);
        };
        let id = id.as_str().context("invalid checkpoint reference")?;
        let checkpoint = self.checkpoint_on(db, id).await?;
        ensure!(
            checkpoint.controller_id == operation.controller_id
                && checkpoint.operation_id == operation.operation_id
                && checkpoint.phase == phase
                && checkpoint.session == operation.session,
            "checkpoint boundary provenance mismatch"
        );
        Ok(Some((id.into(), checkpoint)))
    }

    pub(crate) async fn has_unobserved_prompts(
        &self,
        session: &AcpSessionKey,
        controller: &str,
    ) -> Result<bool> {
        let transaction = self.read_snapshot().await?;
        let mut unobserved = false;
        if let Some(task) = self.active_task(&transaction, session).await? {
            for id in &task.open_operations {
                let operation = self
                    .prompt_operation(&transaction, id)
                    .await?
                    .context("open prompt missing")?;
                unobserved |= operation.controller_id != controller;
            }
        }
        transaction.commit().await?;
        Ok(unobserved)
    }

    pub(super) async fn read_snapshot(&self) -> Result<DatabaseTransaction> {
        // Every boundary and membership row must come from the same snapshot.
        // PostgreSQL's default READ COMMITTED does not provide this across
        // multiple SELECTs. SQLite read transactions already pin a snapshot.
        // https://www.postgresql.org/docs/current/transaction-iso.html
        // https://www.sqlite.org/isolation.html
        if self.db.get_database_backend() == DbBackend::Sqlite {
            Ok(self.db.begin().await?)
        } else {
            Ok(self
                .db
                .begin_with_config(
                    Some(IsolationLevel::RepeatableRead),
                    Some(AccessMode::ReadOnly),
                )
                .await?)
        }
    }

    pub(crate) async fn workspace_evidence(
        &self,
        session: &AcpSessionKey,
    ) -> Result<crate::session_evidence::types::WorkspaceEvidence> {
        use crate::session_evidence::types::WorkspaceEvidence;

        let transaction = self.read_snapshot().await?;
        let mut evidence = WorkspaceEvidence::default();
        if let Some(task) = self.active_task(&transaction, session).await? {
            self.task_attempt(&transaction, &task).await?;
            let origin = self
                .prompt_operation(&transaction, &task.origin_operation)
                .await?
                .context("workspace task origin missing")?;
            let baseline = self
                .boundary_workspace(&transaction, &origin.request)
                .await?;
            evidence.baseline = baseline.as_ref().map(|(id, _)| id.clone());
            if let Some((_, workspace)) = &baseline {
                evidence.gaps.extend(workspace.gaps.iter().cloned());
            } else {
                evidence
                    .gaps
                    .insert("workspace_baseline_unavailable".into());
            }
            if let Some(id) = &task.last_response {
                let operation = self
                    .prompt_operation(&transaction, id)
                    .await?
                    .context("workspace prompt result missing")?;
                let boundary = operation
                    .response
                    .as_ref()
                    .context("workspace result boundary missing")?;
                if let Some((id, workspace)) =
                    self.boundary_workspace(&transaction, boundary).await?
                {
                    evidence.latest_prompt_result = Some(id);
                    evidence.gaps.extend(workspace.gaps.iter().cloned());
                    if baseline
                        .as_ref()
                        .is_some_and(|(_, before)| before.repository != workspace.repository)
                    {
                        evidence.gaps.insert("workspace_repository_changed".into());
                    }
                }
            }
            if evidence.latest_prompt_result.is_none() {
                evidence.gaps.insert("workspace_result_unavailable".into());
            }
        }
        transaction.commit().await?;
        Ok(evidence)
    }

    async fn boundary_workspace(
        &self,
        db: &impl ConnectionTrait,
        boundary: &Boundary,
    ) -> Result<Option<(String, crate::session_evidence::workspace::Workspace)>> {
        let records = range_records(db, &self.owner_key, &boundary.range).await?;
        let raw = &records
            .first()
            .context("workspace boundary record missing")?
            .input
            .raw;
        let Some(id) = raw.get("workspace_artifact") else {
            return Ok(None);
        };
        let id = id
            .as_str()
            .context("invalid workspace artifact reference")?;
        let artifact = self.workspace_on(db, id).await?;
        Ok(Some((
            id.into(),
            crate::session_evidence::workspace::Workspace::from_artifact(&artifact)?,
        )))
    }

    async fn replace_task_object<T: serde::Serialize>(
        &self,
        db: &impl ConnectionTrait,
        kind: &str,
        key: &str,
        previous: u64,
        value: &T,
    ) -> Result<()> {
        let json = serde_json::to_string(value)?;
        ensure!(
            json.len() <= MAX_OBJECT_BYTES,
            "task object exceeds size limit"
        );
        let revision = previous.checked_add(1).context("task revision overflow")?;
        let changed = object_entity::Entity::update_many()
            .col_expr(object_entity::Column::ObjectJson, Expr::value(json))
            .col_expr(
                object_entity::Column::Digest,
                Expr::value(canonical_digest(value)?),
            )
            .col_expr(
                object_entity::Column::Revision,
                Expr::value(i64::try_from(revision)?),
            )
            .filter(object_entity::Column::Id.eq(self.object_id(kind, key)?))
            .filter(object_entity::Column::Owner.eq(&self.owner_key))
            .filter(object_entity::Column::Revision.eq(i64::try_from(previous)?))
            .exec(db)
            .await?;
        ensure!(
            changed.rows_affected == 1,
            "task changed; reload before retry"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
