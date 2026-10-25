//! Typed, target-bound management commands and their versioned identity digest.

use bitrouter_ai::tenancy::{
    authority::{
        AuditReaderCreate, BindingPut, CredentialCreate, PrincipalPut, TenantPut, TenantState,
    },
    identity::{BindingId, CredentialId, InstallationId, OperationId, PrincipalId, TenantId},
    operation::Mutation,
    validation::{Validate, ValidationError},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(super) enum Command {
    Tenant(TenantId, TenantPut),
    Principal(PrincipalId, PrincipalPut),
    Binding(TenantId, BindingId, BindingPut),
    Issue(TenantId, CredentialCreate),
    AuditReader(TenantId, AuditReaderCreate),
    Detach(TenantId, BindingId, Mutation),
    Revoke(TenantId, CredentialId, Mutation),
}

impl Command {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Tenant(_, body) => body.validate(),
            Self::Principal(_, body) => body.validate(),
            Self::Binding(_, _, body) => body.validate(),
            Self::Issue(_, body) => body.validate(),
            Self::AuditReader(_, body) => body.validate(),
            Self::Detach(_, _, body) | Self::Revoke(_, _, body) => body.validate(),
        }
    }

    pub fn operation_id(&self) -> &OperationId {
        match self {
            Self::Tenant(_, body) => &body.operation_id,
            Self::Principal(_, body) => &body.operation_id,
            Self::Binding(_, _, body) => &body.operation_id,
            Self::Issue(_, body) => &body.operation_id,
            Self::AuditReader(_, body) => &body.operation_id,
            Self::Detach(_, _, body) | Self::Revoke(_, _, body) => &body.operation_id,
        }
    }

    pub fn installation(&self) -> &InstallationId {
        match self {
            Self::Tenant(_, body) => &body.installation_id,
            Self::Principal(_, body) => &body.installation_id,
            Self::Binding(_, _, body) => &body.installation_id,
            Self::Issue(_, body) => &body.installation_id,
            Self::AuditReader(_, body) => &body.installation_id,
            Self::Detach(_, _, body) | Self::Revoke(_, _, body) => &body.installation_id,
        }
    }

    pub fn safety(&self) -> bool {
        matches!(self, Self::Revoke(..))
            || matches!(self, Self::Tenant(_, body) if body.state != TenantState::Active && body.expected_revision.is_some())
    }

    pub fn digest(&self, principal: &PrincipalId) -> anyhow::Result<String> {
        let (method, kind, targets, body) = match self {
            Self::Tenant(tenant, body) => (
                "PUT",
                "tenant.put",
                json!({"tenant_id": tenant}),
                serde_json::to_value(body)?,
            ),
            Self::Principal(principal, body) => (
                "PUT",
                "principal.put",
                json!({"principal_id": principal}),
                serde_json::to_value(body)?,
            ),
            Self::Binding(tenant, binding, body) => (
                "PUT",
                "binding.put",
                json!({"tenant_id": tenant, "binding_id": binding}),
                serde_json::to_value(body)?,
            ),
            Self::Issue(tenant, body) => (
                "POST",
                "credential.create",
                json!({"tenant_id": tenant}),
                serde_json::to_value(body)?,
            ),
            Self::AuditReader(tenant, body) => (
                "POST",
                "audit-reader.create",
                json!({"tenant_id": tenant}),
                serde_json::to_value(body)?,
            ),
            Self::Detach(tenant, binding, body) => (
                "DELETE",
                "binding.detach",
                json!({"tenant_id": tenant, "binding_id": binding}),
                serde_json::to_value(body)?,
            ),
            Self::Revoke(tenant, credential, body) => (
                "DELETE",
                "credential.revoke",
                json!({"tenant_id": tenant, "credential_id": credential}),
                serde_json::to_value(body)?,
            ),
        };
        let mut value = json!({
            "hash_version": 1,
            "installation_id": self.installation(),
            "principal_id": principal,
            "method": method,
            "operation_kind": kind,
            "target_ids": targets,
            "body": body,
        });
        sort(&mut value);
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&value)?)))
    }
}

fn sort(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.sort_keys();
            for value in object.values_mut() {
                sort(value);
            }
        }
        Value::Array(array) => array.iter_mut().for_each(sort),
        _ => {}
    }
}
