//! Snapshot test setup over the same bounded primitives used by the server.

use super::*;

impl EvalStore {
    pub(crate) async fn freeze_snapshot(&self, frozen_at: &str) -> Result<EvalSnapshot> {
        let mut budget = Budget::new(self);
        let snapshot = self
            .materialize_snapshot_scoped(frozen_at, None, None, &mut budget)
            .await?;
        self.persist_snapshot_scoped(&snapshot, None, &mut budget)
            .await
    }
    pub(crate) async fn materialize_snapshot(&self, frozen_at: &str) -> Result<EvalSnapshot> {
        self.materialize_snapshot_scoped(frozen_at, None, None, &mut Budget::new(self))
            .await
    }
    pub(crate) async fn persist_snapshot(&self, snapshot: &EvalSnapshot) -> Result<EvalSnapshot> {
        self.persist_snapshot_scoped(snapshot, None, &mut Budget::new(self))
            .await
    }
    pub(crate) async fn freeze_snapshot_for_result_ids(
        &self,
        frozen_at: &str,
        owner_user_id: &str,
        result_ids: &[String],
    ) -> Result<EvalSnapshot> {
        self.legacy_only()?;
        validate_owner(owner_user_id)?;
        let selected = result_ids.iter().cloned().collect::<BTreeSet<_>>();
        ensure!(
            !selected.is_empty() && selected.len() == result_ids.len(),
            "snapshot result ids must be a non-empty unique set"
        );
        let mut budget = Budget::new(self);
        let snapshot = self
            .materialize_snapshot_scoped(
                frozen_at,
                Some(owner_user_id),
                Some(&selected),
                &mut budget,
            )
            .await?;
        self.persist_snapshot_scoped(&snapshot, Some(owner_user_id), &mut budget)
            .await
    }
    pub(crate) async fn snapshot_by_root(
        &self,
        evidence_root: &str,
    ) -> Result<Option<EvalSnapshot>> {
        self.read_snapshot(evidence_root, None).await
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SnapshotFixture {
    pub(crate) snapshot: EvalSnapshot,
    pub(crate) records: Vec<(StoredEvaluationResult, EvalSubject)>,
}

impl SnapshotFixture {
    pub(crate) async fn load(store: &EvalStore, root: &str) -> Result<Self> {
        let tx = store.transaction(false).await?;
        let mut budget = Budget::new(store);
        let (snapshot, owner) = store
            .read_snapshot_in(&tx, root, None, &mut budget)
            .await?
            .context("snapshot is missing")?;
        let records = store
            .manifest_records_in(&tx, &snapshot, &mut budget, false, Some(&owner))
            .await?;
        tx.commit().await?;
        Ok(Self { snapshot, records })
    }

    pub(crate) async fn from_manifest(store: &EvalStore, snapshot: EvalSnapshot) -> Result<Self> {
        let tx = store.transaction(false).await?;
        let records = store
            .manifest_records_in(&tx, &snapshot, &mut Budget::new(store), false, None)
            .await?;
        tx.commit().await?;
        Ok(Self { snapshot, records })
    }
}
