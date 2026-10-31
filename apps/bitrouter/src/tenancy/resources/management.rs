//! Protected dynamic provisioning through the existing authority transaction.

#[cfg(test)]
mod tests;

use bitrouter_ai::tenancy::{
    authority::{GrantScope, OwnerKind, Permission, ResourceKey, ResourceKind},
    configuration::ResourceRef,
    identity::{RevisionId, TenantId},
    operation::ErrorCode,
    resources::{Definition as PublicDefinition, ResourceCapabilities, ResourceStatus},
};
use chrono::{DateTime, Utc};
use sea_orm::ConnectionTrait;

use super::{
    Resources, compiler, lifecycle,
    manifest::{Artifact, CredentialBinding, Definition, Import},
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
        if !matches!(command, Command::Resource(..)) {
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
        input: &bitrouter_ai::tenancy::resources::ResourcePut,
    ) -> Result<StoredResource> {
        if self.key.is_none() {
            return Err(ErrorCode::CapabilityMismatch);
        }
        if storage::references(db)
            .await
            .map_err(|_| ErrorCode::StorageUnavailable)?
            .len()
            >= super::MAX_CLOSURE
        {
            return Err(ErrorCode::CapacityExceeded);
        }
        let revision = uuid::Uuid::new_v4()
            .to_string()
            .try_into()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let definition = match &input.definition {
            PublicDefinition::Credential { secret } => Definition::Credential {
                credential: crate::gateway::providers::auth::credentials::Credential::ApiKey {
                    value: secret.clone(),
                },
                oauth_generation: 0,
                oauth: None,
            },
            PublicDefinition::Route {
                routing,
                selector,
                policy,
                credentials,
            } => Definition::Route {
                routing: routing.clone(),
                selector: selector.clone(),
                policy: policy.clone(),
                credentials: credentials
                    .iter()
                    .map(|binding| CredentialBinding {
                        provider: binding.provider.clone(),
                        account: binding.account.clone(),
                        credential: binding.credential.clone(),
                    })
                    .collect(),
                exploration: Vec::new(),
                core_choice: None,
                assessment: None,
                block: None,
            },
            PublicDefinition::Policy {
                document,
                name,
                dependencies,
            } => Definition::Policy {
                document: serde_json::from_value(document.clone())
                    .map_err(|_| ErrorCode::InvalidRequest)?,
                name: name.clone(),
                dependencies: dependencies.clone(),
            },
        };
        let import = Import {
            reference: reference(&input.key, revision),
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
            if state.resource_lifecycle.revoked(&artifact.reference) {
                return Err(ErrorCode::Forbidden);
            }
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
        let envelope = if matches!(import.definition, Definition::Route { .. }) {
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
    match command {
        Command::Resource(tenant, input) => {
            authorize(
                state,
                caller,
                tenant,
                &input.key,
                permission(input.key.resource_kind, false),
                now,
            )?;
            // Authorized CAS rejection belongs to the durable operation path.
            // Do not spend compilation/crypto work on a known stale head, but
            // still let apply retain its exact keyed digest and scoped outcome.
            if state.resource_lifecycle.head(&input.key) != input.expected_revision.as_ref() {
                return Ok((context(caller, tenant, &input.key), None));
            }
            let record = resources
                .prepare_resource(db, state, caller, tenant, input)
                .await?;
            Ok((context(caller, tenant, &input.key), Some(record)))
        }
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
    let record = match command {
        Command::Resource(tenant, input) => {
            authorize(
                state,
                caller,
                tenant,
                &input.key,
                permission(input.key.resource_kind, false),
                now,
            )?;
            if state.resource_lifecycle.head(&input.key) != input.expected_revision.as_ref() {
                return Err(ErrorCode::RevisionConflict);
            }
            let retained = prepared
                .resource
                .as_ref()
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            if let Some(previous) = state.resource_lifecycle.head(&input.key) {
                let previous = reference(&input.key, previous.clone());
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
            lifecycle::Record {
                reference: retained.reference.clone(),
                is_head: true,
                revoked: false,
                authority_revision,
            }
        }
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
    };
    if state.resource_lifecycle.record(&record.reference).is_none()
        && state.resource_lifecycle.len() >= super::MAX_CLOSURE
    {
        return Err(ErrorCode::CapacityExceeded);
    }
    let resource = record.reference.resource_id.clone();
    let revision = record.reference.revision_id.clone();
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
    pub(in crate::tenancy) fn resource_capabilities(
        &self,
        caller: &Caller,
    ) -> std::result::Result<ResourceCapabilities, Failure> {
        let capabilities = self.capabilities(caller)?;
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
        let _permit = self.read_permit()?;
        let before = self.current()?;
        authorize(
            &before,
            caller,
            tenant,
            &reference.key(),
            Permission::ResourceRead,
            Utc::now(),
        )
        .map_err(Failure::from)?;
        // Authenticate encrypted content without returning it; status is not a
        // secret retrieval endpoint and absent/foreign references are not listed.
        tokio::time::timeout(
            crate::tenancy::MANAGEMENT_TIMEOUT,
            self.resources.load(&self.db, reference),
        )
        .await
        .map_err(|_| self.unavailable())?
        .map_err(Failure::from)?;
        let state = self.current()?;
        authorize(
            &state,
            caller,
            tenant,
            &reference.key(),
            Permission::ResourceRead,
            Utc::now(),
        )
        .map_err(Failure::from)?;
        Ok(ResourceStatus {
            format: 1,
            installation_id: state.installation.clone(),
            reference: reference.clone(),
            authority_revision: state.revision,
            head_revision: state.resource_lifecycle.head(&reference.key()).cloned(),
            revoked: state.resource_lifecycle.revoked(reference),
        })
    }
}
