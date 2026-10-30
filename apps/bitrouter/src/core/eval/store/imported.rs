//! Exact, private Legacy-root import evidence. This is never live authority.
use super::*;
use bitrouter_ai::tenancy::identity::{Digest, InstallationId, PrincipalId, TenantId};
use sha2::{Digest as _, Sha256};

pub(super) mod entity {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "eval_import_provenance")]
    pub struct Model {
        pub installation_id: String,
        pub tenant_id: String,
        pub creator_principal_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub snapshot_key: String,
        pub manifest_sha256: String,
        pub provenance_digest: String,
        pub provenance_json: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
impl io::Columns for entity::Model {
    const MAXIMUM_BYTES: usize = 16 * 1024;
    const NAMES: &'static [&'static str] = &[
        "installation_id",
        "tenant_id",
        "creator_principal_id",
        "snapshot_key",
        "manifest_sha256",
        "provenance_digest",
        "provenance_json",
    ];
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Evidence {
    pub version: u8,
    pub source_installation: InstallationId,
    pub source_kind: String,
    pub source_owner: String,
    pub original_root: String,
    pub manifest_sha256: Digest,
    pub mapping_digest: Digest,
    pub database_evidence_digest: Digest,
    pub ownership_evidence_digest: Digest,
    pub partition_digest: Digest,
    pub target_installation: InstallationId,
    pub target_tenant: TenantId,
    pub target_creator: PrincipalId,
    pub snapshot_key: String,
}
pub(super) fn manifest_sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
impl Evidence {
    pub(super) fn digest(&self) -> Result<String> {
        Ok(manifest_sha(&serde_json::to_vec(&(
            "bitrouter.eval.import-provenance.v1",
            self,
        ))?))
    }
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && matches!(
                    self.source_kind.as_str(),
                    "local_user" | "shared_user" | "native_owner"
                ),
            "import provenance version/namespace"
        );
        validate_owner(&self.source_owner)?;
        ensure!(
            self.source_owner != "*",
            "unresolved snapshot cannot be imported"
        );
        ensure!(
            self.source_installation != self.target_installation,
            "source/target identity overlap"
        );
        ensure!(
            crate::storage::scope::physical_key_for_namespace(
                &self.target_installation,
                &self.target_tenant,
                KeyDomain::EvalSnapshot,
                &[&self.original_root]
            )? == self.snapshot_key,
            "import physical identity mismatch"
        );
        Ok(())
    }
}
impl EvalStore {
    pub(super) async fn decode_snapshot_in(
        &self,
        db: &impl ConnectionTrait,
        row: &snapshot_entity::Model,
        budget: &mut Budget<'_>,
    ) -> Result<EvalSnapshot> {
        if self.scope.is_legacy() {
            return self.decode_snapshot(row);
        }
        let snapshot = codec::Decoder { scope: &self.scope }.snapshot_projection(row)?;
        ensure!(
            snapshot.entries.len() <= io::MAX_ROWS,
            "import snapshot row bound"
        );
        // Imported entry order is part of its original Legacy root. Native
        // snapshots still require their unchanged sorted/unique shape below.
        let record = io::one(
            db,
            entity::Entity::find()
                .filter(
                    self.scope
                        .predicate(entity::Column::InstallationId, entity::Column::TenantId),
                )
                .filter(entity::Column::SnapshotKey.eq(&row.evidence_root)),
            budget,
        )
        .await?;
        if let Some(record) = record {
            let proof: Evidence = serde_json::from_str(&record.provenance_json)?;
            proof.validate()?;
            ensure!(
                record.provenance_json.len() <= 16 * 1024
                    && proof.digest()? == record.provenance_digest
                    && proof.target_installation.as_str() == record.installation_id
                    && proof.target_tenant.as_str() == record.tenant_id
                    && proof.target_creator.as_str() == record.creator_principal_id
                    && row.creator_principal_id.as_deref()
                        == Some(record.creator_principal_id.as_str())
                    && proof.snapshot_key == record.snapshot_key
                    && proof.original_root == snapshot.evidence_root
                    && proof.manifest_sha256.as_str() == record.manifest_sha256
                    && manifest_sha(row.manifest_json.as_bytes()) == record.manifest_sha256,
                "import provenance does not bind this exact stored manifest"
            );
            self.scope.validate_metadata(MetadataRef {
                installation: Some(&record.installation_id),
                tenant: Some(&record.tenant_id),
                creator: Some(&record.creator_principal_id),
            })?;
            let legacy = Scope::legacy();
            codec::Decoder { scope: &legacy }
                .verify_manifest(&snapshot, Some(&proof.source_owner))?;
        } else {
            self.verify_manifest(&snapshot, None)?;
        }
        Ok(snapshot)
    }
    /// External manifests do not select their own import interpretation. They
    /// must equal the manifest fetched under this actual current scope and proof.
    pub(super) async fn verify_stored_or_native(
        &self,
        db: &impl ConnectionTrait,
        snapshot: &EvalSnapshot,
        budget: &mut Budget<'_>,
    ) -> Result<()> {
        if self.verify_manifest(snapshot, None).is_ok() {
            return Ok(());
        }
        let row = io::one(
            db,
            self.snapshots().filter(
                snapshot_entity::Column::EvidenceRoot
                    .eq(self.key(KeyDomain::EvalSnapshot, &snapshot.evidence_root)?),
            ),
            budget,
        )
        .await?
        .context("imported manifest unavailable")?;
        ensure!(
            self.decode_snapshot_in(db, &row, budget).await? == *snapshot,
            "imported manifest differs from stored evidence"
        );
        Ok(())
    }
}
