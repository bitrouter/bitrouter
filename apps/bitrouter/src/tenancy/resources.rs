//! Explicit operator provisioning and immutable, encrypted tenant compilation.
//!
//! Production authenticates/imports under the authority actor bootstrap, before
//! readiness or listener publication. This module supplies dependency references, not grants;
//! the authority actor authorizes roots before compilation and the complete
//! closure before commit. It never reads the host's ambient routing catalog.
//!
//! Internal compiler profile (not public wire maxima): 4 MiB per import,
//! 256 imported/direct references, 4096 closure references per compilation, 1 MiB per
//! artifact, route envelope and final compiled plaintext, and 8 MiB aggregate
//! closure plaintext. Aggregate limits are charged before retaining each item.
//! Route resolution also preflights 256 expanded accounts/endpoints per selector
//! and an 8 MiB conservative source-expansion budget (including inherited fields
//! and candidates later filtered by protocol/preferences). These import quotas
//! do not claim runtime execution or OAuth qualification.

pub(in crate::tenancy) mod admission;
mod compiler;
pub(crate) mod execution;
pub(in crate::tenancy) mod lifecycle;
pub(in crate::tenancy) mod management;
mod manifest;
mod oauth;
mod restore;
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

fn qualify_model_policies(artifacts: &[Artifact]) -> std::result::Result<(), ErrorCode> {
    for artifact in artifacts {
        if let Definition::Policy { document, .. } = &artifact.definition {
            crate::model_policy::compile(document).map_err(|_| ErrorCode::CapabilityMismatch)?;
        }
    }
    Ok(())
}

pub(crate) const COMPILER_VERSION: &str = "tenancy-resources-v1";
const MODEL_POLICY_COMPILER_VERSION: &str = "tenancy-model-policy-v1";
const PROFILED_COMPILER_VERSION: &str = "tenancy-admission-v1";
const SHARED_HEALTH_COMPILER_VERSION: &str = "tenancy-admission-v2";
const NATIVE_COMPILER_VERSION: &str = "tenancy-native-generate-content-v1";
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
    file_pool: Option<String>,
    configured: std::sync::OnceLock<execution::catalog::Catalog>,
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
            .any(|artifact| artifact.definition.uses_model_policy())
        {
            return 7;
        }
        if artifacts
            .iter()
            .any(|artifact| artifact.definition.is_native_route())
        {
            return 6;
        }
        if artifacts
            .iter()
            .any(|artifact| artifact.definition.is_admission_profile())
        {
            return 5;
        }
        if artifacts
            .iter()
            .any(|artifact| artifact.definition.is_profiled())
        {
            return 4;
        }
        1
    }

    fn persisted_bytes(&self) -> Result<Vec<u8>> {
        if !matches!(self.format, 4..=6) {
            return manifest::bounded_bytes(self, MAX_ARTIFACT_BYTES);
        }
        // The complete artifact retains the sole profiled projection. Visible
        // profiled routes are reconstructed from it once during authenticated
        // readback; ordinary routes retain their complete projection.
        #[derive(Serialize)]
        struct Compact<'a> {
            format: u32,
            document: &'a Configuration,
            routes: Vec<&'a compiler::RouteEnvelope>,
            artifacts: &'a [Artifact],
        }
        let profiled: HashSet<_> = self
            .artifacts
            .iter()
            .filter(|artifact| matches!(artifact.definition, Definition::ProfiledRoute { .. }))
            .map(|artifact| &artifact.reference)
            .collect();
        manifest::bounded_bytes(
            &Compact {
                format: self.format,
                document: &self.document,
                routes: self
                    .routes
                    .iter()
                    .filter(|route| !profiled.contains(&route.reference))
                    .collect(),
                artifacts: &self.artifacts,
            },
            MAX_ARTIFACT_BYTES,
        )
    }

    fn validate_format(&self) -> Result<()> {
        // The authenticated marker must match the selected typed profile.
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
        let validated =
            validation::compiled(self, stored).map_err(|_| ErrorCode::AuthorityUnavailable)?;
        validated.require_capabilities()
    }

    /// Open the same authenticated typed decode used by retained-data checks.
    /// The returned view pins data only; it grants no current execution authority.
    pub(crate) fn open_execution(
        &self,
        stored: &super::configuration_store::StoredConfiguration,
    ) -> std::result::Result<execution::RetainedExecutionRevision, ErrorCode> {
        let validated =
            validation::compiled(self, stored).map_err(|_| ErrorCode::AuthorityUnavailable)?;
        validated.require_capabilities()?;
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
        if let Some(secret) = self
            .configured()
            .and_then(|catalog| catalog.credential(reference))
        {
            return Ok(
                crate::gateway::providers::auth::credentials::Credential::ApiKey {
                    value: secret.to_owned(),
                },
            );
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
            configured: std::sync::OnceLock::new(),
            file_pool: config
                .catalog
                .as_ref()
                .map(|catalog| catalog.pool_id.clone()),

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
        if let Some(pool) = self.file_pool.as_deref() {
            self.require_unclaimed_pool(db, pool).await?;
        }
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
        if let Some(pool) = self.file_pool.as_deref() {
            self.require_unclaimed_pool(db, pool).await?;
        }
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
        let mut after = None;
        loop {
            let page = storage::reference_page(db, after.as_ref())
                .await
                .map_err(|_| anyhow::anyhow!("tenancy catalog unavailable"))?;
            if page.is_empty() {
                break;
            }
            for reference in &page {
                let artifact = self.load(db, reference).await.map_err(|_| {
                    anyhow::anyhow!("retained tenancy resource authentication failed")
                })?;
                ensure!(
                    lifecycle::exact(db, &self.installation, reference)
                        .await?
                        .is_some(),
                    "current retained resource lacks lifecycle metadata"
                );
                self.authenticate_oauth(db, &artifact).await?;
                admission::index::verify(self, db, &artifact).await?;
            }
            after = page.last().cloned();
        }
        admission::index::verify_index(self, db).await?;
        self.inspect_all_configurations_in(db, 64 * 1024 * 1024, &|| Ok(()))
            .await?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn with_key(installation: InstallationId, secret: [u8; 32]) -> Result<Self> {
        Ok(Self {
            installation,
            key: Some(DeploymentKey::from_bytes(secret)?),
            approved_origins: Vec::new(),
            configured: std::sync::OnceLock::new(),
            file_pool: None,
        })
    }

    #[cfg(test)]
    pub(super) fn fixture_key(
        installation: &InstallationId,
        key_env: Option<&str>,
    ) -> Result<Self> {
        match key_env {
            Some(name) => Self::load_key(
                installation,
                &TenancyConfig {
                    wrapping_key_env: Some(name.into()),
                    ..Default::default()
                },
            ),
            None => Self::with_key(installation.clone(), rand::random()),
        }
    }

    pub(in crate::tenancy) async fn require_unclaimed_pool(
        &self,
        db: &impl ConnectionTrait,
        pool: &str,
    ) -> Result<()> {
        use sea_orm::sea_query::{Alias, Expr, Query};
        let query = Query::select()
            .expr(Expr::val(1))
            .from(Alias::new("tenancy_resource_revisions"))
            .and_where(Expr::col(Alias::new("owner_kind")).eq("operator_pool"))
            .and_where(Expr::col(Alias::new("owner_id")).eq(pool))
            .limit(1)
            .to_owned();
        ensure!(
            db.query_one(db.get_database_backend().build(&query))
                .await?
                .is_none(),
            "file catalog pool conflicts with retained managed or imported resources"
        );
        Ok(())
    }

    pub(in crate::tenancy) fn file_owned_key(
        &self,
        key: &bitrouter_ai::tenancy::authority::ResourceKey,
    ) -> bool {
        key.owner_kind == OwnerKind::OperatorPool
            && self
                .file_pool
                .as_deref()
                .or_else(|| self.configured().map(|catalog| catalog.pool()))
                == Some(key.owner_id.as_str())
    }

    pub(in crate::tenancy) fn configured(&self) -> Option<&execution::catalog::Catalog> {
        self.configured.get()
    }

    pub(in crate::tenancy) fn configure(&self, config: &crate::config::Config) -> Result<()> {
        if config.tenancy.catalog.is_none() {
            return Ok(());
        }
        let catalog = execution::catalog::Catalog::compile(config, self)?;
        ensure!(
            self.configured.set(catalog).is_ok(),
            "configured catalog already loaded"
        );
        Ok(())
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
            matches!(manifest.format, 1..=4) && manifest.installation_id == self.installation,
            "resource manifest installation or format mismatch"
        );
        ensure!(
            manifest.resources.len() <= MAX_RESOURCES,
            "resource manifest capacity exceeded"
        );
        let mut unique = HashSet::new();
        for resource in &manifest.resources {
            ensure!(
                !self.file_owned_key(&resource.reference.key()),
                "cannot import into file-owned catalog pool"
            );
            ensure!(
                manifest.format == 4 || !resource.definition.uses_model_policy(),
                "model policy requires import format four"
            );
            ensure!(
                matches!(manifest.format, 3 | 4) || !resource.definition.is_native_route(),
                "native route requires import format three"
            );
            ensure!(
                unique.insert(resource.reference.clone()),
                "duplicate imported resource"
            );
            resource.validate()?;
        }
        admission::preflight_import(self, db, &manifest.resources).await?;
        self.preflight_oauth(db, &manifest.resources).await?;
        // Credentials and policy documents precede routes, independent of input order.
        for resource in manifest
            .resources
            .iter()
            .filter(|r| !r.definition.is_route())
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
            // Later members of the same health group can resolve this witness
            // immediately. Its managed head must already exist in this same
            // import transaction; a later graph error rolls everything back.
            admission::initialize_import_heads(self, db, std::slice::from_ref(resource)).await?;
            self.retain_oauth(db, resource).await?;
        }
        for resource in manifest
            .resources
            .iter()
            .filter(|r| r.definition.is_route())
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
            ensure!(
                !self.file_owned_key(&resource.reference.key()),
                "cannot import into file-owned catalog pool"
            );
            let closure = self
                .closure(db, std::slice::from_ref(&resource.reference))
                .await
                .map_err(|_| anyhow::anyhow!("imported resource dependency unavailable"))?;
            admission::validate_graph(&closure)?;
        }
        admission::initialize_import_heads(self, db, &manifest.resources).await?;
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
        qualify_model_policies(&artifacts)?;
        let routes = compiler::configuration_routes(document, &artifacts)?;
        let compiler_version = match Envelope::format(&artifacts) {
            7 => MODEL_POLICY_COMPILER_VERSION,
            6 => NATIVE_COMPILER_VERSION,
            5 => SHARED_HEALTH_COMPILER_VERSION,
            4 => PROFILED_COMPILER_VERSION,
            _ => COMPILER_VERSION,
        };
        let binding = (
            &self.installation,
            tenant,
            revision,
            compiler_version,
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
            let bytes = envelope
                .persisted_bytes()
                .map_err(|_| ErrorCode::CapacityExceeded)?;
            // The same typed projection used by activation/GET must accept the
            // output before the caller can persist it. Consume the envelope
            // after serialization: validation does not change retained bytes
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
            compiler_version: compiler_version.into(),
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
