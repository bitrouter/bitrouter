//! Service-local immutable bytes and views, never cached execution permission.
//!
//! Installation, process instance and wrapping-key/compiler epoch are implicit
//! in the owning Service: none can be replaced during its lifetime. Every key
//! additionally names the tenant, exact resource revision and published authority
//! revision. An old miss can populate only its old namespace after revalidation.

use std::sync::atomic::{AtomicBool, Ordering};

use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    operation::ErrorCode,
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

const EXECUTION_ENTRIES: usize = 64;
const EXECUTION_BYTES: usize = 128 * 1024 * 1024;
const CREDENTIAL_ENTRIES: usize = 256;
const CREDENTIAL_BYTES: usize = 8 * 1024 * 1024;

pub(in crate::tenancy) struct Caches {
    executions: Cache<ExecutionKey, RetainedExecutionRevision>,
    credentials: Cache<CredentialKey, StaticCredential>,
    closed: AtomicBool,
}

impl Caches {
    pub(in crate::tenancy) fn new() -> anyhow::Result<Self> {
        Self::bounded(
            EXECUTION_ENTRIES,
            EXECUTION_BYTES,
            CREDENTIAL_ENTRIES,
            CREDENTIAL_BYTES,
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
struct ExecutionKey {
    authority_revision: u64,
    tenant: Box<str>,
    revision: Box<str>,
}

impl ExecutionKey {
    fn heap_bytes(&self) -> usize {
        // Validated identities have finite wire bounds; Box<str> has no spare
        // capacity. Inline key storage is charged by the cache's entry vector.
        self.tenant.len() + self.revision.len()
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

    pub(super) async fn retained_execution(
        &self,
        submission: &Submission,
    ) -> Result<Pin<RetainedExecutionRevision>, Failure> {
        let authority_revision = self
            .with_dispatch_authority(submission, |state| Ok(u64::from(state.revision)))
            .map_err(Failure::from)?;
        let key = ExecutionKey {
            authority_revision,
            tenant: submission.tenant.as_str().into(),
            revision: submission.binding.execution_revision.as_str().into(),
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
        // Eight fail-fast read slots bound duplicate misses and transient decode
        // work. A cache miss never creates another semaphore or per-key queue.
        let _read = self.read_permit()?;
        if let Some(revision) = self
            .caches
            .executions
            .get(&key)
            .map_err(|error| self.cache_failure(error))?
        {
            self.check_cached_revision(submission, &revision)?;
            return Ok(revision);
        }
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
        let revision = self
            .resources
            .open_execution(&retained)
            .map_err(|_| self.unavailable())?;
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
