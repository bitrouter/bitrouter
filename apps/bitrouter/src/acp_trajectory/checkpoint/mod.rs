//! Immutable native-session prefixes, assessment revisions and family views.
//!
//! Capture remains the only content source. These operations do not contact a
//! harness, inspect a workspace, invoke a judge, or publish routing changes.

mod assessment;
pub mod entities;
mod resource;
#[cfg(test)]
mod tests;
pub mod types;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use bitrouter_sdk::acp::capture::{CaptureEvent, CaptureKind};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseTransaction, EntityTrait, QueryFilter,
    QueryOrder, Set, TransactionTrait,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::entities::{connections, events, sessions};
use super::{CANONICAL_VERSION, CanonicalEvent, CanonicalStore, SessionIdentity};
use entities::{checkpoints, heads, members, resources, revisions};
use types::*;

fn digest(value: &impl Serialize) -> Result<String> {
    Ok(Sha256::digest(serde_json::to_vec(value)?)
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect())
}

fn reference(row: &events::Model) -> Result<EventReference> {
    Ok(EventReference {
        connection_id: row.connection_id.clone(),
        sequence: row.sequence,
        session_sequence: row.session_sequence,
        digest: digest(&(
            &row.event_json,
            &row.captured_at,
            &row.session_key,
            row.session_sequence,
        ))?,
    })
}

fn content_digest(segments: &[PrefixSegment]) -> Result<String> {
    let content: Vec<_> = segments
        .iter()
        .map(|s| {
            (
                &s.identity,
                s.watermark,
                &s.parent_key,
                s.parent_watermark,
                &s.events,
                &s.setup,
            )
        })
        .collect();
    digest(&(CANONICAL_VERSION, content))
}

fn native(row: &sessions::Model) -> SessionIdentity {
    SessionIdentity {
        owner: row.owner.clone(),
        source: row.source.clone(),
        native_session_id: row.native_session_id.clone(),
    }
}

async fn session(db: &impl ConnectionTrait, key: &str) -> Result<sessions::Model> {
    let row = sessions::Entity::find_by_id(key)
        .one(db)
        .await?
        .context("recorded native session not found")?;
    ensure!(!row.deleted, "recorded ACP content was deleted");
    Ok(row)
}

async fn lock_session(tx: &DatabaseTransaction, key: &str) -> Result<sessions::Model> {
    // A write locks the row on every supported database, including SQLite.
    // Do not inspect rows_affected: MySQL may report zero for an unchanged value.
    sessions::Entity::update_many()
        .col_expr(
            sessions::Column::Head,
            Expr::col(sessions::Column::Head).into(),
        )
        .filter(sessions::Column::SessionKey.eq(key))
        .exec(tx)
        .await?;
    session(tx, key).await
}

async fn manifest(
    db: &impl ConnectionTrait,
    identity: &SessionIdentity,
    id: &str,
) -> Result<Checkpoint> {
    let row = checkpoints::Entity::find_by_id(id)
        .one(db)
        .await?
        .context("checkpoint not found")?;
    ensure!(
        row.session_key == identity.key()?,
        "checkpoint belongs to a different native session"
    );
    ensure!(!row.deleted, "checkpoint content was deleted");
    let cp: Checkpoint = serde_json::from_str(&row.manifest_json)?;
    ensure!(
        cp.schema_version == 1 && cp.canonical_version == CANONICAL_VERSION,
        "unsupported checkpoint version"
    );
    ensure!(
        cp.identity == *identity && cp.watermark == row.watermark,
        "checkpoint identity or watermark mismatch"
    );
    ensure!(
        content_digest(&cp.segments)? == cp.prefix_digest,
        "checkpoint prefix digest mismatch"
    );
    ensure!(
        digest(&(identity, cp.watermark, &cp.prefix_digest))? == id,
        "checkpoint ID mismatch"
    );
    for segment in &cp.segments {
        session(db, &segment.identity.key()?).await?;
    }
    Ok(cp)
}

impl CanonicalStore {
    /// Freeze the observed current prefix. A stale expected watermark is an
    /// error, not permission to silently evaluate a different input.
    pub async fn freeze_checkpoint(
        &self,
        identity: &SessionIdentity,
        expected_watermark: i64,
    ) -> Result<Checkpoint> {
        self.freeze_checkpoint_checked(identity, expected_watermark, |_| Box::pin(async { Ok(()) }))
            .await
    }

    /// Automated discovery fences its feedback epoch in the same transaction
    /// that freezes source references. No external work is allowed in the check.
    pub(crate) async fn freeze_checkpoint_checked<F>(
        &self,
        identity: &SessionIdentity,
        expected_watermark: i64,
        precondition: F,
    ) -> Result<Checkpoint>
    where
        F: for<'a> FnOnce(&'a DatabaseTransaction) -> futures::future::BoxFuture<'a, Result<()>>
            + Send,
    {
        ensure!(expected_watermark >= 0, "watermark must be nonnegative");
        let first = session(&self.db, &identity.key()?).await?;
        ensure!(
            first.head == expected_watermark,
            "session changed; refresh the expected watermark"
        );
        let mut current = first.clone();
        let mut watermark = expected_watermark;
        let mut dependencies = BTreeMap::new();
        let mut segments = Vec::new();
        let mut gaps = BTreeSet::new();
        loop {
            ensure!(
                !dependencies.contains_key(&current.session_key),
                "cyclic native fork lineage"
            );
            dependencies.insert(current.session_key.clone(), current.clone());
            ensure!(
                watermark <= current.head,
                "inherited watermark exceeds recorded parent history"
            );
            let (segment, segment_gaps) = self.prefix_segment(&current, watermark).await?;
            gaps.extend(segment_gaps);
            let parent = segment.parent_key.clone();
            let parent_watermark = segment.parent_watermark;
            segments.push(segment);
            let Some(parent_key) = parent else {
                break;
            };
            let Some(parent_watermark) = parent_watermark else {
                gaps.insert(format!("parent_watermark_unknown:{parent_key}"));
                break;
            };
            let Some(parent) = sessions::Entity::find_by_id(&parent_key)
                .one(&self.db)
                .await?
            else {
                gaps.insert(format!("parent_history_missing:{parent_key}"));
                break;
            };
            ensure!(
                parent.owner == identity.owner && parent.source == identity.source,
                "fork lineage crosses native identity scope"
            );
            if parent.deleted {
                gaps.insert(format!("parent_history_deleted:{parent_key}"));
                break;
            }
            current = parent;
            watermark = parent_watermark;
        }
        segments.reverse();
        // A successful fork responds in the new child's native session. When
        // that response closes a parent's recorded request, retain its source
        // as a deletion/locking dependency without inheriting the child's work.
        for expected in segments.iter().flat_map(|segment| &segment.setup) {
            let raw =
                events::Entity::find_by_id((expected.connection_id.clone(), expected.sequence))
                    .one(&self.db)
                    .await?
                    .context("supplementary checkpoint evidence disappeared")?;
            ensure!(
                reference(&raw)? == *expected,
                "supplementary checkpoint evidence changed"
            );
            if let Some(key) = raw.session_key
                && !dependencies.contains_key(&key)
            {
                let dependency = session(&self.db, &key).await?;
                ensure!(
                    dependency.owner == identity.owner && dependency.source == identity.source,
                    "supplementary checkpoint evidence crosses native scope"
                );
                dependencies.insert(key, dependency);
            }
        }
        let prefix_digest = content_digest(&segments)?;
        let checkpoint_id = digest(&(identity, expected_watermark, &prefix_digest))?;
        let family_id = self.family_id(identity).await?;
        let tx = self.db.begin().await?;
        precondition(&tx).await?;
        // Stable lock ordering prevents two overlapping fork freezes from
        // acquiring parent/child locks in opposite orders.
        for (key, expected) in &dependencies {
            ensure!(
                lock_session(&tx, key).await? == *expected,
                "session changed while freezing; retry with a fresh prefix"
            );
        }
        if checkpoints::Entity::find_by_id(&checkpoint_id)
            .one(&tx)
            .await?
            .is_some()
        {
            let existing = manifest(&tx, identity, &checkpoint_id).await?;
            tx.commit().await?;
            self.observe_checkpoint_resources(identity, &checkpoint_id)
                .await?;
            return Ok(existing);
        }
        let previous = checkpoints::Entity::find()
            .filter(checkpoints::Column::SessionKey.eq(identity.key()?))
            .filter(checkpoints::Column::Deleted.eq(false))
            .filter(checkpoints::Column::Watermark.lt(expected_watermark))
            .order_by_desc(checkpoints::Column::Watermark)
            .one(&tx)
            .await?;
        let cp = Checkpoint {
            schema_version: 1,
            canonical_version: CANONICAL_VERSION,
            checkpoint_id: checkpoint_id.clone(),
            identity: identity.clone(),
            watermark: expected_watermark,
            prefix_digest,
            previous_checkpoint_id: previous.map(|row| row.checkpoint_id),
            family_id,
            segments,
            gaps: gaps.into_iter().collect(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        checkpoints::ActiveModel {
            checkpoint_id: Set(checkpoint_id.clone()),
            session_key: Set(identity.key()?),
            watermark: Set(expected_watermark),
            manifest_json: Set(serde_json::to_string(&cp)?),
            deleted: Set(false),
        }
        .insert(&tx)
        .await?;
        for key in dependencies.keys() {
            members::ActiveModel {
                checkpoint_id: Set(checkpoint_id.clone()),
                session_key: Set(key.clone()),
            }
            .insert(&tx)
            .await?;
        }
        tx.commit().await?;
        self.observe_checkpoint_resources(identity, &checkpoint_id)
            .await?;
        Ok(cp)
    }

    async fn prefix_segment(
        &self,
        row: &sessions::Model,
        watermark: i64,
    ) -> Result<(PrefixSegment, Vec<String>)> {
        let rows = events::Entity::find()
            .filter(events::Column::SessionKey.eq(&row.session_key))
            .filter(events::Column::SessionSequence.lte(watermark))
            .filter(events::Column::Replay.eq(false))
            .order_by_asc(events::Column::SessionSequence)
            .all(&self.db)
            .await?;
        let mut gaps = Vec::new();
        if rows.len() as u128 != watermark as u128
            || rows
                .iter()
                .enumerate()
                .any(|(i, r)| r.session_sequence != Some(i as i64 + 1))
        {
            gaps.push(format!("canonical_sequence_gap:{}", row.session_key));
        }
        let mut events = Vec::new();
        let mut setup = Vec::new();
        let mut connection_ids = BTreeSet::new();
        let mut setup_calls = Vec::new();
        let mut pending = BTreeMap::new();
        let mut observed_new = false;
        let mut observed_fork = false;
        for raw in &rows {
            connection_ids.insert(raw.connection_id.clone());
            let event: CaptureEvent = serde_json::from_str(&raw.event_json)?;
            if let Some(call) = event.call_id {
                let key = (raw.connection_id.clone(), call);
                match event.kind {
                    CaptureKind::Request => {
                        pending.insert(key, (event.method.clone(), raw.sequence));
                    }
                    CaptureKind::Response => {
                        pending.remove(&key);
                    }
                    _ => {}
                }
                if event.kind == CaptureKind::Response
                    && event
                        .payload
                        .get("result")
                        .and_then(|r| r.get("sessionId"))
                        .is_some()
                {
                    observed_new |= event.method == "session/new";
                    observed_fork |= event.method == "session/fork";
                    if matches!(event.method.as_str(), "session/new" | "session/fork") {
                        setup_calls.push((raw.connection_id.clone(), call, event.method.clone()));
                    }
                }
            }
            if matches!(event.method.as_str(), "session/load" | "session/resume") {
                gaps.push(format!(
                    "history_across_load_or_resume_unknown:{}",
                    raw.connection_id
                ));
            }
            events.push(reference(raw)?);
        }
        let mut cross_session_completions = Vec::new();
        // ACP's fork response identifies the newly created session:
        // https://docs.rs/agent-client-protocol/latest/agent_client_protocol/schema/v1/struct.ForkSessionResponse.html
        for ((connection, call), (method, start)) in &pending {
            if method != "session/fork" {
                continue;
            }
            let Some(boundary) = rows
                .iter()
                .filter(|raw| &raw.connection_id == connection)
                .map(|raw| raw.sequence)
                .max()
            else {
                continue;
            };
            let candidates = super::events::Entity::find()
                .filter(super::events::Column::ConnectionId.eq(connection))
                .filter(super::events::Column::Sequence.gt(*start))
                .filter(super::events::Column::Sequence.lte(boundary))
                .all(&self.db)
                .await?;
            for raw in candidates {
                let event: CaptureEvent = serde_json::from_str(&raw.event_json)?;
                if event.kind != CaptureKind::Response
                    || event.method != *method
                    || event.call_id != Some(*call)
                {
                    continue;
                }
                let Some(child_key) = &raw.session_key else {
                    continue;
                };
                let Some(child) = sessions::Entity::find_by_id(child_key)
                    .one(&self.db)
                    .await?
                else {
                    continue;
                };
                if !child.deleted
                    && child.parent_key.as_ref() == Some(&row.session_key)
                    && child.owner == row.owner
                    && child.source == row.source
                {
                    setup.push(reference(&raw)?);
                    cross_session_completions.push((connection.clone(), *call));
                    break;
                }
            }
        }
        for key in cross_session_completions {
            pending.remove(&key);
        }
        if !pending.is_empty() {
            gaps.push(format!("requests_pending_at_watermark:{}", row.session_key));
        }
        for (connection, call, method) in setup_calls {
            let candidates = super::events::Entity::find()
                .filter(super::events::Column::ConnectionId.eq(&connection))
                .all(&self.db)
                .await?;
            let mut found = false;
            for raw in candidates {
                let event: CaptureEvent = serde_json::from_str(&raw.event_json)?;
                if event.kind == CaptureKind::Request
                    && event.call_id == Some(call)
                    && event.method == method
                {
                    setup.push(reference(&raw)?);
                    found = true;
                    break;
                }
            }
            if !found {
                gaps.push(format!("setup_evidence_missing:{connection}:{call}"));
            }
        }
        if row.history_origin == "native_id_reused" {
            gaps.push(format!("native_identity_reused:{}", row.session_key));
        } else if !observed_new && !observed_fork {
            gaps.push(format!(
                "history_before_recording_unknown:{}",
                row.session_key
            ));
        }
        let mut observed_connections = Vec::new();
        for id in connection_ids {
            let connection = connections::Entity::find_by_id(&id)
                .one(&self.db)
                .await?
                .context("capture connection missing")?;
            if connection.state == "interrupted" {
                gaps.push(format!("capture_interrupted:{id}"));
            }
            observed_connections.push(ConnectionObservation {
                connection_id: id,
                state: connection.state,
                controller_instance_id: connection.controller_instance_id,
                route_scope_id: connection.route_scope_id,
                metadata: serde_json::from_str(&connection.metadata_json)?,
            });
        }
        Ok((
            PrefixSegment {
                identity: native(row),
                watermark,
                parent_key: if observed_fork {
                    row.parent_key.clone()
                } else {
                    None
                },
                parent_watermark: if observed_fork {
                    row.parent_watermark
                } else {
                    None
                },
                events,
                setup,
                connections: observed_connections,
                boundary_at: rows
                    .last()
                    .map(|r| r.captured_at.clone())
                    .unwrap_or_default(),
            },
            gaps,
        ))
    }

    pub async fn checkpoints(&self, identity: &SessionIdentity) -> Result<Vec<Checkpoint>> {
        session(&self.db, &identity.key()?).await?;
        let rows = checkpoints::Entity::find()
            .filter(checkpoints::Column::SessionKey.eq(identity.key()?))
            .filter(checkpoints::Column::Deleted.eq(false))
            .order_by_asc(checkpoints::Column::Watermark)
            .all(&self.db)
            .await?;
        let mut result = Vec::new();
        for row in rows {
            result.push(manifest(&self.db, identity, &row.checkpoint_id).await?);
        }
        Ok(result)
    }

    pub async fn checkpoint_content(
        &self,
        identity: &SessionIdentity,
        id: &str,
    ) -> Result<CheckpointContent> {
        let cp = manifest(&self.db, identity, id).await?;
        let mut canonical = Vec::new();
        let mut setup = Vec::new();
        for segment in &cp.segments {
            for (refs, target) in [
                (&segment.events, &mut canonical),
                (&segment.setup, &mut setup),
            ] {
                for expected in refs {
                    let row = events::Entity::find_by_id((
                        expected.connection_id.clone(),
                        expected.sequence,
                    ))
                    .one(&self.db)
                    .await?
                    .context("checkpoint source event missing")?;
                    ensure!(
                        reference(&row)? == *expected,
                        "checkpoint source content changed"
                    );
                    target.push(CanonicalEvent {
                        node_id: expected.node_id(),
                        sequence: expected.session_sequence.unwrap_or_default(),
                        captured_at: row.captured_at,
                        event: serde_json::from_str(&row.event_json)?,
                    });
                }
            }
        }
        // A deletion during the reads must invalidate the whole export.
        manifest(&self.db, identity, id).await?;
        Ok(CheckpointContent {
            checkpoint: cp,
            events: canonical,
            setup,
        })
    }

    pub(crate) async fn family_id(&self, identity: &SessionIdentity) -> Result<String> {
        let mut key = identity.key()?;
        let mut visited = BTreeSet::new();
        loop {
            ensure!(visited.insert(key.clone()), "cyclic native fork lineage");
            let Some(row) = sessions::Entity::find_by_id(&key).one(&self.db).await? else {
                return Ok(key);
            };
            ensure!(
                row.owner == identity.owner && row.source == identity.source,
                "fork lineage crosses native identity scope"
            );
            match row.parent_key {
                Some(parent) => key = parent,
                None => return Ok(key),
            }
        }
    }
}

/// Content deletion invalidates every checkpoint which references the source,
/// including child checkpoints. Assessment explanations can quote that content.
pub(super) async fn delete_dependents(tx: &DatabaseTransaction, key: &str) -> Result<()> {
    let affected = members::Entity::find()
        .filter(members::Column::SessionKey.eq(key))
        .all(tx)
        .await?;
    for member in affected {
        // A stopped judge may have cached a response quoting the deleted
        // source, including through an inherited prefix. Remove those jobs
        // in the same transaction that invalidates their checkpoint.
        if let Some(cp) = checkpoints::Entity::find_by_id(&member.checkpoint_id)
            .one(tx)
            .await?
        {
            crate::evolution::store::records::Entity::delete_many()
                .filter(crate::evolution::store::records::Column::SessionKey.eq(cp.session_key))
                .filter(crate::evolution::store::records::Column::Kind.eq("judge_job"))
                .exec(tx)
                .await?;
        }
        let revisions = revisions::Entity::find()
            .filter(revisions::Column::CheckpointId.eq(&member.checkpoint_id))
            .all(tx)
            .await?;
        for revision in revisions {
            heads::Entity::update_many()
                .col_expr(
                    heads::Column::RevisionId,
                    Expr::value(Option::<String>::None),
                )
                .filter(heads::Column::RevisionId.eq(revision.revision_id))
                .exec(tx)
                .await?;
        }
        revisions::Entity::delete_many()
            .filter(revisions::Column::CheckpointId.eq(&member.checkpoint_id))
            .exec(tx)
            .await?;
        resources::Entity::delete_many()
            .filter(resources::Column::CheckpointId.eq(&member.checkpoint_id))
            .exec(tx)
            .await?;
        checkpoints::Entity::update_many()
            .col_expr(checkpoints::Column::Deleted, Expr::value(true))
            .col_expr(checkpoints::Column::ManifestJson, Expr::value("{}"))
            .filter(checkpoints::Column::CheckpointId.eq(&member.checkpoint_id))
            .exec(tx)
            .await?;
    }
    Ok(())
}
