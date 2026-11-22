//! Tenant-owned management input; platform configuration has no writer command.

use super::super::manifest::{CredentialBinding, Definition};
use crate::tenancy::{command::Command, model::Snapshot};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKey},
    identity::{RevisionId, TenantId},
    operation::ErrorCode,
    resources::Definition as PublicDefinition,
};

type Result<T> = std::result::Result<T, ErrorCode>;

#[derive(Clone, Copy)]
pub(super) struct Put<'a> {
    pub key: &'a ResourceKey,
    pub expected_revision: &'a Option<RevisionId>,
    definition: &'a PublicDefinition,
}

pub(super) fn input(command: &Command) -> Option<(&TenantId, Put<'_>)> {
    match command {
        Command::Resource(tenant, input) => Some((
            tenant,
            Put {
                key: &input.key,
                expected_revision: &input.expected_revision,
                definition: &input.definition,
            },
        )),
        _ => None,
    }
}

impl Put<'_> {
    pub(super) fn authorize(self, state: &Snapshot) -> Result<()> {
        if self.key.owner_kind != OwnerKind::Tenant {
            return Err(ErrorCode::Forbidden);
        }
        state
            .resource_admission
            .validate_put(self.key, self.definition)
    }

    pub(super) fn definition(self) -> Result<Definition> {
        Ok(match self.definition {
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
        })
    }
}
