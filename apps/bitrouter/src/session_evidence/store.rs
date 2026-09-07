//! Durable raw records and compare-and-swap cursors using the app database.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Set, TransactionTrait,
    sea_query::{Expr, OnConflict},
};

use super::types::{
    Attempt, AttemptPhase, EdgeKind, EvidenceManifest, MAX_GRAPH_ITEMS, MAX_OBJECT_BYTES,
    RECORD_PAGE_SIZE, RecordInput, RegisteredSource, SourceCursor, SourceDescriptor, SourceRange,
    StoredRecord, digest_identifier, identifier,
};
use crate::eval::types::canonical_digest;

mod checkpoints;
pub mod execution;
pub mod forks;
pub(crate) mod lifecycle;
pub(crate) mod sdk_messages;
mod spools;
pub(crate) mod tasks;
mod workspace;

mod source_entity {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "native_evidence_sources")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub owner: String,
        pub descriptor_json: String,
        pub cursor_json: String,
        pub cursor_digest: String,
        pub revision: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod record_entity {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "native_evidence_records")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub owner: String,
        pub source_id: String,
        pub generation: String,
        pub source_sequence: i64,
        pub digest: String,
        pub record_json: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod object_entity {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "native_evidence_objects")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub owner: String,
        pub kind: String,
        pub object_key: String,
        pub revision: i64,
        pub digest: String,
        pub object_json: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// An instance is permanently scoped to one authenticated/local owner. Caller
/// JSON cannot select a different owner for any read or write operation.
#[derive(Clone)]
pub struct EvidenceStore {
    db: DatabaseConnection,
    owner: String,
    owner_key: String,
    #[cfg(test)]
    pub(crate) task_read_probe:
        std::sync::Arc<tokio::sync::Mutex<Option<std::sync::Arc<tasks::TaskReadProbe>>>>,
}

impl EvidenceStore {
    pub fn new(db: DatabaseConnection, owner: impl Into<String>) -> Result<Self> {
        let owner = owner.into();
        identifier(&owner)?;
        let owner_key = canonical_digest(&owner)?;
        Ok(Self {
            db,
            owner,
            owner_key,
            #[cfg(test)]
            task_read_probe: Default::default(),
        })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub(crate) fn source_id(&self, descriptor: &SourceDescriptor) -> Result<String> {
        descriptor.id(&self.owner_key)
    }

    pub async fn register(&self, descriptor: SourceDescriptor) -> Result<RegisteredSource> {
        let id = self.source_id(&descriptor)?;
        source_entity::Entity::insert(source_entity::ActiveModel {
            id: Set(id.clone()),
            owner: Set(self.owner_key.clone()),
            descriptor_json: Set(serde_json::to_string(&descriptor)?),
            cursor_json: Set(serde_json::to_string(&SourceCursor::default())?),
            cursor_digest: Set(canonical_digest(&SourceCursor::default())?),
            revision: Set(0),
        })
        .on_conflict(
            OnConflict::column(source_entity::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .do_nothing()
        .exec(&self.db)
        .await?;
        let source = self
            .source(&id)
            .await?
            .context("registered evidence source disappeared")?;
        ensure!(
            source.descriptor == descriptor,
            "evidence source descriptor conflict"
        );
        Ok(source)
    }

    pub async fn source(&self, id: &str) -> Result<Option<RegisteredSource>> {
        source_entity::Entity::find_by_id(id)
            .filter(source_entity::Column::Owner.eq(&self.owner_key))
            .one(&self.db)
            .await?
            .map(|row| {
                ensure!(row.owner == self.owner_key, "foreign evidence source");
                decode_source(row)
            })
            .transpose()
    }

    pub async fn sources(&self, after: Option<&str>, limit: u64) -> Result<Vec<RegisteredSource>> {
        ensure!(
            (1..=MAX_GRAPH_ITEMS as u64).contains(&limit),
            "invalid source page limit"
        );
        let mut query = source_entity::Entity::find()
            .filter(source_entity::Column::Owner.eq(&self.owner_key))
            .order_by_asc(source_entity::Column::Id)
            .limit(limit);
        if let Some(id) = after {
            digest_identifier(id)?;
            query = query.filter(source_entity::Column::Id.gt(id));
        }
        let rows = query.all(&self.db).await?;
        rows.into_iter()
            .map(|row| {
                ensure!(row.owner == self.owner_key, "foreign evidence source");
                decode_source(row)
            })
            .collect()
    }

    /// Recovery must be able to advance past one corrupt registration without
    /// treating it as valid or disabling collection for healthy live sources.
    /// Keep the opaque database cursor separate from a decoded source id.
    pub(crate) async fn source_inventory(
        &self,
        after: Option<&str>,
        limit: u64,
    ) -> Result<Vec<(String, Result<RegisteredSource>)>> {
        ensure!(
            (1..=MAX_GRAPH_ITEMS as u64).contains(&limit),
            "invalid inventory page limit"
        );
        let mut query = source_entity::Entity::find()
            .filter(source_entity::Column::Owner.eq(&self.owner_key))
            .order_by_asc(source_entity::Column::Id)
            .limit(limit);
        if let Some(id) = after {
            // This is an exact cursor returned by the database, not an input
            // evidence identifier. Even a corrupt primary key must be passed
            // back unchanged so recovery can advance beyond that row.
            query = query.filter(source_entity::Column::Id.gt(id));
        }
        Ok(query
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| {
                let id = row.id.clone();
                let source = if row.owner == self.owner_key {
                    decode_source(row)
                } else {
                    Err(anyhow::anyhow!("foreign evidence source"))
                };
                (id, source)
            })
            .collect())
    }

    /// Commit a complete, contiguous batch and its cursor as one transaction.
    /// The source revision detects competing collectors. Replaying an existing
    /// sequence is allowed only when the entire immutable record is identical.
    pub async fn append(
        &self,
        source: &RegisteredSource,
        records: &[RecordInput],
        cursor: SourceCursor,
    ) -> Result<RegisteredSource> {
        let transaction = self.db.begin().await?;
        let next = self
            .append_on(&transaction, source, records, cursor)
            .await?;
        transaction.commit().await?;
        Ok(next)
    }

    async fn append_on(
        &self,
        db: &impl ConnectionTrait,
        source: &RegisteredSource,
        records: &[RecordInput],
        cursor: SourceCursor,
    ) -> Result<RegisteredSource> {
        ensure!(
            source.id == source.descriptor.id(&self.owner_key)?,
            "evidence source owner mismatch"
        );
        validate_batch(source, records, &cursor)?;
        let next_revision = source
            .revision
            .checked_add(1)
            .context("source revision overflow")?;
        let updated = source_entity::Entity::update_many()
            .col_expr(
                source_entity::Column::CursorJson,
                Expr::value(serde_json::to_string(&cursor)?),
            )
            .col_expr(source_entity::Column::Revision, Expr::value(next_revision))
            .col_expr(
                source_entity::Column::CursorDigest,
                Expr::value(canonical_digest(&cursor)?),
            )
            .filter(source_entity::Column::Id.eq(&source.id))
            .filter(source_entity::Column::Owner.eq(&self.owner_key))
            .filter(source_entity::Column::Revision.eq(source.revision))
            .filter(source_entity::Column::CursorDigest.eq(canonical_digest(&source.cursor)?))
            .exec(db)
            .await?;
        ensure!(
            updated.rows_affected == 1,
            "evidence source cursor changed; reload before retry"
        );
        for record in records {
            let id = record.id(&source.id)?;
            let digest = canonical_digest(record)?;
            if let Some(existing) = record_entity::Entity::find_by_id(&id).one(db).await? {
                ensure!(
                    existing.owner == self.owner_key && existing.digest == digest,
                    "native record id conflict"
                );
                decode_record(existing)?;
                self.write_facts(
                    db,
                    &source.descriptor,
                    &StoredRecord {
                        id,
                        source_id: source.id.clone(),
                        digest,
                        input: record.clone(),
                    },
                )
                .await?;
                continue;
            }
            record_entity::ActiveModel {
                id: Set(id.clone()),
                owner: Set(self.owner_key.clone()),
                source_id: Set(source.id.clone()),
                generation: Set(canonical_digest(&record.generation)?),
                source_sequence: Set(i64::try_from(record.sequence)?),
                digest: Set(digest.clone()),
                record_json: Set(serde_json::to_string(record)?),
            }
            .insert(db)
            .await?;
            self.write_facts(
                db,
                &source.descriptor,
                &StoredRecord {
                    id,
                    source_id: source.id.clone(),
                    digest,
                    input: record.clone(),
                },
            )
            .await?;
        }
        Ok(RegisteredSource {
            revision: next_revision,
            cursor,
            ..source.clone()
        })
    }

    pub async fn records(&self, range: &SourceRange) -> Result<Vec<StoredRecord>> {
        range_records(&self.db, &self.owner_key, range).await
    }

    pub async fn create_attempt(&self, attempt: &Attempt) -> Result<()> {
        attempt.validate()?;
        ensure!(
            attempt.revision == 0,
            "new attempt must start at revision zero"
        );
        ensure!(
            attempt.phase == AttemptPhase::Collecting
                && attempt.latest_manifest.is_none()
                && attempt.effective_manifest.is_none(),
            "new attempt cannot reference a checkpoint"
        );
        self.insert_object(&self.db, "attempt", &attempt.id, 0, attempt)
            .await
    }

    pub async fn attempt(&self, id: &str) -> Result<Option<Attempt>> {
        self.object(&self.db, "attempt", id)
            .await?
            .map(tasks::decode_task_object)
            .transpose()
    }

    pub async fn attempts(&self, after: Option<&str>, limit: u64) -> Result<Vec<Attempt>> {
        ensure!(
            (1..=MAX_GRAPH_ITEMS as u64).contains(&limit),
            "invalid attempt page limit"
        );
        let mut query = object_entity::Entity::find()
            .filter(object_entity::Column::Owner.eq(&self.owner_key))
            .filter(object_entity::Column::Kind.eq("attempt"))
            .order_by_asc(object_entity::Column::Id)
            .limit(limit);
        if let Some(id) = after {
            query = query.filter(object_entity::Column::Id.gt(self.object_id("attempt", id)?));
        }
        let rows = query.all(&self.db).await?;
        rows.into_iter()
            .map(|row| {
                ensure!(row.owner == self.owner_key, "foreign evidence attempt");
                tasks::decode_task_object(row)
            })
            .collect()
    }

    pub async fn update_attempt(&self, expected_revision: u64, attempt: &Attempt) -> Result<()> {
        attempt.validate()?;
        ensure!(
            Some(attempt.revision) == expected_revision.checked_add(1),
            "attempt revision must advance once"
        );
        let transaction = self.db.begin().await?;
        let old: Attempt = tasks::decode_task_object(
            self.object(&transaction, "attempt", &attempt.id)
                .await?
                .context("unknown evidence attempt")?,
        )?;
        ensure!(
            old.revision == expected_revision,
            "attempt changed; reload before retry"
        );
        ensure!(
            old.session == attempt.session
                && old.task_id == attempt.task_id
                && old.started_at == attempt.started_at,
            "attempt identity is immutable"
        );
        for pointer in [&attempt.latest_manifest, &attempt.effective_manifest]
            .into_iter()
            .flatten()
        {
            let manifest: EvidenceManifest = decode_object(
                self.object(&transaction, "manifest", pointer)
                    .await?
                    .context("unknown or foreign manifest pointer")?,
            )?;
            ensure!(
                manifest.attempt_id == attempt.id && manifest.revision <= expected_revision,
                "manifest pointer belongs to a different attempt or future revision"
            );
            if attempt.effective_manifest.as_ref() == Some(pointer) {
                ensure!(
                    manifest.coverage.optimization_eligible(),
                    "effective manifest has incomplete evidence"
                );
            }
        }
        if attempt.phase == AttemptPhase::Ready {
            ensure!(
                attempt.latest_manifest.is_some()
                    && attempt.effective_manifest == attempt.latest_manifest,
                "ready attempt requires an eligible checkpoint"
            );
            let manifest: EvidenceManifest = decode_object(
                self.object(
                    &transaction,
                    "manifest",
                    attempt
                        .latest_manifest
                        .as_deref()
                        .context("missing ready manifest")?,
                )
                .await?
                .context("missing ready manifest")?,
            )?;
            ensure!(
                manifest.revision == expected_revision && manifest.members == attempt.members,
                "ready checkpoint is stale"
            );
        } else {
            ensure!(
                attempt.effective_manifest.is_none(),
                "only a ready attempt selects optimization evidence"
            );
        }
        let id = self.object_id("attempt", &attempt.id)?;
        let updated = object_entity::Entity::update_many()
            .col_expr(
                object_entity::Column::ObjectJson,
                Expr::value(serde_json::to_string(attempt)?),
            )
            .col_expr(
                object_entity::Column::Digest,
                Expr::value(canonical_digest(attempt)?),
            )
            .col_expr(
                object_entity::Column::Revision,
                Expr::value(i64::try_from(attempt.revision)?),
            )
            .filter(object_entity::Column::Id.eq(id))
            .filter(object_entity::Column::Owner.eq(&self.owner_key))
            .filter(object_entity::Column::Revision.eq(i64::try_from(expected_revision)?))
            .exec(&transaction)
            .await?;
        ensure!(
            updated.rows_affected == 1,
            "attempt changed; reload before retry"
        );
        transaction.commit().await?;
        Ok(())
    }

    /// Verify every referenced record against this owner's durable database.
    /// Content-addressed manifests cannot be changed by later source appends.
    pub async fn freeze(&self, manifest: &EvidenceManifest) -> Result<String> {
        manifest.validate()?;
        let digest = manifest.digest()?;
        if let Some(existing) = self.manifest(&digest).await? {
            ensure!(existing == *manifest, "immutable manifest conflict");
            return Ok(digest);
        }
        let transaction = self.db.begin().await?;
        let attempt: Attempt = tasks::decode_task_object(
            self.object(&transaction, "attempt", &manifest.attempt_id)
                .await?
                .context("unknown evidence attempt")?,
        )?;
        ensure!(
            attempt.members == manifest.members,
            "manifest attempt membership mismatch"
        );
        ensure!(
            attempt.revision == manifest.revision,
            "manifest attempt revision mismatch"
        );
        let mut sources = BTreeMap::new();
        for range in &manifest.ranges {
            let source = source_entity::Entity::find_by_id(&range.source_id)
                .filter(source_entity::Column::Owner.eq(&self.owner_key))
                .one(&transaction)
                .await?
                .context("unknown or foreign evidence source")?;
            let source = decode_source(source)?;
            ensure!(
                source.descriptor.namespace == attempt.session.namespace
                    && source.descriptor.harness == attempt.session.harness,
                "manifest source namespace mismatch"
            );
            ensure!(
                source.descriptor.node.is_some(),
                "manifest source has no native attribution"
            );
            sources.insert(range.source_id.clone(), source);
        }
        // Only explicit fork ancestry can bring context from a non-member.
        // A shared owner, namespace or prompt is not task membership.
        let mut related = manifest.members.clone();
        for _ in 0..manifest.edges.len() {
            let previous = related.len();
            for edge in &manifest.edges {
                if edge.kind == EdgeKind::Fork && related.contains(&edge.to) {
                    related.insert(edge.from.clone());
                }
            }
            if previous == related.len() {
                break;
            }
        }
        for edge in &manifest.edges {
            ensure!(
                related.contains(&edge.from) && related.contains(&edge.to),
                "unrelated execution edge"
            );
            if let Some(checkpoint) = &edge.checkpoint {
                ensure!(
                    sources
                        .get(&checkpoint.source_id)
                        .and_then(|s| s.descriptor.node.as_ref())
                        == Some(&edge.from),
                    "edge checkpoint is not from its source node"
                );
            }
        }
        validate_fork_cycles(manifest)?;
        let mut actual = BTreeMap::new();
        for range in &manifest.ranges {
            let node = sources
                .get(&range.source_id)
                .and_then(|s| s.descriptor.node.as_ref())
                .context("missing source node")?;
            ensure!(
                manifest.members.contains(node)
                    || manifest.edges.iter().any(|edge| edge.kind == EdgeKind::Fork
                        && &edge.from == node
                        && related.contains(&edge.to)
                        && edge
                            .checkpoint
                            .as_ref()
                            .is_some_and(|checkpoint| checkpoint.contains(range))),
                "source range is outside the attempt or inherited checkpoint"
            );
            let mut start = range.start;
            while start < range.end {
                let end = range.end.min(start + RECORD_PAGE_SIZE);
                let records = range_records(
                    &transaction,
                    &self.owner_key,
                    &SourceRange {
                        start,
                        end,
                        ..range.clone()
                    },
                )
                .await?;
                ensure!(
                    records.len() as u64 == end - start,
                    "manifest source range is incomplete"
                );
                actual.extend(records.into_iter().map(|record| (record.id, record.digest)));
                start = end;
            }
        }
        ensure!(
            actual == manifest.record_digests,
            "manifest record digest mismatch"
        );
        self.insert_object(
            &transaction,
            "manifest",
            &digest,
            i64::try_from(manifest.revision)?,
            manifest,
        )
        .await?;
        transaction.commit().await?;
        Ok(digest)
    }

    pub async fn manifest(&self, digest: &str) -> Result<Option<EvidenceManifest>> {
        self.object(&self.db, "manifest", digest)
            .await?
            .map(decode_object)
            .transpose()
    }

    fn object_id(&self, kind: &str, key: &str) -> Result<String> {
        identifier(key)?;
        canonical_digest(&(&self.owner_key, kind, key))
    }

    async fn object(
        &self,
        db: &impl ConnectionTrait,
        kind: &str,
        key: &str,
    ) -> Result<Option<object_entity::Model>> {
        let row = object_entity::Entity::find_by_id(self.object_id(kind, key)?)
            .filter(object_entity::Column::Owner.eq(&self.owner_key))
            .filter(object_entity::Column::Kind.eq(kind))
            .filter(object_entity::Column::ObjectKey.eq(key))
            .one(db)
            .await?;
        if let Some(row) = &row {
            ensure!(
                row.owner == self.owner_key && row.kind == kind && row.object_key == key,
                "foreign evidence object"
            );
        }
        Ok(row)
    }

    async fn insert_object<T: serde::Serialize>(
        &self,
        db: &impl ConnectionTrait,
        kind: &str,
        key: &str,
        revision: i64,
        value: &T,
    ) -> Result<()> {
        let digest = canonical_digest(value)?;
        let json = serde_json::to_string(value)?;
        ensure!(
            json.len() <= MAX_OBJECT_BYTES,
            "evidence object exceeds size limit"
        );
        object_entity::Entity::insert(object_entity::ActiveModel {
            id: Set(self.object_id(kind, key)?),
            owner: Set(self.owner_key.clone()),
            kind: Set(kind.to_owned()),
            object_key: Set(key.to_owned()),
            revision: Set(revision),
            digest: Set(digest.clone()),
            object_json: Set(json.clone()),
        })
        .on_conflict(
            OnConflict::column(object_entity::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .do_nothing()
        .exec(db)
        .await?;
        let row = self
            .object(db, kind, key)
            .await?
            .context("evidence object disappeared")?;
        ensure!(row.digest == digest, "immutable evidence object conflict");
        ensure!(row.object_json == json, "corrupt immutable evidence object");
        Ok(())
    }
}

fn validate_batch(
    source: &RegisteredSource,
    records: &[RecordInput],
    cursor: &SourceCursor,
) -> Result<()> {
    identifier(&cursor.generation)?;
    ensure!(
        records.len() <= MAX_GRAPH_ITEMS,
        "append batch exceeds record limit"
    );
    ensure!(
        serde_json::to_vec(records)?.len() <= MAX_OBJECT_BYTES,
        "append batch exceeds byte limit"
    );
    digest_identifier(&cursor.anchor_digest)?;
    ensure!(
        cursor.next_sequence <= i64::MAX as u64,
        "source cursor overflow"
    );
    let start = if source.cursor.generation == cursor.generation {
        ensure!(
            cursor.offset >= source.cursor.offset,
            "source cursor moved backwards"
        );
        source.cursor.next_sequence
    } else {
        0
    };
    ensure!(
        cursor.next_sequence >= start,
        "source sequence moved backwards"
    );
    if source.cursor.generation == cursor.generation && cursor.next_sequence == start {
        ensure!(
            cursor == &source.cursor,
            "cursor advanced without durable records"
        );
    }
    if source.cursor.generation != cursor.generation {
        ensure!(
            !records.is_empty(),
            "new generation requires durable records"
        );
    }
    let mut sequences = std::collections::BTreeSet::new();
    for record in records {
        record.validate()?;
        ensure!(
            record.generation == cursor.generation,
            "batch generation mismatch"
        );
        ensure!(
            record.sequence < cursor.next_sequence,
            "record exceeds committed cursor"
        );
        if record.sequence >= start {
            sequences.insert(record.sequence);
        }
    }
    ensure!(
        sequences.len() as u64 == cursor.next_sequence - start,
        "batch contains a sequence gap"
    );
    Ok(())
}

fn decode_source(row: source_entity::Model) -> Result<RegisteredSource> {
    let source = RegisteredSource {
        id: row.id,
        descriptor: serde_json::from_str(&row.descriptor_json)?,
        revision: row.revision,
        cursor: serde_json::from_str(&row.cursor_json)?,
    };
    ensure!(
        source.id == source.descriptor.id(&row.owner)?
            && source.revision >= 0
            && canonical_digest(&source.cursor)? == row.cursor_digest,
        "corrupt evidence source"
    );
    Ok(source)
}

async fn range_records(
    db: &impl ConnectionTrait,
    owner: &str,
    range: &SourceRange,
) -> Result<Vec<StoredRecord>> {
    range.validate()?;
    ensure!(
        range.end - range.start <= RECORD_PAGE_SIZE,
        "record read requires a bounded page"
    );
    let rows = record_entity::Entity::find()
        .filter(record_entity::Column::Owner.eq(owner))
        .filter(record_entity::Column::SourceId.eq(&range.source_id))
        .filter(record_entity::Column::Generation.eq(canonical_digest(&range.generation)?))
        .filter(record_entity::Column::SourceSequence.gte(i64::try_from(range.start)?))
        .filter(record_entity::Column::SourceSequence.lt(i64::try_from(range.end)?))
        .order_by_asc(record_entity::Column::SourceSequence)
        .limit(RECORD_PAGE_SIZE)
        .all(db)
        .await?;
    rows.into_iter()
        .map(|row| {
            ensure!(
                row.owner == owner && row.source_id == range.source_id,
                "foreign evidence record"
            );
            let record = decode_record(row)?;
            ensure!(
                record.input.generation == range.generation,
                "foreign evidence generation"
            );
            Ok(record)
        })
        .collect()
}

fn decode_record(row: record_entity::Model) -> Result<StoredRecord> {
    ensure!(
        row.record_json.len() <= MAX_OBJECT_BYTES,
        "stored record exceeds size limit"
    );
    let input: RecordInput = serde_json::from_str(&row.record_json)?;
    input.validate()?;
    ensure!(
        canonical_digest(&input)? == row.digest
            && input.id(&row.source_id)? == row.id
            && canonical_digest(&input.generation)? == row.generation
            && i64::try_from(input.sequence)? == row.source_sequence,
        "corrupt native evidence record"
    );
    Ok(StoredRecord {
        id: row.id,
        source_id: row.source_id,
        digest: row.digest,
        input,
    })
}

fn decode_object<T: serde::de::DeserializeOwned + serde::Serialize>(
    row: object_entity::Model,
) -> Result<T> {
    ensure!(
        row.object_json.len() <= MAX_OBJECT_BYTES,
        "stored object exceeds size limit"
    );
    let value: T = serde_json::from_str(&row.object_json)?;
    ensure!(
        canonical_digest(&value)? == row.digest,
        "corrupt evidence object digest"
    );
    ensure!(
        canonical_digest(&(&row.owner, &row.kind, &row.object_key))? == row.id,
        "corrupt evidence object identity"
    );
    let fields = serde_json::to_value(&value)?;
    ensure!(
        fields.get("revision").and_then(serde_json::Value::as_i64) == Some(row.revision),
        "corrupt evidence revision"
    );
    ensure!(
        match row.kind.as_str() {
            "attempt"
            | "fork_binding"
            | "active_task"
            | "prompt_operation"
            | "prompt_bridge"
            | "prompt_bridge_gap"
            | "workspace_artifact"
            | "lifecycle_request"
            | "lifecycle_response"
            | "spool_extent"
            | "native_checkpoint"
            | "task_selection"
            | "pending_task_selection"
            | "task_archive" =>
                fields.get("id").and_then(serde_json::Value::as_str)
                    == Some(row.object_key.as_str()),
            "manifest" => row.object_key == row.digest,
            _ => false,
        },
        "corrupt evidence object key"
    );
    Ok(value)
}

fn validate_fork_cycles(manifest: &EvidenceManifest) -> Result<()> {
    for edge in manifest
        .edges
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Fork)
    {
        let mut frontier = vec![&edge.from];
        let mut seen = BTreeSet::new();
        while let Some(node) = frontier.pop() {
            ensure!(node != &edge.to, "cyclic fork inheritance");
            if seen.insert(node) {
                frontier.extend(
                    manifest
                        .edges
                        .iter()
                        .filter(|other| other.kind == EdgeKind::Fork && &other.to == node)
                        .map(|other| &other.from),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
