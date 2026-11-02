//! Explicit operator provisioning and immutable, encrypted tenant compilation.
//!
//! Production authenticates/imports under the authority actor bootstrap, before
//! readiness or listener publication. This module supplies dependency references, not grants;
//! the authority actor authorizes roots before compilation and the complete
//! closure before commit. It never reads the host's ambient routing catalog.
//!
//! Internal compiler profile (not public wire maxima): 4 MiB per import,
//! 256 imported/direct references, 4096 retained/closure references, 1 MiB per
//! artifact, route envelope and final compiled plaintext, and 8 MiB aggregate
//! closure plaintext. Aggregate limits are charged before retaining each item.
//! Route resolution also preflights 256 expanded accounts/endpoints per selector
//! and an 8 MiB conservative source-expansion budget (including inherited fields
//! and candidates later filtered by protocol/preferences). These import quotas
//! do not claim runtime execution or OAuth qualification.

pub(crate) mod block;
mod checkpoint;
mod compiler;
mod core_choice;
pub(crate) mod execution;
#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) mod learned;
pub(in crate::tenancy) mod lifecycle;
pub(in crate::tenancy) mod management;
mod manifest;
mod oauth;
mod recovery;
mod storage;
#[cfg(test)]
mod tests;
mod validation;

use std::{collections::HashSet, path::Path};

use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::{Configuration, ResourceRef},
    identity::{InstallationId, RevisionId, TenantId},
    operation::ErrorCode,
    validation::Validate,
};
#[cfg(test)]
use sea_orm::TransactionTrait;
use sea_orm::{ConnectionTrait, DatabaseConnection};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::{config::tenancy::TenancyConfig, runtime_crypto::DeploymentKey};

use manifest::{Artifact, Definition, Manifest};

pub(crate) const COMPILER_VERSION: &str = "tenancy-resources-v1";
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;
const MAX_CLOSURE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESOURCES: usize = 256;
const MAX_CLOSURE: usize = 4096;

/// One installation's explicitly supplied wrapping key. No default is generated.
pub(super) struct Resources {
    installation: InstallationId,
    key: Option<DeploymentKey>,
    approved_origins: Vec<String>,
}

/// Safe for configuration_store.compiled_json; definitions are ciphertext only.
pub(crate) struct CompiledConfiguration {
    pub(crate) payload: Value,
    pub(crate) dependencies: Vec<ResourceRef>,
    pub(crate) compiler_version: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    #[serde(default = "Envelope::v1", skip_serializing_if = "Envelope::is_v1")]
    format: u32,
    document: Configuration,
    routes: Vec<compiler::RouteEnvelope>,
    artifacts: Vec<Artifact>,
}

impl Envelope {
    fn v1() -> u32 {
        1
    }

    fn is_v1(format: &u32) -> bool {
        *format == 1
    }

    fn format(artifacts: &[Artifact]) -> u32 {
        if artifacts
            .iter()
            .any(|artifact| matches!(artifact.definition, Definition::Checkpoint { .. }))
        {
            return 3;
        }
        if artifacts
            .iter()
            .any(|artifact| artifact.definition.requires_envelope_v2())
        {
            2
        } else {
            1
        }
    }

    fn validate_format(&self) -> Result<()> {
        // Format 1 is deliberately omitted: historical plaintext, AAD and
        // compiler identifiers stay exact. Old deny-unknown-fields readers
        // reject format 2, whose independent profiles may coexist.
        ensure!(
            self.format == Self::format(&self.artifacts),
            "unsupported retained profile format"
        );
        Ok(())
    }
}

/// Distinguish bounded execution projection failures from invalid compositions
/// while preserving the reader's fail-closed handling of already retained data.
#[derive(Debug, thiserror::Error)]
#[error("retained execution capacity exceeded")]
struct ExecutionCapacityExceeded;

impl Resources {
    pub(super) fn continuation_crypto(&self) -> Option<super::continuation::Crypto> {
        self.key
            .clone()
            .map(|key| super::continuation::Crypto::new(self.installation.clone(), key))
    }

    #[cfg(feature = "core")]
    pub(in crate::tenancy) fn trajectory_crypto(
        &self,
        installation: &InstallationId,
    ) -> Result<crate::runtime_crypto::trajectory::TrajectoryCrypto> {
        ensure!(
            &self.installation == installation,
            "trajectory installation mismatch"
        );
        self.key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("explicit trajectory wrapping key unavailable"))?
            .trajectory()
    }

    /// Authenticate a retained configuration, including integrity-only reads.
    /// This reads no mutable catalog or global configuration. Invalid retained
    /// bytes, missing keys and unsupported formats fence authority.
    pub(crate) fn validate_compiled(
        &self,
        stored: &super::configuration_store::StoredConfiguration,
    ) -> std::result::Result<(), ErrorCode> {
        validation::compiled(self, stored)
            .map(|_| ())
            .map_err(|_| ErrorCode::AuthorityUnavailable)
    }

    /// Activation and startup must also qualify the authenticated routes for
    /// this binary. A missing capability is not retained-data corruption.
    pub(crate) fn validate_executable(
        &self,
        stored: &super::configuration_store::StoredConfiguration,
    ) -> std::result::Result<(), ErrorCode> {
        validation::compiled(self, stored)
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .require_capabilities()
    }

    /// Open the same authenticated typed decode used by retained-data checks.
    /// The returned view pins data only; it grants no current execution authority.
    pub(crate) fn open_execution(
        &self,
        stored: &super::configuration_store::StoredConfiguration,
    ) -> std::result::Result<execution::RetainedExecutionRevision, ErrorCode> {
        let validated =
            validation::compiled(self, stored).map_err(|_| ErrorCode::AuthorityUnavailable)?;
        execution::RetainedExecutionRevision::new(
            self.installation.clone(),
            stored.revision.tenant_id.clone(),
            stored.revision.revision_id.clone(),
            stored.authority_revision,
            validated,
        )
    }

    /// Same authenticated decoder, charging the original borrowed bootstrap
    /// operation before each target/native expansion. It grants no authority.
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn open_paid_recovery(
        &self,
        stored: &super::configuration_store::StoredConfiguration,
        budget: &crate::storage::operation_budget::OperationBudget<'_>,
    ) -> Result<execution::RetainedExecutionRevision> {
        let validated = validation::compiled_with_operation(self, stored, Some(budget))?;
        execution::RetainedExecutionRevision::new(
            self.installation.clone(),
            stored.revision.tenant_id.clone(),
            stored.revision.revision_id.clone(),
            stored.authority_revision,
            validated,
        )
        .map_err(|_| anyhow::anyhow!("paid retained identifier normalization failed"))
    }

    /// Read one exact separately owned static credential slot. No ambient
    /// provider/account lookup or refresh is performed; callers authorize use.
    pub(crate) async fn credential(
        &self,
        db: &DatabaseConnection,
        reference: &ResourceRef,
    ) -> std::result::Result<crate::gateway::providers::auth::credentials::Credential, ErrorCode>
    {
        if reference.resource_kind != ResourceKind::Credential {
            return Err(ErrorCode::InvalidRequest);
        }
        let artifact = self.load(db, reference).await?;
        if artifact.reference != *reference || artifact.envelope.is_some() {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        match artifact.definition {
            Definition::Credential {
                credential:
                    credential @ crate::gateway::providers::auth::credentials::Credential::ApiKey {
                        ..
                    },
                oauth_generation: 0,
                oauth: None,
            } => Ok(credential),
            _ => Err(ErrorCode::AuthorityUnavailable),
        }
    }

    #[cfg(test)]
    /// Open a test catalog without a serving authority. Production uses actor bootstrap.
    /// All startup errors deliberately discard source/SQL diagnostics.
    pub(crate) async fn open(
        db: &DatabaseConnection,
        installation: &InstallationId,
        config: &TenancyConfig,
    ) -> Result<Self> {
        let resources = Self::load_key(installation, config)?;
        resources
            .initialize_catalog(db, config.resource_import.as_deref())
            .await?;
        Ok(resources)
    }

    pub(super) fn load_key(installation: &InstallationId, config: &TenancyConfig) -> Result<Self> {
        let key = config
            .wrapping_key_env
            .as_deref()
            .map(DeploymentKey::from_environment)
            .transpose()
            .map_err(|_| anyhow::anyhow!("tenancy wrapping key unavailable"))?;
        Ok(Self {
            installation: installation.clone(),
            key,
            approved_origins: config
                .resource_origins
                .iter()
                .map(|origin| {
                    url::Url::parse(origin)
                        .map(|url| url.origin().ascii_serialization())
                        .map_err(|_| anyhow::anyhow!("invalid approved resource origin"))
                })
                .collect::<Result<_>>()?,
        })
    }

    #[cfg(test)]
    async fn initialize_catalog(
        &self,
        db: &DatabaseConnection,
        import: Option<&Path>,
    ) -> Result<()> {
        self.authenticate_retained(db).await?;
        if let Some(path) = import {
            let bytes = Self::read_import(Some(path))
                .await?
                .ok_or_else(|| anyhow::anyhow!("missing import"))?;
            self.import_bytes(db, &bytes).await?;
        }
        Ok(())
    }

    pub(super) async fn bootstrap(
        &self,
        db: &impl ConnectionTrait,
        import: Option<&[u8]>,
    ) -> Result<()> {
        self.authenticate_retained(db).await?;
        if let Some(bytes) = import {
            self.import_in(db, bytes).await?;
        }
        Ok(())
    }

    async fn authenticate_retained(&self, db: &impl ConnectionTrait) -> Result<()> {
        let retained = storage::has_resources(db)
            .await
            .map_err(|_| anyhow::anyhow!("tenancy catalog unavailable"))?;
        ensure!(
            self.key.is_some() || !retained,
            "retained tenancy catalog requires its wrapping key"
        );
        // Verify all retained rows even when the import source has been removed.
        // Verify BEFORE importing: a wrong key must not commit new rows and leave
        // a mixed-key catalog behind when startup subsequently fails.
        for reference in storage::references(db)
            .await
            .map_err(|_| anyhow::anyhow!("tenancy catalog unavailable"))?
        {
            let artifact = self
                .load(db, &reference)
                .await
                .map_err(|_| anyhow::anyhow!("retained tenancy resource authentication failed"))?;
            self.authenticate_oauth(db, &artifact).await?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn with_key(installation: InstallationId, secret: [u8; 32]) -> Result<Self> {
        Ok(Self {
            installation,
            key: Some(DeploymentKey::from_bytes(secret)?),
            approved_origins: Vec::new(),
        })
    }

    pub(super) async fn read_import(path: Option<&Path>) -> Result<Option<Vec<u8>>> {
        let Some(path) = path else {
            return Ok(None);
        };
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| anyhow::anyhow!("tenancy resource manifest unavailable"))?;
        let metadata = file
            .metadata()
            .await
            .map_err(|_| anyhow::anyhow!("tenancy resource manifest unavailable"))?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_MANIFEST_BYTES as u64,
            "tenancy resource manifest exceeds file bounds"
        );
        let mut bytes = Vec::new();
        file.take((MAX_MANIFEST_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| anyhow::anyhow!("tenancy resource manifest unavailable"))?;
        ensure!(
            bytes.len() <= MAX_MANIFEST_BYTES,
            "tenancy resource manifest exceeds file bounds"
        );
        Ok(Some(bytes))
    }

    #[cfg(test)]
    pub(super) async fn provision_bytes(
        &self,
        db: &DatabaseConnection,
        bytes: &[u8],
    ) -> Result<()> {
        self.import_bytes(db, bytes).await
    }

    #[cfg(test)]
    async fn import_bytes(&self, db: &DatabaseConnection, bytes: &[u8]) -> Result<()> {
        let transaction = db.begin().await?;
        match self.import_in(&transaction, bytes).await {
            Ok(()) => {
                transaction.commit().await?;
                Ok(())
            }
            Err(error) => {
                transaction.rollback().await?;
                Err(error)
            }
        }
    }

    async fn import_in(&self, db: &impl ConnectionTrait, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= MAX_MANIFEST_BYTES,
            "resource manifest capacity exceeded"
        );
        ensure!(self.key.is_some(), "resource import requires wrapping key");
        let manifest: Manifest = serde_json::from_slice(bytes)
            .map_err(|_| anyhow::anyhow!("invalid resource manifest"))?;
        ensure!(
            manifest.format == 1 && manifest.installation_id == self.installation,
            "resource manifest installation or format mismatch"
        );
        ensure!(
            manifest.resources.len() <= MAX_RESOURCES,
            "resource manifest capacity exceeded"
        );
        let mut unique = HashSet::new();
        for resource in &manifest.resources {
            ensure!(
                unique.insert(resource.reference.clone()),
                "duplicate imported resource"
            );
            resource.validate()?;
        }
        let mut retained: HashSet<_> = storage::references(db).await?.into_iter().collect();
        retained.extend(
            manifest
                .resources
                .iter()
                .map(|resource| resource.reference.clone()),
        );
        ensure!(
            retained.len() <= MAX_CLOSURE,
            "retained resource capacity exceeded"
        );
        self.preflight_oauth(db, &manifest.resources).await?;
        // Credentials and policy documents precede routes, independent of input order.
        for resource in manifest
            .resources
            .iter()
            .filter(|r| !matches!(r.definition, Definition::Route { .. }))
        {
            storage::retain(
                self,
                db,
                &Artifact {
                    reference: resource.reference.clone(),
                    definition: resource.definition.clone(),
                    envelope: None,
                },
            )
            .await?;
            self.retain_oauth(db, resource).await?;
        }
        for resource in manifest
            .resources
            .iter()
            .filter(|r| matches!(r.definition, Definition::Route { .. }))
        {
            let policy = match resource.definition.policy_reference() {
                Some(reference) => Some(
                    self.load(db, reference)
                        .await
                        .map_err(|_| anyhow::anyhow!("imported policy unavailable"))?,
                ),
                None => None,
            };
            let envelope = compiler::prepare_route(resource, policy.as_ref())
                .map_err(|_| anyhow::anyhow!("invalid imported route closure"))?;
            storage::retain(
                self,
                db,
                &Artifact {
                    reference: resource.reference.clone(),
                    definition: resource.definition.clone(),
                    envelope: Some(envelope),
                },
            )
            .await?;
        }
        // Validate the complete dependency graph before the import becomes visible.
        for resource in &manifest.resources {
            let closure = self
                .closure(db, std::slice::from_ref(&resource.reference))
                .await
                .map_err(|_| anyhow::anyhow!("imported resource dependency unavailable"))?;
            core_choice::validate_graph(&closure, &[], true)?;
        }
        Ok(())
    }

    pub(crate) async fn compile(
        &self,
        db: &impl ConnectionTrait,
        tenant: &TenantId,
        revision: &RevisionId,
        document: &Configuration,
    ) -> std::result::Result<CompiledConfiguration, ErrorCode> {
        if document
            .routes
            .len()
            .saturating_add(usize::from(document.policy.is_some()))
            > 256
        {
            return Err(ErrorCode::CapacityExceeded);
        }
        manifest::bounded_bytes(document, MAX_ARTIFACT_BYTES)
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        document.validate().map_err(|_| ErrorCode::InvalidRequest)?;
        let mut names = HashSet::new();
        if !document
            .routes
            .iter()
            .all(|r| names.insert(r.resource_id.clone()))
        {
            return Err(ErrorCode::InvalidRequest);
        }
        let roots: Vec<_> = document
            .routes
            .iter()
            .chain(document.policy.iter())
            .cloned()
            .collect();
        let artifacts = self.closure(db, &roots).await?;
        let dependencies: Vec<_> = artifacts.iter().map(|a| a.reference.clone()).collect();
        if dependencies
            .iter()
            .any(|r| r.owner_kind == OwnerKind::Tenant && r.owner_id.as_str() != tenant.as_str())
        {
            return Err(ErrorCode::Forbidden);
        }
        let routes = compiler::configuration_routes(document, &artifacts)?;
        let binding = (
            &self.installation,
            tenant,
            revision,
            COMPILER_VERSION,
            document,
            &dependencies,
        );
        manifest::ByteBudget::new(MAX_ARTIFACT_BYTES - 256)
            .charge(&binding)
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        let payload = if roots.is_empty() {
            // No retained definitions, keys or dispatch namespace exist in this case.
            json!({"kind": "empty", "binding": binding})
        } else {
            let artifacts: Vec<_> = artifacts
                .into_iter()
                .filter(|artifact| !matches!(artifact.definition, Definition::Credential { .. }))
                .collect();
            let envelope = Envelope {
                format: Envelope::format(&artifacts),
                document: document.clone(),
                routes,
                artifacts,
            };
            let bytes = manifest::bounded_bytes(&envelope, MAX_ARTIFACT_BYTES)
                .map_err(|_| ErrorCode::CapacityExceeded)?;
            // The same typed projection used by activation/GET must accept the
            // output before the caller can persist it. Consume the envelope
            // after serialization: validation does not change compiler-v1 bytes
            // or the authenticated binding, and needs no extra envelope clone.
            let validated = execution::validate(envelope).map_err(|error| {
                if error.is::<ExecutionCapacityExceeded>() {
                    ErrorCode::CapacityExceeded
                } else {
                    ErrorCode::InvalidRequest
                }
            })?;
            validated.require_capabilities()?;
            let key = self.key.as_ref().ok_or(ErrorCode::AuthorityUnavailable)?;
            let sealed = key
                .seal_bound("configuration", &binding, &bytes)
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            json!({"kind": "sealed", "sealed": sealed})
        };
        Ok(CompiledConfiguration {
            payload,
            dependencies,
            compiler_version: COMPILER_VERSION.into(),
        })
    }

    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) async fn recovery_static_credentials(
        &self,
        db: &impl ConnectionTrait,
        references: &[ResourceRef],
        budget: &super::session::recovery::Budget,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        for reference in references.iter().filter(|r| {
            r.resource_kind == bitrouter_ai::tenancy::authority::ResourceKind::Credential
        }) {
            check()?;
            // The existing artifact decoder caps the encrypted row before transfer.
            // Reserve its maximum transfer/decode allowance before calling it.
            budget.charge(
                1,
                MAX_ARTIFACT_BYTES
                    .checked_mul(2)
                    .ok_or_else(|| anyhow::anyhow!("artifact budget overflow"))?,
            )?;
            let artifact = self
                .load(db, reference)
                .await
                .map_err(|_| anyhow::anyhow!("recovery credential unavailable"))?;
            check()?;
            ensure!(
                matches!(
                    artifact.definition,
                    manifest::Definition::Credential {
                        credential:
                            crate::gateway::providers::auth::credentials::Credential::ApiKey { .. },
                        oauth: None,
                        oauth_generation: 0,
                    }
                ),
                "recovery requires immutable static credentials"
            );
        }
        Ok(())
    }
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) async fn recovery_static_profile(
        &self,
        db: &impl ConnectionTrait,
        references: &[ResourceRef],
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<bool> {
        for reference in references.iter().filter(|r| {
            r.resource_kind == bitrouter_ai::tenancy::authority::ResourceKind::Credential
        }) {
            check()?;
            let artifact = self
                .load(db, reference)
                .await
                .map_err(|_| anyhow::anyhow!("native credential profile unavailable"))?;
            check()?;
            if !matches!(
                artifact.definition,
                manifest::Definition::Credential {
                    credential: crate::gateway::providers::auth::credentials::Credential::ApiKey { .. },
                    oauth: None,
                    oauth_generation: 0,
                }
            ) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) async fn paid_recovery_static(
        &self,
        db: &impl ConnectionTrait,
        reference: &ResourceRef,
        budget: &crate::storage::operation_budget::OperationBudget<'_>,
    ) -> Result<()> {
        // Charge the existing authenticated decoder's maximum before transfer.
        budget.charge(
            1,
            MAX_ARTIFACT_BYTES
                .checked_mul(2)
                .ok_or_else(|| anyhow::anyhow!("paid credential bound"))?,
        )?;
        let artifact = self
            .load(db, reference)
            .await
            .map_err(|_| anyhow::anyhow!("paid retained credential unavailable"))?;
        budget.check()?;
        ensure!(
            artifact.reference == *reference
                && matches!(
                    artifact.definition,
                    manifest::Definition::Credential {
                        credential:
                            crate::gateway::providers::auth::credentials::Credential::ApiKey { .. },
                        oauth: None,
                        oauth_generation: 0,
                    }
                ),
            "paid recovery requires immutable static credential"
        );
        Ok(())
    }

    async fn load(
        &self,
        db: &impl ConnectionTrait,
        reference: &ResourceRef,
    ) -> std::result::Result<Artifact, ErrorCode> {
        let record = super::configuration_store::resource(db, reference)
            .await
            .map_err(|_| ErrorCode::StorageUnavailable)?
            .ok_or(ErrorCode::NotFound)?;
        storage::authenticate(self, record).map_err(|_| ErrorCode::AuthorityUnavailable)
    }

    async fn closure(
        &self,
        db: &impl ConnectionTrait,
        roots: &[ResourceRef],
    ) -> std::result::Result<Vec<Artifact>, ErrorCode> {
        let mut queue = roots.to_vec();
        let mut seen = HashSet::new();
        let mut artifacts = Vec::new();
        let mut budget = manifest::ByteBudget::new(MAX_CLOSURE_BYTES);
        let mut index = 0;
        while index < queue.len() {
            let reference = queue[index].clone();
            index += 1;
            if !seen.insert(reference.clone()) {
                continue;
            }
            if seen.len() > MAX_CLOSURE {
                return Err(ErrorCode::CapacityExceeded);
            }
            let artifact = self.load(db, &reference).await?;
            budget
                .charge(&artifact)
                .map_err(|_| ErrorCode::CapacityExceeded)?;
            for dependency in artifact.definition.dependencies() {
                if !seen.contains(&dependency) && !queue.contains(&dependency) {
                    queue.push(dependency);
                }
            }
            if queue.len() > MAX_CLOSURE {
                return Err(ErrorCode::CapacityExceeded);
            }
            artifacts.push(artifact);
        }
        Ok(artifacts)
    }
}
