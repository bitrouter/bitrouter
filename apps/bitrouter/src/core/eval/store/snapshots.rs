use super::*;

pub(super) struct PreparedManifest<'a> {
    snapshot: &'a EvalSnapshot,
    json: String,
}

impl EvalStore {
    pub(super) fn snapshot_root(
        &self,
        owner: Option<&str>,
        frozen_at: &str,
        entries: &[EvalSnapshotEntry],
    ) -> Result<String> {
        match self.scope.snapshot_namespace() {
            Some((installation, tenant)) => canonical_digest(&(
                "bitrouter.eval.snapshot.tenant.v1",
                installation,
                tenant,
                frozen_at,
                entries,
            )),
            None => canonical_digest(&(owner.unwrap_or("*"), frozen_at, entries)),
        }
    }

    fn manifest_shape(&self, snapshot: &EvalSnapshot) -> Result<()> {
        chrono::DateTime::parse_from_rfc3339(&snapshot.frozen_at)
            .context("snapshot frozen_at must be RFC3339")?;
        if !self.scope.is_legacy() {
            ensure!(
                snapshot.entries.len() <= io::MAX_ROWS,
                "tenant eval snapshot exceeds row bound"
            );
            ensure!(
                snapshot
                    .entries
                    .windows(2)
                    .all(|pair| pair[0].result_id < pair[1].result_id),
                "tenant eval snapshot entries must be sorted and unique"
            );
            io::json_size(snapshot, io::MAX_BYTES)?;
        }
        Ok(())
    }

    pub(super) fn verify_manifest(
        &self,
        snapshot: &EvalSnapshot,
        owner: Option<&str>,
    ) -> Result<()> {
        self.manifest_shape(snapshot)?;
        ensure!(
            self.snapshot_root(owner, &snapshot.frozen_at, &snapshot.entries)?
                == snapshot.evidence_root,
            "eval snapshot manifest does not match its namespace and evidence root"
        );
        Ok(())
    }

    pub async fn freeze_snapshot(&self, frozen_at: &str) -> Result<EvalSnapshot> {
        let mut budget = Budget::new(self);
        let snapshot = self
            .materialize_snapshot_scoped(frozen_at, None, None, &mut budget)
            .await?;
        self.persist_snapshot_scoped(&snapshot, None, &mut budget)
            .await
    }

    /// Materialize the admitted result set in one stable database snapshot.
    pub async fn materialize_snapshot(&self, frozen_at: &str) -> Result<EvalSnapshot> {
        self.materialize_snapshot_scoped(frozen_at, None, None, &mut Budget::new(self))
            .await
    }

    /// Persist immutable evidence without reevaluating later admission changes.
    pub async fn persist_snapshot(&self, snapshot: &EvalSnapshot) -> Result<EvalSnapshot> {
        self.persist_snapshot_scoped(snapshot, None, &mut Budget::new(self))
            .await
    }

    pub async fn freeze_snapshot_for_owner(
        &self,
        frozen_at: &str,
        owner_user_id: &str,
    ) -> Result<EvalSnapshot> {
        self.legacy_only()?;
        validate_owner(owner_user_id)?;
        let mut budget = Budget::new(self);
        let snapshot = self
            .materialize_snapshot_scoped(frozen_at, Some(owner_user_id), None, &mut budget)
            .await?;
        self.persist_snapshot_scoped(&snapshot, Some(owner_user_id), &mut budget)
            .await
    }

    /// Freeze exactly the named admitted results for one legacy owner.
    pub async fn freeze_snapshot_for_result_ids(
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

    async fn materialize_snapshot_scoped(
        &self,
        frozen_at: &str,
        owner: Option<&str>,
        selected: Option<&BTreeSet<String>>,
        budget: &mut Budget<'_>,
    ) -> Result<EvalSnapshot> {
        chrono::DateTime::parse_from_rfc3339(frozen_at)
            .context("snapshot frozen_at must be RFC3339")?;
        if !self.scope.is_legacy()
            && let Some(ids) = selected
        {
            ensure!(
                ids.len() <= io::MAX_ROWS,
                "tenant eval selection exceeds row bound"
            );
        }
        let tx = self.transaction(false).await?;
        let latest = self.latest_in(&tx, owner, budget).await?;
        let mut query = self.results().order_by_asc(result_entity::Column::ResultId);
        if let Some(owner) = owner {
            query = query.filter(result_entity::Column::OwnerUserId.eq(owner));
        }
        if let Some(ids) = selected {
            let keys = ids
                .iter()
                .map(|id| self.key(KeyDomain::EvalResult, id))
                .collect::<Result<Vec<_>>>()?;
            query = query.filter(result_entity::Column::ResultId.is_in(keys));
        }
        let mut entries = Vec::new();
        for row in io::load(&tx, query, budget).await? {
            let value = self.decode_result(&row)?;
            let admitted = latest
                .get(&value.result_id)
                .is_some_and(|event| event.status == AdmissionStatus::Admitted);
            ensure!(
                admitted || selected.is_none(),
                "selected eval result is not admitted"
            );
            // Validate even excluded rows; corrupt indexed projections are not
            // interpreted as a legitimate absent admission.
            let subject = io::one(
                &tx,
                self.subjects()
                    .filter(subject_entity::Column::EvalId.eq(&row.eval_id)),
                budget,
            )
            .await?
            .context("snapshot result subject is unavailable")?;
            let decoded_subject = self.decode_subject(&subject)?;
            ensure!(
                subject.owner_user_id == row.owner_user_id
                    && decoded_subject.eval_id == value.result.eval_id,
                "snapshot result parent mismatch"
            );
            if admitted {
                entries.push(EvalSnapshotEntry {
                    result_id: value.result_id,
                    content_digest: value.content_digest,
                    subject_content_digest: subject.content_digest,
                    eval_id: decoded_subject.eval_id,
                });
            }
        }
        if let Some(ids) = selected {
            ensure!(
                entries.len() == ids.len(),
                "selected eval results are unavailable"
            );
        }
        if !self.scope.is_legacy() {
            entries.sort_by(|a, b| a.result_id.cmp(&b.result_id));
        }
        let snapshot = EvalSnapshot {
            evidence_root: self.snapshot_root(owner, frozen_at, &entries)?,
            frozen_at: frozen_at.to_owned(),
            entries,
        };
        self.manifest_shape(&snapshot)?;
        if !self.scope.is_legacy() {
            budget.charge(
                snapshot.entries.len(),
                io::json_size(&snapshot, io::MAX_BYTES)?,
            )?;
        }
        tx.commit().await?;
        Ok(snapshot)
    }

    async fn persist_snapshot_scoped(
        &self,
        snapshot: &EvalSnapshot,
        owner: Option<&str>,
        budget: &mut Budget<'_>,
    ) -> Result<EvalSnapshot> {
        self.verify_manifest(snapshot, owner)?;
        let tx = self.transaction(true).await?;
        // Validate the immutable closure in the stable transaction. Never compare
        // against today's admissions: frozen evidence survives later disputes.
        if !self.scope.is_legacy() {
            self.manifest_records_in(&tx, snapshot, budget, true, None)
                .await?;
        }
        let prepared = self.prepare_manifest(snapshot, budget)?;
        let existing = self
            .persist_prepared_in(&tx, prepared, owner, budget)
            .await?;
        tx.commit().await?;
        Ok(existing)
    }

    pub(super) fn prepare_manifest<'a>(
        &self,
        snapshot: &'a EvalSnapshot,
        budget: &mut Budget<'_>,
    ) -> Result<PreparedManifest<'a>> {
        if !self.scope.is_legacy() {
            budget.charge(
                snapshot.entries.len(),
                io::json_size(snapshot, io::MAX_BYTES)?,
            )?;
        }
        budget.check()?;
        Ok(PreparedManifest {
            snapshot,
            json: serde_json::to_string(snapshot)?,
        })
    }

    pub(super) async fn persist_prepared_in(
        &self,
        db: &impl ConnectionTrait,
        prepared: PreparedManifest<'_>,
        owner: Option<&str>,
        budget: &mut Budget<'_>,
    ) -> Result<EvalSnapshot> {
        let snapshot = prepared.snapshot;
        let metadata = self.scope.metadata(&self.provenance)?;
        io::insert(
            db,
            snapshot_entity::Entity::insert(snapshot_entity::ActiveModel {
                installation_id: Set(metadata.installation),
                tenant_id: Set(metadata.tenant),
                creator_principal_id: Set(metadata.creator),
                evidence_root: Set(self.key(KeyDomain::EvalSnapshot, &snapshot.evidence_root)?),
                owner_user_id: Set(self.owner(owner.unwrap_or("*"))?),
                manifest_json: Set(prepared.json),
                result_count: Set(i32::try_from(snapshot.entries.len())?),
                frozen_at: Set(snapshot.frozen_at.clone()),
            })
            .into_query(),
            budget,
        )
        .await?;
        let row = io::one(
            db,
            self.snapshots()
                .filter(
                    snapshot_entity::Column::EvidenceRoot
                        .eq(self.key(KeyDomain::EvalSnapshot, &snapshot.evidence_root)?),
                )
                .lock_exclusive(),
            budget,
        )
        .await?
        .context("eval snapshot identity conflict")?;
        let existing = self.decode_snapshot(&row)?;
        ensure!(
            existing == *snapshot,
            "eval snapshot immutable content conflict"
        );
        Ok(existing)
    }

    pub async fn snapshot_by_root(&self, evidence_root: &str) -> Result<Option<EvalSnapshot>> {
        self.read_snapshot(evidence_root, None).await
    }

    pub async fn snapshot_by_root_for_owner(
        &self,
        evidence_root: &str,
        owner_user_id: &str,
    ) -> Result<Option<EvalSnapshot>> {
        self.legacy_only()?;
        self.read_snapshot(evidence_root, Some(owner_user_id)).await
    }

    async fn read_snapshot(&self, root: &str, owner: Option<&str>) -> Result<Option<EvalSnapshot>> {
        let tx = self.transaction(false).await?;
        let mut budget = Budget::new(self);
        let value = if let Some((snapshot, stored_owner)) =
            self.read_snapshot_in(&tx, root, owner, &mut budget).await?
        {
            if !self.scope.is_legacy() {
                self.manifest_records_in(&tx, &snapshot, &mut budget, false, Some(&stored_owner))
                    .await?;
            }
            Some(snapshot)
        } else {
            None
        };
        tx.commit().await?;
        Ok(value)
    }

    pub(super) async fn read_snapshot_in(
        &self,
        db: &impl ConnectionTrait,
        root: &str,
        owner: Option<&str>,
        budget: &mut Budget<'_>,
    ) -> Result<Option<(EvalSnapshot, String)>> {
        let mut query = self.snapshots().filter(
            snapshot_entity::Column::EvidenceRoot.eq(self.key(KeyDomain::EvalSnapshot, root)?),
        );
        if let Some(owner) = owner {
            query = query.filter(snapshot_entity::Column::OwnerUserId.eq(owner));
        }
        if let Some(row) = io::one(db, query, budget).await? {
            let snapshot = self.decode_snapshot(&row)?;
            ensure!(
                snapshot.evidence_root == root,
                "eval snapshot logical root conflict"
            );
            Ok(Some((snapshot, row.owner_user_id)))
        } else {
            Ok(None)
        }
    }

    /// Load the stored manifest and its closure once, in one stable transaction.
    pub(crate) async fn snapshot_records(&self, root: &str) -> Result<Option<SnapshotRecords>> {
        let tx = self.transaction(false).await?;
        let mut budget = Budget::new(self);
        let value = if let Some((snapshot, stored_owner)) =
            self.read_snapshot_in(&tx, root, None, &mut budget).await?
        {
            let records = self
                .manifest_records_in(&tx, &snapshot, &mut budget, false, Some(&stored_owner))
                .await?;
            Some(SnapshotRecords { snapshot, records })
        } else {
            None
        };
        tx.commit().await?;
        Ok(value)
    }

    /// Compiler bridge: one namespace, one stable read, one cumulative byte bound.
    pub(crate) async fn manifest_records(
        &self,
        snapshot: &EvalSnapshot,
    ) -> Result<Vec<(StoredEvaluationResult, EvalSubject)>> {
        if !self.scope.is_legacy() {
            self.verify_manifest(snapshot, None)?;
        }
        let tx = self.transaction(false).await?;
        let values = self
            .manifest_records_in(&tx, snapshot, &mut Budget::new(self), false, None)
            .await?;
        tx.commit().await?;
        Ok(values)
    }

    pub(super) async fn manifest_records_in(
        &self,
        db: &impl ConnectionTrait,
        snapshot: &EvalSnapshot,
        budget: &mut Budget<'_>,
        current: bool,
        stored_owner: Option<&str>,
    ) -> Result<Vec<(StoredEvaluationResult, EvalSubject)>> {
        if !self.scope.is_legacy() {
            self.verify_manifest(snapshot, None)?;
            budget.charge(
                snapshot.entries.len(),
                io::json_size(snapshot, io::MAX_BYTES)?,
            )?;
        }
        let mut records = Vec::with_capacity(snapshot.entries.len());
        let mut legacy_owners = BTreeSet::new();
        for entry in &snapshot.entries {
            let mut query = self.results().filter(
                result_entity::Column::ResultId
                    .eq(self.key(KeyDomain::EvalResult, &entry.result_id)?),
            );
            if current {
                query = query.lock_exclusive();
            }
            let row = io::one(db, query, budget)
                .await?
                .context("snapshot result is unavailable")?;
            let value = self.decode_result(&row)?;
            ensure!(
                value.result_id == entry.result_id
                    && value.content_digest == entry.content_digest
                    && value.result.eval_id == entry.eval_id,
                "snapshot result identity or content mismatch"
            );
            let mut query = self
                .subjects()
                .filter(subject_entity::Column::EvalId.eq(&row.eval_id));
            if current {
                query = query.lock_exclusive();
            }
            let subject = io::one(db, query, budget)
                .await?
                .context("snapshot subject is unavailable")?;
            let decoded_subject = self.decode_subject(&subject)?;
            ensure!(
                decoded_subject.eval_id == entry.eval_id
                    && subject.content_digest == entry.subject_content_digest
                    && subject.owner_user_id == row.owner_user_id,
                "snapshot subject identity or content mismatch"
            );
            if self.scope.is_legacy() {
                legacy_owners.insert(row.owner_user_id.clone());
            }
            records.push((value, decoded_subject));
        }
        if self.scope.is_legacy() {
            // Legacy has both wildcard and explicitly owner-bound root formats.
            // Preserve both, but do not accept a tenant root as a Legacy proof.
            let wildcard = self.snapshot_root(None, &snapshot.frozen_at, &snapshot.entries)?;
            let owned = if legacy_owners.len() == 1 {
                legacy_owners
                    .first()
                    .map(|owner| {
                        self.snapshot_root(Some(owner), &snapshot.frozen_at, &snapshot.entries)
                    })
                    .transpose()?
            } else {
                None
            };
            if snapshot.evidence_root != wildcard && owned.as_ref() != Some(&snapshot.evidence_root)
            {
                // An empty owner snapshot has no result from which to recover
                // its owner. The existing persisted row supplies that identity.
                let owner = if let Some(owner) = stored_owner {
                    owner.to_owned()
                } else {
                    let row = io::one(
                        db,
                        self.snapshots().filter(
                            snapshot_entity::Column::EvidenceRoot.eq(&snapshot.evidence_root),
                        ),
                        budget,
                    )
                    .await?
                    .context("eval manifest root is unavailable in this namespace")?;
                    ensure!(
                        self.decode_snapshot(&row)? == *snapshot,
                        "eval manifest content mismatch"
                    );
                    row.owner_user_id
                };
                self.verify_manifest(snapshot, Some(&owner))?;
                ensure!(
                    snapshot.entries.is_empty()
                        || owner == "*"
                        || (legacy_owners.len() == 1 && legacy_owners.contains(&owner)),
                    "eval manifest owner or content mismatch"
                );
            }
        }
        Ok(records)
    }
}
