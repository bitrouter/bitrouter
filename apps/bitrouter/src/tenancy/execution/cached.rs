//! Service-local immutable bytes and views, never cached execution permission.
//!
//! Installation, process instance and wrapping-key/compiler epoch are implicit
//! in the owning Service: none can be replaced during its lifetime. Every key
//! additionally names the tenant, exact resource revision and published authority
//! revision. An old miss can populate only its old namespace after revalidation.

use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
mod tests;

use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    identity::AuthorityRevision,
    operation::ErrorCode,
    ordered::{CandidateManifest, GenerationProtocol},
};

use super::{Service, Submission, authorize_route};
use crate::{
    gateway::providers::auth::credentials::Credential,
    tenancy::{
        cache::{Cache, CacheError, Pin},
        configuration_store,
        error::Failure,
        resources::execution::RetainedExecutionRevision,
    },
};

pub(in crate::tenancy) struct Caches {
    executions: Cache<ExecutionKey, RetainedExecutionRevision>,
    credentials: Cache<CredentialKey, StaticCredential>,
    closed: AtomicBool,
}

impl Caches {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(in crate::tenancy) fn observed(
        &self,
    ) -> [Result<
        crate::telemetry::receipt_latency::capacity::runtime::CacheUsage,
        crate::telemetry::receipt_latency::capacity::ReadError,
    >; 2] {
        [self.executions.observed(), self.credentials.observed()]
    }

    pub(in crate::tenancy) fn new(
        config: &crate::config::tenancy::capacity::Caches,
    ) -> anyhow::Result<Self> {
        Self::bounded(
            config.execution_entries,
            config.execution_bytes,
            config.credential_entries,
            config.credential_bytes,
        )
    }

    #[cfg(test)]
    pub(in crate::tenancy) fn with_limits(
        execution_entries: usize,
        execution_bytes: usize,
        credential_entries: usize,
        credential_bytes: usize,
    ) -> anyhow::Result<Self> {
        Self::bounded(
            execution_entries,
            execution_bytes,
            credential_entries,
            credential_bytes,
        )
    }

    fn bounded(
        execution_entries: usize,
        execution_bytes: usize,
        credential_entries: usize,
        credential_bytes: usize,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            executions: Cache::new(execution_entries, execution_bytes)
                .map_err(|_| anyhow::anyhow!("invalid tenant execution cache bounds"))?,
            credentials: Cache::new(credential_entries, credential_bytes)
                .map_err(|_| anyhow::anyhow!("invalid tenant credential cache bounds"))?,
            closed: AtomicBool::new(false),
        })
    }

    // The Service's authority write lock linearizes this with publication under
    // with_dispatch_authority. Cache mutation never awaits or acquires authority.
    pub(in crate::tenancy) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(in crate::tenancy) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

#[derive(PartialEq, Eq)]
enum ExecutionKey {
    Dispatch {
        authority_revision: u64,
        tenant: Box<str>,
        revision: Box<str>,
        binding: Box<str>,
    },
    // Stored bytes, not just an ID or a digest: a same-revision ciphertext/AAD
    // replacement must go through the authenticated decoder again. Service
    // ownership fixes installation/store/wrapping-key/compiler context.
    Control {
        authority_revision: u64,
        binding: Box<str>,
    },
}

impl ExecutionKey {
    fn heap_bytes(&self) -> usize {
        // Validated identities have finite wire bounds; Box<str> has no spare
        // capacity. Inline key storage is charged by the cache's entry vector.
        match self {
            Self::Dispatch {
                tenant,
                revision,
                binding,
                ..
            } => tenant.len() + revision.len() + binding.len(),
            Self::Control { binding, .. } => binding.len(),
        }
    }
}

#[derive(PartialEq, Eq)]
struct CredentialKey {
    authority_revision: u64,
    tenant: Box<str>,
    owner_kind: OwnerKind,
    owner: Box<str>,
    kind: ResourceKind,
    resource: Box<str>,
    revision: Box<str>,
}

impl CredentialKey {
    fn heap_bytes(&self) -> usize {
        self.tenant.len() + self.owner.len() + self.resource.len() + self.revision.len()
    }
}

struct StaticCredential {
    secret: Box<str>,
}

impl Service {
    fn with_control_cache<T>(
        &self,
        revision: AuthorityRevision,
        access: impl FnOnce(&Cache<ExecutionKey, RetainedExecutionRevision>) -> Result<T, CacheError>,
    ) -> Result<T, Failure> {
        // Shutdown closes caches under the corresponding authority write gate.
        // Neither this lookup/publication nor cache eviction performs I/O.
        let authority = self.authority.read().map_err(Failure::from)?;
        if !self.live.load(Ordering::Acquire) || self.caches.is_closed() {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        if authority.header().revision != revision {
            return Err(ErrorCode::RevisionConflict.into());
        }
        access(&self.caches.executions).map_err(|error| self.cache_failure(error))
    }

    pub(in crate::tenancy) fn manifest_from_stored(
        &self,
        stored: &configuration_store::StoredConfiguration,
        authority_revision: AuthorityRevision,
        reference: &ResourceRef,
        inbound: GenerationProtocol,
    ) -> Result<CandidateManifest, Failure> {
        let key = ExecutionKey::Control {
            authority_revision: u64::from(authority_revision),
            binding: configuration_store::execution_cache_binding(stored)
                .map_err(|_| self.unavailable())?,
        };
        if let Some(view) = self.with_control_cache(authority_revision, |cache| cache.get(&key))? {
            return view
                .candidate_manifest(reference, inbound, authority_revision)
                .map_err(Failure::from);
        }
        // The caller still owns the original fail-fast read permit. Duplicate
        // misses are bounded by those eight slots, without a cache wait queue.
        let view = self
            .resources
            .open_execution(stored)
            .map_err(|code| match code {
                ErrorCode::InvalidRequest | ErrorCode::CapabilityMismatch => code.into(),
                _ => self.unavailable(),
            })?;
        let heap = match view.retained_heap_bytes() {
            Ok(heap) => heap,
            // Weight traversal has its own bounded-work limit. Failure to cache
            // must not reject a previously valid, bounded management read.
            Err(ErrorCode::CapacityExceeded) => {
                return view
                    .candidate_manifest(reference, inbound, authority_revision)
                    .map_err(Failure::from);
            }
            Err(error) => return Err(error.into()),
        };
        let key_heap = key.heap_bytes();
        let mut candidate = Some(view);
        let cached = self.with_control_cache(authority_revision, |cache| {
            cache.insert_if_fits(key, &mut candidate, heap, key_heap)
        })?;
        let view = match cached.as_deref().or(candidate.as_ref()) {
            Some(view) => view,
            None => return Err(self.unavailable()),
        };
        // DTO construction runs outside the authority/cache locks, and retains
        // no Arc into the cached view. The ordered caller still performs its
        // fresh principal, Extend, lease, observation and final authority fence.
        view.candidate_manifest(reference, inbound, authority_revision)
            .map_err(Failure::from)
    }

    fn cache_failure(&self, error: CacheError) -> Failure {
        match error {
            CacheError::Capacity => ErrorCode::CapacityExceeded.into(),
            CacheError::Unavailable => self.unavailable(),
        }
    }

    fn read_deadline(&self, submission: &Submission) -> tokio::time::Instant {
        submission
            .deadline
            .min(tokio::time::Instant::now() + crate::tenancy::MANAGEMENT_TIMEOUT)
    }

    fn read_timeout(&self, submission: &Submission) -> Failure {
        if submission.deadline.expired() {
            ErrorCode::CapacityExceeded.into()
        } else {
            self.unavailable()
        }
    }

    fn check_cached_revision(
        &self,
        submission: &Submission,
        revision: &RetainedExecutionRevision,
    ) -> Result<(), Failure> {
        if revision.installation_id() != &submission.binding.installation_id
            || revision.tenant_id() != &submission.tenant
            || revision.revision_id() != &submission.binding.execution_revision
        {
            return Err(self.unavailable());
        }
        crate::tenancy::configuration::validate_profile(revision.document())
            .map_err(Failure::from)?;
        self.with_dispatch_authority(submission, |state| {
            if revision.authority_revision() > state.revision {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            Ok(())
        })
        .map_err(Failure::from)
    }

    #[cfg(all(test, unix))]
    pub(in crate::tenancy) async fn warm_retained_execution_for_test(
        &self,
        submission: &Submission,
    ) -> Result<(), Failure> {
        drop(self.retained_execution(submission).await?);
        Ok(())
    }

    pub(super) async fn retained_execution(
        &self,
        submission: &Submission,
    ) -> Result<Pin<RetainedExecutionRevision>, Failure> {
        let authority_revision = self
            .with_dispatch_authority(submission, |state| Ok(u64::from(state.revision)))
            .map_err(Failure::from)?;
        // The stored envelope is immutable by contract, but a database replacement
        // must never select a different view only when the cache happens to be cold.
        // Read bounded facts before lookup; cache hits avoid authenticated decoding,
        // compilation and heap traversal, not the current-content integrity fence.
        let _read = self.read_permit()?;
        let retained = tokio::time::timeout_at(
            self.read_deadline(submission),
            configuration_store::configuration(
                &self.db,
                &submission.tenant,
                &submission.binding.execution_revision,
            ),
        )
        .await
        .map_err(|_| self.read_timeout(submission))?
        .map_err(|_| self.unavailable())?
        .ok_or_else(|| Failure::from(ErrorCode::NotFound))?;
        let key = ExecutionKey::Dispatch {
            authority_revision,
            tenant: submission.tenant.as_str().into(),
            revision: submission.binding.execution_revision.as_str().into(),
            binding: configuration_store::execution_cache_binding(&retained)
                .map_err(|_| self.unavailable())?,
        };
        if let Some(revision) = self
            .caches
            .executions
            .get(&key)
            .map_err(|error| self.cache_failure(error))?
        {
            self.check_cached_revision(submission, &revision)?;
            return Ok(revision);
        }
        let revision = self
            .resources
            .open_execution(&retained)
            .map_err(|code| match code {
                // Authenticated history can be readable but unsupported for
                // this execution. A rejected request must not fence storage.
                ErrorCode::InvalidRequest | ErrorCode::CapabilityMismatch => code.into(),
                _ => self.unavailable(),
            })?;
        self.check_cached_revision(submission, &revision)?;
        let heap = revision.retained_heap_bytes().map_err(Failure::from)?;
        let key_heap = key.heap_bytes();
        // Recheck time/authority after synchronous decode and weight traversal.
        // Shutdown takes the corresponding write lock before closing caches.
        self.with_dispatch_authority(submission, |_| {
            self.caches
                .executions
                .insert(key, revision, heap, key_heap)
                .map_err(|error| self.cache_failure(error).0.code)
        })
        .map_err(Failure::from)
    }

    pub(super) async fn retained_credential(
        &self,
        submission: &Submission,
        reference: &ResourceRef,
    ) -> Result<Credential, Failure> {
        let authority_revision = self
            .with_dispatch_authority(submission, |state| {
                authorize_route(state, submission, std::slice::from_ref(reference))?;
                Ok(u64::from(state.revision))
            })
            .map_err(Failure::from)?;
        let key = CredentialKey {
            authority_revision,
            tenant: submission.tenant.as_str().into(),
            owner_kind: reference.owner_kind,
            owner: reference.owner_id.as_str().into(),
            kind: reference.resource_kind,
            resource: reference.resource_id.as_str().into(),
            revision: reference.revision_id.as_str().into(),
        };
        let credential = match self
            .caches
            .credentials
            .get(&key)
            .map_err(|error| self.cache_failure(error))?
        {
            Some(credential) => credential,
            None => {
                let _read = self.read_permit()?;
                if let Some(credential) = self
                    .caches
                    .credentials
                    .get(&key)
                    .map_err(|error| self.cache_failure(error))?
                {
                    credential
                } else {
                    let value = tokio::time::timeout_at(
                        self.read_deadline(submission),
                        self.resources.credential(&self.db, reference),
                    )
                    .await
                    .map_err(|_| self.read_timeout(submission))?
                    .map_err(|_| self.unavailable())?;
                    let Credential::ApiKey { value } = value else {
                        return Err(self.unavailable());
                    };
                    let credential = StaticCredential {
                        secret: value.into_boxed_str(),
                    };
                    let heap = credential.secret.len();
                    let key_heap = key.heap_bytes();
                    self.with_dispatch_authority(submission, |state| {
                        authorize_route(state, submission, std::slice::from_ref(reference))?;
                        self.caches
                            .credentials
                            .insert(key, credential, heap, key_heap)
                            .map_err(|error| self.cache_failure(error).0.code)
                    })
                    .map_err(Failure::from)?
                }
            }
        };
        self.with_dispatch_authority(submission, |state| {
            authorize_route(state, submission, std::slice::from_ref(reference))
        })
        .map_err(Failure::from)?;
        // The short-lived outbound copy belongs to the admitted execution, not
        // the immutable cache. Provider requests never receive a cache handle.
        Ok(Credential::ApiKey {
            value: credential.secret.to_string(),
        })
    }
}
