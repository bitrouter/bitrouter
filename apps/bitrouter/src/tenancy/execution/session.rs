//! Checked, lease-free authority for the in-process native harness.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use bitrouter_ai::core::protocol::{CoreError, ErrorCode as CoreCode, OwnershipGrant};
use bitrouter_ai::tenancy::{
    authority::{GrantScope, Permission},
    configuration::ResourceRef,
    identity::{InstallationId, PrincipalId, RevisionId, TenantId},
    operation::ErrorCode,
    validation::Validate,
};
use serde::{Deserialize, Serialize};

use super::{Service, TenantExecution};
use crate::tenancy::error::Failure;

/// Persisted bounds are an immutable upper envelope, never current entitlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Descriptor {
    pub installation: InstallationId,
    pub tenant: TenantId,
    pub principal: PrincipalId,
    pub name: String,
    pub revision: RevisionId,
    pub credential_scope: GrantScope,
    pub binding_scope: GrantScope,
    pub harness: String,
    pub session: String,
}

/// Its only constructor authenticates with the same Service that owns the DB.
pub(crate) struct Authority {
    host: Arc<super::host::Authority>,
    proposed: Descriptor,
    required: Vec<ResourceRef>,
    choices: Vec<crate::tenancy::resources::execution::ChoiceProfile>,
    recovery_profile: bool,
    experiments: Vec<String>,
}

/// Completed from authenticated lookup and a validated immutable row descriptor.
pub(crate) struct Binding {
    authority: Arc<Authority>,
    descriptor: Descriptor,
    grant: OwnershipGrant,
    closing: Arc<AtomicBool>,
    paid_source: Arc<Mutex<Option<Arc<crate::tenancy::experiment::store::completed::Edge>>>>,
    recovery: std::sync::OnceLock<Arc<crate::tenancy::session::recovery::CheckedRecovery>>,
}

impl Service {
    pub(crate) async fn session_authority(
        &self,
        headers: &http::HeaderMap,
        name: &str,
    ) -> Result<Arc<Authority>, Failure> {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let host = self.host_authority(headers)?;
        // Authenticate and resolve retained R1 before any harness storage or path I/O.
        let retained = host.retained().await?;
        // The opener has no model input yet. This fixed-R1 profile therefore
        // approves its complete native route closure before any journal read.
        // No retained cache root or execution capacity lives with the session.
        let mut recovery_profile = cfg!(unix)
            && retained.document().learning.mode
                == bitrouter_ai::tenancy::configuration::LearningMode::Off;
        let mut required = Vec::new();
        let mut choices = Vec::new();
        let mut experiments = Vec::new();
        for reference in &retained.document().routes {
            let route = retained
                .select_native(
                    reference.resource_id.as_str(),
                    bitrouter_ai::types::ModelOperation::Generation,
                )
                .map_err(Failure::from)?;
            recovery_profile &= route.stopped_recovery_profile();
            if let Some(profile) = route.choice_profile() {
                choices.push(profile);
            }
            if route.policy().is_some_and(|policy| {
                policy
                    .document()
                    .policies
                    .get(policy.name())
                    .and_then(|definition| definition.optimization.as_ref())
                    .is_some_and(|optimization| optimization.active.is_some())
            }) {
                experiments.push(reference.resource_id.to_string());
            }
            for dependency in route.dependencies() {
                if !required.contains(dependency) {
                    if required.len() == 4096 {
                        return Err(ErrorCode::CapacityExceeded.into());
                    }
                    required.push(dependency.clone());
                }
            }
        }
        host.with_authority(&required, |effective| {
            if !effective.permits(Permission::CredentialUse) {
                return Err(ErrorCode::Forbidden);
            }
            Ok(())
        })
        .map_err(Failure::from)?;
        drop(retained);
        let (installation, tenant, principal) = host.identity();
        let (credential_scope, binding_scope) = host.scopes();
        let proposed = Descriptor {
            installation: installation.clone(),
            tenant: tenant.clone(),
            principal: principal.clone(),
            name: name.to_owned(),
            revision: host.revision().clone(),
            credential_scope: credential_scope.clone(),
            binding_scope: binding_scope.clone(),
            harness: format!("harness_{}", uuid::Uuid::new_v4()),
            session: format!("session_{}", uuid::Uuid::new_v4()),
        };
        Ok(Arc::new(Authority {
            host,
            proposed,
            required,
            choices,
            recovery_profile,
            experiments,
        }))
    }
}

impl Authority {
    pub(crate) fn startup_stamp(&self) -> Result<Option<crate::tenancy::origin::Stamp>, String> {
        self.service()
            .startup
            .as_ref()
            .map(|origin| {
                origin
                    .stamp(&self.service().cleanup)
                    .map_err(|_| "native origin unavailable".to_owned())
            })
            .transpose()
    }

    pub(in crate::tenancy) fn dependencies(&self) -> &[ResourceRef] {
        &self.required
    }
    pub(crate) fn recovery_profile_digest(&self) -> Result<String, String> {
        if !self.recovery_profile {
            return Err("unsupported native recovery profile".into());
        }
        let bytes = serde_json::to_vec(&(
            "choice_http_no_local_effects_v1",
            &self.proposed.installation,
            &self.proposed.tenant,
            &self.proposed.revision,
            &self.required,
        ))
        .map_err(|_| "native recovery profile encoding".to_owned())?;
        Ok(crate::storage::checkpoint::sha256(&bytes))
    }
    pub(crate) fn recovery_profile(&self) -> bool {
        self.recovery_profile
    }

    pub(crate) fn experimental_model(&self, model: &str) -> bool {
        self.experiments.iter().any(|candidate| candidate == model)
    }
    pub(crate) fn has_choices(&self) -> bool {
        !self.choices.is_empty()
    }
    pub(crate) fn accepts_choice(
        &self,
        profile: &crate::tenancy::resources::execution::ChoiceProfile,
    ) -> bool {
        self.choices.contains(profile)
    }
    pub(crate) fn caller(&self) -> crate::gateway::caller::CallerContext {
        self.host.caller()
    }
    pub(crate) fn database(&self) -> &sea_orm::DatabaseConnection {
        &self.service().db
    }
    pub(crate) fn service(&self) -> &Service {
        self.host.service()
    }
    pub(crate) fn host(&self) -> &super::host::Authority {
        &self.host
    }
    pub(crate) fn descriptor(&self) -> &Descriptor {
        &self.proposed
    }
    pub(crate) fn storage_identity(&self) -> (&InstallationId, &TenantId, &PrincipalId) {
        self.host.identity()
    }
    pub(crate) fn check(&self) -> Result<(), Failure> {
        self.with_authority(|| ()).map_err(Failure::from)
    }
    pub(crate) fn with_authority<T>(&self, work: impl FnOnce() -> T) -> Result<T, ErrorCode> {
        self.host.with_authority(&self.required, |effective| {
            if !effective.permits(Permission::CredentialUse) {
                return Err(ErrorCode::Forbidden);
            }
            Ok(work())
        })
    }
    pub(in crate::tenancy) fn origin(
        &self,
        proof: &Arc<crate::tenancy::transaction::Proof>,
    ) -> bool {
        self.host.origin(proof)
    }
    pub(crate) fn matches(&self, descriptor: &Descriptor) -> bool {
        descriptor.installation == self.proposed.installation
            && descriptor.tenant == self.proposed.tenant
            && descriptor.principal == self.proposed.principal
            && descriptor.name == self.proposed.name
            && descriptor.revision == self.proposed.revision
    }
    pub(crate) fn validate_descriptor(&self, descriptor: &Descriptor) -> Result<(), Failure> {
        descriptor
            .credential_scope
            .validate()
            .map_err(|_| Failure::from(ErrorCode::Forbidden))?;
        descriptor
            .binding_scope
            .validate()
            .map_err(|_| Failure::from(ErrorCode::Forbidden))?;
        if !self.matches(descriptor)
            || self
                .required
                .iter()
                .any(|reference| !approved(descriptor, reference))
        {
            return Err(ErrorCode::Forbidden.into());
        }
        Ok(())
    }
    pub(crate) fn bind(
        self: &Arc<Self>,
        descriptor: Descriptor,
        grant: OwnershipGrant,
    ) -> Result<Arc<Binding>, CoreError> {
        self.check().map_err(core_error)?;
        self.validate_descriptor(&descriptor).map_err(core_error)?;
        grant.validate()?;
        if !self.matches(&descriptor)
            || grant.session_id != descriptor.session
            || grant.harness_id != descriptor.harness
        {
            return Err(CoreError::rejected(
                CoreCode::UnauthorizedScope,
                "native session binding mismatch",
            ));
        }
        Ok(Arc::new(Binding {
            authority: self.clone(),
            descriptor,
            grant,
            closing: Arc::new(AtomicBool::new(false)),
            paid_source: Arc::new(Mutex::new(None)),
            recovery: Default::default(),
        }))
    }
}

impl Binding {
    pub(crate) fn begin_recovery(
        &self,
        checked: Arc<crate::tenancy::session::recovery::CheckedRecovery>,
    ) -> Result<(), CoreError> {
        self.check()?;
        if !checked.binds(&self.descriptor) {
            return Err(core_error(ErrorCode::Forbidden.into()));
        }
        self.recovery
            .set(checked)
            .map_err(|_| core_error(ErrorCode::Forbidden.into()))
    }
    pub(crate) fn recovery(
        &self,
    ) -> Option<&Arc<crate::tenancy::session::recovery::CheckedRecovery>> {
        self.recovery.get()
    }
    pub(crate) fn check_restore(&self) -> Result<(), CoreError> {
        if let Some(checked) = self.recovery.get() {
            if !checked.binds(&self.descriptor) || self.closing.load(Ordering::Acquire) {
                return Err(core_error(ErrorCode::Forbidden.into()));
            }
            self.authority.check().map_err(core_error)
        } else {
            self.check()
        }
    }

    pub(crate) fn step(
        self: &Arc<Self>,
        origin: Arc<crate::harness::resources::managed::store::model::Origin>,
    ) -> Result<Arc<StepBinding>, CoreError> {
        if !Arc::ptr_eq(self, origin.binding()) {
            return Err(CoreError::rejected(
                CoreCode::UnauthorizedScope,
                "native step owner changed",
            ));
        }
        Ok(Arc::new(StepBinding {
            binding: self.clone(),
            origin,
            parent: Default::default(),
        }))
    }
    pub(crate) fn authority(&self) -> &Arc<Authority> {
        &self.authority
    }
    pub(crate) fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    pub(crate) fn check(&self) -> Result<(), CoreError> {
        self.with_dispatch(|| ())
    }
    pub(crate) fn with_dispatch<T>(&self, work: impl FnOnce() -> T) -> Result<T, CoreError> {
        self.authority
            .with_authority(|| {
                if self.closing.load(Ordering::Acquire)
                    || self
                        .recovery
                        .get()
                        .is_some_and(|checked| !checked.retired())
                {
                    Err(CoreError::rejected(
                        CoreCode::CheckpointUnavailable,
                        "native owner is closing",
                    ))
                } else {
                    Ok(work())
                }
            })
            .map_err(|code| core_error(code.into()))?
    }
    pub(crate) fn close_new_work(&self) -> Result<(), CoreError> {
        let _guard = self
            .authority
            .service()
            .snapshot
            .write()
            .map_err(|_| core_error(ErrorCode::AuthorityUnavailable.into()))?;
        self.closing.store(true, Ordering::Release);
        Ok(())
    }
    pub(in crate::tenancy) fn assessment_check(&self) -> Result<(), ErrorCode> {
        self.check().map_err(|_| ErrorCode::Forbidden)?;
        self.authority
            .host
            .with_authority(&self.authority.required, |effective| {
                if !effective.permits(Permission::EvaluationSubmit)
                    || !effective.permits(Permission::ResourceRead)
                    || !effective.covers(&GrantScope::Tenant {
                        operator_pools: Vec::new(),
                    })
                {
                    return Err(ErrorCode::Forbidden);
                }
                Ok(())
            })
    }
    pub(in crate::tenancy) fn assessment_in(
        &self,
        state: &crate::tenancy::model::Snapshot,
    ) -> Result<(), ErrorCode> {
        if self.closing.load(Ordering::Acquire)
            || self.authority.service().host_stop().is_cancelled()
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        self.authority
            .host
            .in_snapshot(state, &self.authority.required, |effective| {
                if !effective.permits(Permission::EvaluationSubmit)
                    || !effective.permits(Permission::ResourceRead)
                    || !effective.permits(Permission::CredentialUse)
                    || !effective.covers(&GrantScope::Tenant {
                        operator_pools: Vec::new(),
                    })
                {
                    return Err(ErrorCode::Forbidden);
                }
                Ok(())
            })
    }

    pub(crate) fn ownership_grant(&self) -> &OwnershipGrant {
        &self.grant
    }
    pub(crate) fn grant(&self, grant: &OwnershipGrant) -> Result<(), CoreError> {
        if &self.grant != grant {
            return Err(CoreError::rejected(
                CoreCode::UnauthorizedScope,
                "native grant changed",
            ));
        }
        Ok(())
    }
    pub(crate) fn belongs_to(&self, service: &Service) -> bool {
        self.authority.origin(&service.cleanup)
    }
    pub(crate) async fn admit(&self, model: &str) -> Result<Arc<TenantExecution>, Failure> {
        let descriptor = &self.descriptor;
        let mut submission = self.authority.host.submission();
        submission.session_closed = Some(self.closing.clone());
        self.authority
            .service()
            .admit_selected_execution(submission, false, |revision| {
                let route = revision
                    .select_native(model, bitrouter_ai::types::ModelOperation::Generation)?;
                for reference in route.dependencies() {
                    if !approved(descriptor, reference) {
                        return Err(ErrorCode::Forbidden);
                    }
                }
                Ok(route)
            })
            .await
    }
}

fn approved(descriptor: &Descriptor, reference: &ResourceRef) -> bool {
    let key = reference.key();
    crate::tenancy::authority::covers_key(&descriptor.credential_scope, &descriptor.tenant, &key)
        && crate::tenancy::authority::covers_key(
            &descriptor.binding_scope,
            &descriptor.tenant,
            &key,
        )
}

pub(crate) fn core_error(error: Failure) -> CoreError {
    let code = match error.0.code {
        ErrorCode::CapacityExceeded => CoreCode::LimitExceeded,
        ErrorCode::AuthorityUnavailable | ErrorCode::StorageUnavailable => {
            CoreCode::CheckpointUnavailable
        }
        _ => CoreCode::UnauthorizedScope,
    };
    CoreError::rejected(code, "authenticated native session is unavailable")
}

/// One actual Core model step. Idle sessions retain no execution or cache pin.
pub(crate) struct StepBinding {
    binding: Arc<Binding>,
    origin: Arc<crate::harness::resources::managed::store::model::Origin>,
    parent: std::sync::OnceLock<(
        std::sync::Weak<TenantExecution>,
        Arc<crate::gateway::pipeline::owned_tasks::OwnedTasks>,
    )>,
}
impl StepBinding {
    pub(in crate::tenancy) fn origin(
        &self,
    ) -> &Arc<crate::harness::resources::managed::store::model::Origin> {
        &self.origin
    }
    pub(super) fn binding(&self) -> &Binding {
        &self.binding
    }
    pub(super) fn attach_parent(
        &self,
        parent: &Arc<TenantExecution>,
        tasks: Arc<crate::gateway::pipeline::owned_tasks::OwnedTasks>,
    ) -> Result<(), Failure> {
        let descriptor = self.binding.descriptor();
        if !self.binding.belongs_to(&parent.service)
            || parent.submission.tenant != descriptor.tenant
            || parent.submission.principal != descriptor.principal
            || parent.submission.binding.execution_revision != descriptor.revision
            || parent.choice.is_some()
        {
            return Err(ErrorCode::Forbidden.into());
        }
        if parent
            .route
            .choice_profile()
            .as_ref()
            .is_some_and(|profile| !self.binding.authority.accepts_choice(profile))
        {
            return Err(ErrorCode::Forbidden.into());
        }
        self.parent
            .set((Arc::downgrade(parent), tasks))
            .map_err(|_| Failure::from(ErrorCode::Forbidden))
    }
    pub(super) fn parent(&self) -> Result<Arc<TenantExecution>, Failure> {
        self.parent
            .get()
            .and_then(|(parent, _)| parent.upgrade())
            .ok_or_else(|| ErrorCode::AuthorityUnavailable.into())
    }
    pub(crate) fn tasks(
        &self,
    ) -> Result<Arc<crate::gateway::pipeline::owned_tasks::OwnedTasks>, Failure> {
        self.parent
            .get()
            .map(|(_, tasks)| tasks.clone())
            .ok_or_else(|| ErrorCode::AuthorityUnavailable.into())
    }
    #[cfg(all(test, unix))]
    pub(crate) async fn choice_final_gate_for_test(
        &self,
        child: &bitrouter_ai::tenancy::identity::ExecutionId,
    ) -> anyhow::Result<()> {
        let parent = self
            .parent()
            .map_err(|_| anyhow::anyhow!("Choice parent unavailable"))?;
        parent.service.choice_final_gate_for_test(child).await
    }

    pub(crate) fn cleanup_deadline(&self) -> Result<tokio::time::Instant, Failure> {
        Ok(self
            .parent()?
            .receipt_owner()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
            .cleanup_deadline())
    }
    pub(crate) fn choice_profile(
        &self,
    ) -> Result<Option<crate::tenancy::resources::execution::ChoiceProfile>, Failure> {
        let parent = self.parent()?;
        parent.revalidate().map_err(Failure::from)?;
        Ok(parent.route.choice_profile())
    }
    pub(crate) fn try_admit_choice(&self) -> Result<super::choice::AdmittedChoice, Failure> {
        let parent = self.parent()?;
        let route = parent.route.choice_route().map_err(Failure::from)?;
        for reference in route.dependencies() {
            if !approved(self.binding.descriptor(), reference) {
                return Err(ErrorCode::Forbidden.into());
            }
        }
        super::choice::AdmittedChoice::admit(&parent, route)
    }
}

pub(in crate::tenancy) struct PaidSourcePin {
    holder: Arc<Mutex<Option<Arc<crate::tenancy::experiment::store::completed::Edge>>>>,
    source: Arc<crate::tenancy::experiment::store::completed::Edge>,
}
impl Drop for PaidSourcePin {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.holder.lock()
            && slot
                .as_ref()
                .is_some_and(|value| Arc::ptr_eq(value, &self.source))
        {
            *slot = None;
        }
    }
}
impl Binding {
    pub(in crate::tenancy) fn pin_paid_source(
        &self,
        edge: crate::tenancy::experiment::store::completed::Edge,
    ) -> Result<PaidSourcePin, ErrorCode> {
        let expected = crate::storage::scope::Scope::for_session(self.authority())
            .0
            .physical_key(
                crate::storage::scope::KeyDomain::NativeSession,
                "",
                &[
                    self.descriptor().principal.as_str(),
                    &self.descriptor().name,
                ],
            )
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if edge.session != expected || edge.head.execution_epoch != self.grant.execution_epoch {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let mut slot = self
            .paid_source
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if slot.is_some() {
            return Err(ErrorCode::CapacityExceeded);
        }
        let source = Arc::new(edge);
        *slot = Some(source.clone());
        Ok(PaidSourcePin {
            holder: self.paid_source.clone(),
            source,
        })
    }
    pub(crate) fn has_paid_source(&self) -> Result<bool, ErrorCode> {
        self.paid_source
            .lock()
            .map(|slot| slot.is_some())
            .map_err(|_| ErrorCode::AuthorityUnavailable)
    }
    pub(in crate::tenancy) fn check_paid_journal(
        &self,
        journal: &[u8],
        budget: &crate::storage::operation_budget::OperationBudget<'_>,
    ) -> anyhow::Result<()> {
        let source = self
            .paid_source
            .lock()
            .map_err(|_| anyhow::anyhow!("native source pin unavailable"))?
            .clone();
        let Some(source) = source else {
            return Ok(());
        };
        budget.charge(
            1,
            journal
                .len()
                .checked_mul(3)
                .and_then(|n| n.checked_add(8192))
                .ok_or(crate::storage::operation_budget::Error::Capacity)?,
        )?;
        let proposed =
            crate::harness::resources::managed::store::model::completed::CompletedRun::proposed(
                self, journal, budget,
            )?;
        anyhow::ensure!(
            proposed.run() == source.run
                && proposed.checkpoint() == source.checkpoint
                && proposed.packet().prefix_digest == source.prefix
                && proposed.head().execution_epoch == source.head.execution_epoch
                && proposed.head().state_revision >= source.head.state_revision
                && proposed.head().event_seq >= source.head.event_seq,
            crate::storage::operation_budget::Error::Rejected(ErrorCode::RevisionConflict)
        );
        Ok(())
    }
}
