//! Protected dynamic provisioning through the existing authority transaction.

#[cfg(test)]
mod tests;

mod put;

use bitrouter_ai::tenancy::{
    authority::{GrantScope, OwnerKind, Permission, ResourceKey, ResourceKind},
    configuration::ResourceRef,
    identity::{RevisionId, TenantId},
    operation::ErrorCode,
    resources::{ResourceCapabilities, ResourceStatus},
    resources_v2::ResourceCapabilitiesV2,
};
use chrono::{DateTime, Utc};
use sea_orm::ConnectionTrait;

use super::{
    Resources, compiler, lifecycle,
    manifest::{Artifact, Import},
    storage,
};
use crate::tenancy::{
    Service,
    command::Command,
    configuration,
    configuration_store::StoredResource,
    error::Failure,
    model::{Caller, ReadContext, Snapshot},
    mutation::{Applied, Prepared},
    store::Change,
};

type Result<T> = std::result::Result<T, ErrorCode>;

fn reference(key: &ResourceKey, revision: RevisionId) -> ResourceRef {
    ResourceRef {
        owner_kind: key.owner_kind,
        owner_id: key.owner_id.clone(),
        resource_kind: key.resource_kind,
        resource_id: key.resource_id.clone(),
        revision_id: revision,
    }
}

fn authorize(
    state: &Snapshot,
    caller: &Caller,
    tenant: &TenantId,
    key: &ResourceKey,
    permission: Permission,
    now: DateTime<Utc>,
) -> Result<()> {
    if key.owner_kind == OwnerKind::OperatorPool
        && permission != Permission::ResourceRead
        && !matches!(caller, Caller::Installation(_))
    {
        return Err(ErrorCode::Forbidden);
    }
    let scope = reference(
        key,
        "authorization"
            .try_into()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?,
    );
    configuration::authorize(state, caller, tenant, permission, &[scope], now)
}

fn permission(kind: ResourceKind, revoke: bool) -> Permission {
    if kind == ResourceKind::Credential {
        Permission::CredentialManage
    } else if revoke {
        Permission::PolicyActivate
    } else {
        Permission::ResourceDraft
    }
}

fn context(caller: &Caller, tenant: &TenantId, key: &ResourceKey) -> ReadContext {
    if matches!(caller, Caller::Installation(_)) {
        ReadContext::Installation
    } else {
        ReadContext::Tenant {
            tenant: tenant.clone(),
            permission: Permission::ResourceRead,
            scopes: vec![GrantScope::Resources {
                resources: vec![key.clone()],
            }],
        }
    }
}

impl Resources {
    pub(in crate::tenancy) fn management_digest(
        &self,
        command: &Command,
        digest: &str,
    ) -> Result<String> {
        if !matches!(
            command,
            Command::Resource(..) | Command::ResourceV2(..) | Command::ResourceV3(..)
        ) {
            return Ok(digest.to_owned());
        }
        self.key
            .as_ref()
            .ok_or(ErrorCode::CapabilityMismatch)?
            .fingerprint(
                "tenancy-resource-command-v1",
                "operation",
                digest.as_bytes(),
            )
            .map_err(|_| ErrorCode::AuthorityUnavailable)
    }

    async fn prepare_resource(
        &self,
        db: &impl ConnectionTrait,
        state: &Snapshot,
        caller: &Caller,
        tenant: &TenantId,
        input: put::Put<'_>,
    ) -> Result<StoredResource> {
        if self.key.is_none() {
            return Err(ErrorCode::CapabilityMismatch);
        }
        let revision = uuid::Uuid::new_v4()
            .to_string()
            .try_into()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let definition = input.definition()?;
        let import = Import {
            reference: reference(input.key, revision),
            definition,
        };
        import.validate().map_err(|_| ErrorCode::InvalidRequest)?;
        let dependencies = import.definition.dependencies();
        for dependency in &dependencies {
            authorize(
                state,
                caller,
                tenant,
                &dependency.key(),
                Permission::ResourceRead,
                Utc::now(),
            )?;
        }
        let closure = self.closure(db, &dependencies).await?;
        for artifact in &closure {
            authorize(
                state,
                caller,
                tenant,
                &artifact.reference.key(),
                Permission::ResourceRead,
                Utc::now(),
            )?;
            super::lifecycle::check_in(
                db,
                &state.installation,
                state.revision,
                &artifact.reference,
            )
            .await?;
            if artifact.reference.resource_kind == ResourceKind::Credential {
                configuration::authorize(
                    state,
                    caller,
                    tenant,
                    Permission::CredentialUse,
                    std::slice::from_ref(&artifact.reference),
                    Utc::now(),
                )?;
            }
        }
        let envelope = if import.definition.is_route() {
            let policy = import.definition.policy_reference().and_then(|reference| {
                closure
                    .iter()
                    .find(|artifact| &artifact.reference == reference)
            });
            let envelope =
                compiler::prepare_route(&import, policy).map_err(|_| ErrorCode::InvalidRequest)?;
            self.authorize_endpoints(caller, &envelope)?;
            Some(envelope)
        } else {
            None
        };
        let artifact = Artifact {
            reference: import.reference,
            definition: import.definition,
            envelope,
        };
        if artifact.definition.is_profiled() {
            let mut complete = closure;
            complete.push(artifact.clone());
            super::admission::validate_graph(&complete).map_err(|_| ErrorCode::InvalidRequest)?;
        }
        storage::seal(self, &artifact).map_err(|_| ErrorCode::InvalidRequest)
    }

    fn authorize_endpoints(
        &self,
        caller: &Caller,
        envelope: &compiler::RouteEnvelope,
    ) -> Result<()> {
        for target in envelope.chains.iter().flat_map(|chain| &chain.targets) {
            let raw = target
                .target
                .get("api_base")
                .and_then(serde_json::Value::as_str)
                .ok_or(ErrorCode::InvalidRequest)?;
            let url = url::Url::parse(raw).map_err(|_| ErrorCode::InvalidRequest)?;
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(ErrorCode::InvalidRequest);
            }
            if matches!(caller, Caller::Installation(_)) {
                continue;
            }
            // Origins are operator-controlled network authority. DNS and routing
            // for an approved origin must remain under the operator's control.
            // https://url.spec.whatwg.org/#origin
            if !self
                .approved_origins
                .iter()
                .any(|origin| origin.trim_end_matches('/') == url.origin().ascii_serialization())
            {
                return Err(ErrorCode::Forbidden);
            }
            // A selected origin cannot be overridden with a tenant-supplied Host,
            // credential, forwarding or hop-by-hop header.
            if let Some(headers) = target
                .target
                .get("headers")
                .and_then(serde_json::Value::as_array)
            {
                for header in headers {
                    let name = header
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .ok_or(ErrorCode::InvalidRequest)?;
                    if !matches!(
                        name.to_ascii_lowercase().as_str(),
                        "anthropic-version" | "anthropic-beta" | "openai-beta" | "accept"
                    ) {
                        return Err(ErrorCode::Forbidden);
                    }
                }
            }
        }
        Ok(())
    }
}

pub(in crate::tenancy) async fn prepare(
    db: &impl ConnectionTrait,
    resources: &Resources,
    state: &Snapshot,
    caller: &Caller,
    command: &Command,
    now: DateTime<Utc>,
) -> Result<(ReadContext, Option<StoredResource>)> {
    if let Some((tenant, input)) = put::input(command) {
        input.authorize(state, caller)?;
        authorize(
            state,
            caller,
            tenant,
            input.key,
            permission(input.key.resource_kind, false),
            now,
        )?;
        // Authorized CAS rejection belongs to the durable operation path.
        // Do not spend compilation/crypto work on a known stale head, but
        // still let apply retain its exact keyed digest and scoped outcome.
        if state.resource_lifecycle.head(input.key) != input.expected_revision.as_ref() {
            return Ok((context(caller, tenant, input.key), None));
        }
        if input.definition()?.is_admission_profile()
            && !state.resource_admission.has_profile_key(input.key)
            && super::storage::resource_key_exists(db, input.key)
                .await
                .map_err(|_| ErrorCode::StorageUnavailable)?
        {
            return Err(ErrorCode::InvalidRequest);
        }
        let record = resources
            .prepare_resource(db, state, caller, tenant, input)
            .await?;
        return Ok((context(caller, tenant, input.key), Some(record)));
    }
    match command {
        Command::RevokeResource(tenant, input) => {
            let key = input.reference.key();
            authorize(
                state,
                caller,
                tenant,
                &key,
                permission(key.resource_kind, true),
                now,
            )?;
            resources.load(db, &input.reference).await?;
            Ok((context(caller, tenant, &key), None))
        }
        _ => Err(ErrorCode::InvalidRequest),
    }
}

pub(in crate::tenancy) fn apply(
    state: &mut Snapshot,
    caller: &Caller,
    command: &Command,
    prepared: &Prepared,
    now: DateTime<Utc>,
) -> Result<Applied> {
    let authority_revision = state
        .revision
        .get()
        .checked_add(1)
        .and_then(|value| value.try_into().ok())
        .ok_or(ErrorCode::AuthorityUnavailable)?;
    let mut changes = Vec::new();
    let record = if let Some((tenant, input)) = put::input(command) {
        input.authorize(state, caller)?;
        authorize(
            state,
            caller,
            tenant,
            input.key,
            permission(input.key.resource_kind, false),
            now,
        )?;
        if state.resource_lifecycle.head(input.key) != input.expected_revision.as_ref() {
            return Err(ErrorCode::RevisionConflict);
        }
        let retained = prepared
            .resource
            .as_ref()
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        if let Some(previous) = state.resource_lifecycle.head(input.key) {
            let previous = reference(input.key, previous.clone());
            let mut old = state
                .resource_lifecycle
                .record(&previous)
                .cloned()
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            old.is_head = false;
            old.authority_revision = authority_revision;
            state.resource_lifecycle.insert(old.clone());
            changes.push(Change::ResourceLifecycle {
                installation: state.installation.clone(),
                record: old,
            });
        }
        changes.push(Change::Resource(retained.clone()));
        if input.affects_admission() {
            input.apply(
                std::sync::Arc::make_mut(&mut state.resource_admission),
                &retained.reference,
            );
        }
        lifecycle::Record {
            reference: retained.reference.clone(),
            is_head: true,
            revoked: false,
            authority_revision,
        }
    } else {
        match command {
            Command::RevokeResource(tenant, input) => {
                let key = input.reference.key();
                authorize(
                    state,
                    caller,
                    tenant,
                    &key,
                    permission(key.resource_kind, true),
                    now,
                )?;
                if state.revision != input.expected_authority_revision {
                    return Err(ErrorCode::RevisionConflict);
                }
                lifecycle::Record {
                    reference: input.reference.clone(),
                    is_head: state
                        .resource_lifecycle
                        .record(&input.reference)
                        .is_some_and(|r| r.is_head),
                    revoked: true,
                    authority_revision,
                }
            }
            _ => return Err(ErrorCode::InvalidRequest),
        }
    };
    let resource = record.reference.resource_id.clone();
    let revision = record.reference.revision_id.clone();
    if state.resource_admission.tracks(&record.reference) {
        let admission = std::sync::Arc::make_mut(&mut state.resource_admission);
        for change in &changes {
            if let Change::ResourceLifecycle { record, .. } = change
                && admission.tracks(&record.reference)
            {
                admission.apply_lifecycle(record);
            }
        }
        admission.apply_lifecycle(&record);
    }
    state.resource_lifecycle.insert(record.clone());
    changes.push(Change::ResourceLifecycle {
        installation: state.installation.clone(),
        record,
    });
    Ok(Applied {
        changes,
        resource,
        revision,
        issued: None,
        configuration: None,
    })
}

impl Service {
    pub(in crate::tenancy) async fn resource_capabilities_v3(
        &self,
        caller: &Caller,
    ) -> std::result::Result<bitrouter_ai::tenancy::resources_v3::ResourceCapabilitiesV3, Failure>
    {
        use bitrouter_ai::tenancy::{
            ordered::GenerationProtocol, resources_v3::ResourceCapabilitiesV3,
        };
        self.with_capability_authority(caller, |state| {
            Ok(ResourceCapabilitiesV3 {
                format: 3,
                installation_id: state.installation.clone(),
                provisioning_enabled: self.resources.key.is_some()
                    && self.private_adapter.is_some()
                    && self.ordered_dispatch_ready(state),
                max_command_bytes: 1048576,
                tenant_routes_require_approved_origins: true,
                generation_protocols: [
                    GenerationProtocol::ChatCompletions,
                    GenerationProtocol::Messages,
                    GenerationProtocol::Responses,
                    GenerationProtocol::GenerateContent,
                ],
            })
        })
        .await
    }

    pub(in crate::tenancy) async fn resource_capabilities_v2(
        &self,
        caller: &Caller,
    ) -> std::result::Result<ResourceCapabilitiesV2, Failure> {
        let previous = self.resource_capabilities(caller).await?;
        Ok(ResourceCapabilitiesV2 {
            format: 2,
            installation_id: previous.installation_id,
            provisioning_enabled: previous.provisioning_enabled,
            max_command_bytes: previous.max_command_bytes,
            tenant_routes_require_approved_origins: previous.tenant_routes_require_approved_origins,
            admission_profile_versions: [1, 2],
        })
    }

    pub(in crate::tenancy) async fn resource_capabilities(
        &self,
        caller: &Caller,
    ) -> std::result::Result<ResourceCapabilities, Failure> {
        let capabilities = self.capabilities(caller).await?;
        Ok(ResourceCapabilities {
            format: 1,
            installation_id: capabilities.installation_id,
            provisioning_enabled: self.resources.key.is_some(),
            max_command_bytes: 1024 * 1024,
            tenant_routes_require_approved_origins: true,
        })
    }

    pub(in crate::tenancy) async fn resource_status(
        &self,
        caller: &Caller,
        tenant: &TenantId,
        reference: &ResourceRef,
    ) -> std::result::Result<ResourceStatus, Failure> {
        let identity = self
            .authority_read(crate::tenancy::actor::AuthoritySubject::Tenant {
                caller: caller.clone(),
                tenant: tenant.clone(),
                active: false,
                resource: Some(reference.clone()),
            })
            .await?;
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(&identity.lease, |view| {
                authorize(
                    view.facts(),
                    caller,
                    tenant,
                    &reference.key(),
                    Permission::ResourceRead,
                    Utc::now(),
                )
            })
            .map_err(Failure::from)?;
        let _permit = self.read_permit()?;
        // Authenticate encrypted content without disclosing it.
        tokio::time::timeout(
            crate::tenancy::MANAGEMENT_TIMEOUT,
            self.resources.load(&self.db, reference),
        )
        .await
        .map_err(|_| self.unavailable())?
        .map_err(Failure::from)?;
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(&identity.lease, |view| {
                let state = view.facts();
                authorize(
                    state,
                    caller,
                    tenant,
                    &reference.key(),
                    Permission::ResourceRead,
                    Utc::now(),
                )?;
                // The exact revision's grant stays live, but a newly published head
                // may be outside this read set. Never report it under a newer stamp.
                if state.revision != identity.observed_revision {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                Ok(ResourceStatus {
                    format: 1,
                    installation_id: state.installation.clone(),
                    reference: reference.clone(),
                    authority_revision: state.revision,
                    head_revision: state.resource_lifecycle.head(&reference.key()).cloned(),
                    revoked: state.resource_lifecycle.revoked(reference),
                })
            })
            .map_err(Failure::from)
    }
}
