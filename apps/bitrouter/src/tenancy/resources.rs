//! Explicit operator provisioning and immutable, encrypted tenant compilation.
//!
//! The parent opens this under the installation's exclusive lease, before actor
//! or listener startup. This module supplies dependency references, not grants;
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

mod compiler;
pub(crate) mod execution;
mod manifest;
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
use sea_orm::{ConnectionTrait, DatabaseConnection, TransactionTrait};
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
pub(crate) struct Resources {
    installation: InstallationId,
    key: Option<DeploymentKey>,
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
    document: Configuration,
    routes: Vec<compiler::RouteEnvelope>,
    artifacts: Vec<Artifact>,
}

/// Distinguish bounded execution projection failures from invalid compositions
/// while preserving the reader's fail-closed handling of already retained data.
#[derive(Debug, thiserror::Error)]
#[error("retained execution capacity exceeded")]
struct ExecutionCapacityExceeded;

impl Resources {
    /// Authenticate a retained configuration before activation or startup.
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
            } => Ok(credential),
            _ => Err(ErrorCode::AuthorityUnavailable),
        }
    }

    /// Open the explicit catalog and import the optional privileged manifest.
    /// All startup errors deliberately discard source/SQL diagnostics.
    pub(crate) async fn open(
        db: &DatabaseConnection,
        installation: &InstallationId,
        config: &TenancyConfig,
    ) -> Result<Self> {
        let key = config
            .wrapping_key_env
            .as_deref()
            .map(DeploymentKey::from_environment)
            .transpose()
            .map_err(|_| anyhow::anyhow!("tenancy wrapping key unavailable"))?;
        let resources = Self {
            installation: installation.clone(),
            key,
        };
        resources
            .initialize_catalog(db, config.resource_import.as_deref())
            .await?;
        Ok(resources)
    }

    async fn initialize_catalog(
        &self,
        db: &DatabaseConnection,
        import: Option<&Path>,
    ) -> Result<()> {
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
            self.load(db, &reference)
                .await
                .map_err(|_| anyhow::anyhow!("retained tenancy resource authentication failed"))?;
        }
        if let Some(path) = import {
            self.provision(db, path).await?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn with_key(installation: InstallationId, secret: [u8; 32]) -> Result<Self> {
        Ok(Self {
            installation,
            key: Some(DeploymentKey::from_bytes(secret)?),
        })
    }

    async fn provision(&self, db: &DatabaseConnection, path: &Path) -> Result<()> {
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
        self.import_bytes(db, &bytes)
            .await
            .map_err(|_| anyhow::anyhow!("tenancy resource import rejected"))
    }

    #[cfg(test)]
    pub(super) async fn provision_bytes(
        &self,
        db: &DatabaseConnection,
        bytes: &[u8],
    ) -> Result<()> {
        self.import_bytes(db, bytes).await
    }

    async fn import_bytes(&self, db: &DatabaseConnection, bytes: &[u8]) -> Result<()> {
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
        let tx = db.begin().await?;
        let mut retained: HashSet<_> = storage::references(&tx).await?.into_iter().collect();
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
        // Credentials and policy documents precede routes, independent of input order.
        for resource in manifest
            .resources
            .iter()
            .filter(|r| !matches!(r.definition, Definition::Route { .. }))
        {
            storage::retain(
                self,
                &tx,
                &Artifact {
                    reference: resource.reference.clone(),
                    definition: resource.definition.clone(),
                    envelope: None,
                },
            )
            .await?;
        }
        for resource in manifest
            .resources
            .iter()
            .filter(|r| matches!(r.definition, Definition::Route { .. }))
        {
            let policy = match resource.definition.policy_reference() {
                Some(reference) => Some(
                    self.load(&tx, reference)
                        .await
                        .map_err(|_| anyhow::anyhow!("imported policy unavailable"))?,
                ),
                None => None,
            };
            let envelope = compiler::prepare_route(resource, policy.as_ref())
                .map_err(|_| anyhow::anyhow!("invalid imported route closure"))?;
            storage::retain(
                self,
                &tx,
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
            self.closure(&tx, std::slice::from_ref(&resource.reference))
                .await
                .map_err(|_| anyhow::anyhow!("imported resource dependency unavailable"))?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn compile(
        &self,
        db: &DatabaseConnection,
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
            let artifacts = artifacts
                .into_iter()
                .filter(|artifact| !matches!(artifact.definition, Definition::Credential { .. }))
                .collect();
            let envelope = Envelope {
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
            execution::validate(envelope).map_err(|error| {
                if error.is::<ExecutionCapacityExceeded>() {
                    ErrorCode::CapacityExceeded
                } else {
                    ErrorCode::InvalidRequest
                }
            })?;
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
