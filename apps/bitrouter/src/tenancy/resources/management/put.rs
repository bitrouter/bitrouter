//! Borrowed management inputs share one prepare/apply transaction path.

use super::super::{
    admission::State,
    manifest::{CredentialBinding, Definition},
};
use crate::tenancy::{
    command::Command,
    model::{Caller, Snapshot},
};
use bitrouter_ai::tenancy::{
    authority::ResourceKey,
    configuration::ResourceRef,
    identity::{RevisionId, TenantId},
    operation::ErrorCode,
    resources::Definition as PublicDefinition,
};

type Result<T> = std::result::Result<T, ErrorCode>;

#[derive(Clone, Copy)]
pub(super) struct Put<'a> {
    pub key: &'a ResourceKey,
    pub expected_revision: &'a Option<RevisionId>,
    content: &'a PublicDefinition,
}

pub(super) fn input(command: &Command) -> Option<(&TenantId, Put<'_>)> {
    match command {
        Command::Resource(tenant, input) => Some((
            tenant,
            Put {
                key: &input.key,
                expected_revision: &input.expected_revision,
                content: &input.definition,
            },
        )),
        _ => None,
    }
}

impl Put<'_> {
    pub(super) fn authorize(self, state: &Snapshot, caller: &Caller) -> Result<()> {
        let operator_only = self.affects_admission();
        if operator_only && !matches!(caller, Caller::Installation(_)) {
            return Err(ErrorCode::Forbidden);
        }
        state
            .resource_admission
            .validate_put(self.key, self.content)
    }

    pub(super) fn affects_admission(self) -> bool {
        matches!(
            self.content,
            PublicDefinition::AdmissionProfile { .. } | PublicDefinition::ProfiledRoute { .. }
        )
    }

    pub(super) fn apply(self, state: &mut State, reference: &ResourceRef) {
        state.apply_put(reference, self.content);
    }

    pub(super) fn definition(self) -> Result<Definition> {
        Ok(match self.content {
            PublicDefinition::AdmissionProfile { profile } => Definition::AdmissionProfile {
                profile: profile.clone(),
            },
            PublicDefinition::ProfiledRoute {
                routing,
                selector,
                policy,
                credentials,
                attachments,
            } => Definition::ProfiledRoute {
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
                attachments: attachments.clone(),
            },
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
                document: document.clone(),
                name: name.clone(),
                dependencies: dependencies.clone(),
            },
        })
    }
}
