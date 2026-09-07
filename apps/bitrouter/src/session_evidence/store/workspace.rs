//! Content-addressed workspace checkpoints referenced by prompt boundaries.

use serde::{Deserialize, Serialize};

use super::*;
use crate::session_evidence::types::Artifact;
use crate::session_evidence::workspace::Workspace;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredWorkspace {
    id: String,
    revision: u64,
    artifact: Artifact,
}

impl EvidenceStore {
    pub(crate) async fn save_workspace(&self, artifact: Artifact) -> Result<String> {
        Workspace::from_artifact(&artifact)?;
        let stored = StoredWorkspace {
            id: artifact.digest.clone(),
            revision: 0,
            artifact,
        };
        self.insert_object(&self.db, "workspace_artifact", &stored.id, 0, &stored)
            .await?;
        Ok(stored.id)
    }

    pub(super) async fn workspace_on(
        &self,
        db: &impl ConnectionTrait,
        id: &str,
    ) -> Result<Artifact> {
        digest_identifier(id)?;
        let stored: StoredWorkspace = decode_object(
            self.object(db, "workspace_artifact", id)
                .await?
                .context("workspace artifact missing")?,
        )?;
        ensure!(
            stored.revision == 0 && stored.id == stored.artifact.digest,
            "workspace artifact identity mismatch"
        );
        Workspace::from_artifact(&stored.artifact)?;
        Ok(stored.artifact)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_evidence::journal::Journal;
    use crate::session_evidence::types::{Harness, SourceDescriptor, SourceFormat};
    use serde_json::json;

    async fn fixture() -> Result<(EvidenceStore, Artifact)> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let artifact =
            crate::session_evidence::workspace::capture(None, BTreeSet::new(), BTreeSet::new())
                .await?;
        Ok((EvidenceStore::new(db, "alice")?, artifact))
    }

    #[tokio::test]
    async fn workspace_bodies_are_immutable_owned_and_verified_when_loaded() -> Result<()> {
        let (store, artifact) = fixture().await?;
        let id = store.save_workspace(artifact.clone()).await?;
        assert_eq!(store.save_workspace(artifact.clone()).await?, id);
        assert_eq!(store.workspace_on(&store.db, &id).await?, artifact);
        let foreign = EvidenceStore::new(store.db.clone(), "bob")?;
        assert!(foreign.workspace_on(&foreign.db, &id).await.is_err());
        let mut changed = artifact;
        changed.content.push(' ');
        assert!(store.save_workspace(changed).await.is_err());
        let row = store
            .object(&store.db, "workspace_artifact", &id)
            .await?
            .context("workspace row")?;
        object_entity::Entity::update_many()
            .col_expr(object_entity::Column::ObjectJson, Expr::value("{}"))
            .filter(object_entity::Column::Id.eq(row.id))
            .exec(&store.db)
            .await?;
        assert!(store.workspace_on(&store.db, &id).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn prompt_artifact_references_require_owned_content_before_commit() -> Result<()> {
        let (store, artifact) = fixture().await?;
        let foreign = EvidenceStore::new(store.db.clone(), "bob")?;
        let id = foreign.save_workspace(artifact.clone()).await?;
        let journal = Journal::new(
            store.clone(),
            SourceDescriptor {
                namespace: "profile".into(),
                harness: Harness::Codex,
                format: SourceFormat::Acp,
                locator: "controller:fixture".into(),
                node: None,
            },
            "fixture/1".into(),
        )
        .await?;
        let event = json!({"method":"session/prompt","phase":"request","operation_id":"one",
            "observed_at":"2026-09-07T00:00:00Z","native_scope":"session","workspace_artifact":id,
            "payload":{"sessionId":"root","prompt":[]}});
        assert!(journal.append(event.clone()).await.is_err());
        assert_eq!(store.sources(None, 16).await?[0].cursor.next_sequence, 0);
        assert!(store.attempts(None, 16).await?.is_empty());
        store.save_workspace(artifact).await?;
        journal.append(event).await?;
        let attempt = store.attempts(None, 16).await?.remove(0);
        assert_eq!(
            store
                .workspace_evidence(&attempt.root)
                .await?
                .baseline
                .as_deref(),
            Some(id.as_str())
        );
        let row = store
            .object(&store.db, "workspace_artifact", &id)
            .await?
            .context("artifact")?;
        object_entity::Entity::delete_by_id(row.id)
            .exec(&store.db)
            .await?;
        // Operation membership is independently readable; an unavailable chosen
        // checkpoint cannot be presented or consumed as complete evidence.
        assert!(store.active_attempt(&attempt.root).await?.is_some());
        assert!(store.workspace_evidence(&attempt.root).await.is_err());
        Ok(())
    }
}
