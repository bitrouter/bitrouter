//! Persist the observed byte extent before importing a bounded spool prefix.
//! Losing the rest of a file must not turn that prefix into complete evidence.

use super::*;
use crate::session_evidence::types::SourceFormat;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpoolExtent {
    id: String,
    /// Monotonic observed byte length, also used as the database CAS revision.
    revision: u64,
}

impl EvidenceStore {
    fn validate_spool(&self, source: &RegisteredSource) -> Result<()> {
        ensure!(
            self.source_id(&source.descriptor)? == source.id
                && source.descriptor.node.is_none()
                && matches!(
                    source.descriptor.format,
                    SourceFormat::CodexAppServer
                        | SourceFormat::ClaudeCli
                        | SourceFormat::ClaudeHook
                ),
            "invalid owned spool source"
        );
        Ok(())
    }

    pub(crate) async fn observe_spool_extent(
        &self,
        source: &RegisteredSource,
        end: u64,
    ) -> Result<()> {
        self.validate_spool(source)?;
        let revision = i64::try_from(end)?;
        let next = SpoolExtent {
            id: source.id.clone(),
            revision: end,
        };
        let Some(row) = self.object(&self.db, "spool_extent", &source.id).await? else {
            return self
                .insert_object(&self.db, "spool_extent", &source.id, revision, &next)
                .await;
        };
        let existing: SpoolExtent = decode_object(row)?;
        if existing.revision >= end {
            return Ok(());
        }
        // A concurrent larger observation may win. Keep that watermark and
        // let the collector retry; never replace it with a smaller extent.
        let updated = object_entity::Entity::update_many()
            .col_expr(
                object_entity::Column::ObjectJson,
                Expr::value(serde_json::to_string(&next)?),
            )
            .col_expr(
                object_entity::Column::Digest,
                Expr::value(canonical_digest(&next)?),
            )
            .col_expr(object_entity::Column::Revision, Expr::value(revision))
            .filter(object_entity::Column::Id.eq(self.object_id("spool_extent", &source.id)?))
            .filter(object_entity::Column::Owner.eq(&self.owner_key))
            .filter(object_entity::Column::Revision.eq(i64::try_from(existing.revision)?))
            .exec(&self.db)
            .await?;
        ensure!(
            updated.rows_affected == 1,
            "spool extent changed; retry collection"
        );
        Ok(())
    }

    pub(crate) async fn spool_extent(&self, source: &RegisteredSource) -> Result<Option<u64>> {
        self.validate_spool(source)?;
        self.object(&self.db, "spool_extent", &source.id)
            .await?
            .map(|row| decode_object::<SpoolExtent>(row).map(|extent| extent.revision))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_evidence::types::Harness;

    #[tokio::test]
    async fn spool_extents_are_monotonic_owned_and_cannot_be_repaired_over_corrupt_evidence()
    -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvidenceStore::new(db.clone(), "alice")?;
        let source = store
            .register(SourceDescriptor {
                namespace: "profile".into(),
                harness: Harness::ClaudeCode,
                format: SourceFormat::ClaudeCli,
                locator: "spool:/fixture/cli.jsonl".into(),
                node: None,
            })
            .await?;
        assert_eq!(store.spool_extent(&source).await?, None);
        for end in [128, 256, 64] {
            store.observe_spool_extent(&source, end).await?;
        }
        assert_eq!(store.spool_extent(&source).await?, Some(256));
        let foreign = EvidenceStore::new(db, "bob")?;
        assert!(foreign.spool_extent(&source).await.is_err());
        assert!(foreign.observe_spool_extent(&source, 1024).await.is_err());
        object_entity::Entity::update_many()
            .col_expr(object_entity::Column::ObjectJson, Expr::value("{}"))
            .filter(object_entity::Column::Id.eq(store.object_id("spool_extent", &source.id)?))
            .exec(&store.db)
            .await?;
        assert!(store.spool_extent(&source).await.is_err());
        assert!(store.observe_spool_extent(&source, 1024).await.is_err());
        Ok(())
    }
}
