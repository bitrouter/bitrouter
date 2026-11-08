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
    resources_v2::DefinitionV2,
};

type Result<T> = std::result::Result<T, ErrorCode>;

#[derive(Clone, Copy)]
enum Content<'a> {
    V1(&'a PublicDefinition),
    V2(&'a DefinitionV2),
}

#[derive(Clone, Copy)]
pub(super) struct Put<'a> {
    pub key: &'a ResourceKey,
    pub expected_revision: &'a Option<RevisionId>,
    content: Content<'a>,
}

pub(super) fn input(command: &Command) -> Option<(&TenantId, Put<'_>)> {
    match command {
        Command::Resource(tenant, input) => Some((
            tenant,
            Put {
                key: &input.key,
                expected_revision: &input.expected_revision,
                content: Content::V1(&input.definition),
            },
        )),
        Command::ResourceV2(tenant, input) => Some((
            tenant,
            Put {
                key: &input.key,
                expected_revision: &input.expected_revision,
                content: Content::V2(&input.definition),
            },
        )),
        _ => None,
    }
}

impl Put<'_> {
    pub(super) fn authorize(self, state: &Snapshot, caller: &Caller) -> Result<()> {
        let operator_only = match self.content {
            Content::V1(definition) => matches!(
                definition,
                PublicDefinition::AdmissionProfile { .. } | PublicDefinition::ProfiledRoute { .. }
            ),
            Content::V2(_) => true,
        };
        if operator_only && !matches!(caller, Caller::Installation(_)) {
            return Err(ErrorCode::Forbidden);
        }
        match self.content {
            Content::V1(definition) => state.resource_admission.validate_put(self.key, definition),
            Content::V2(DefinitionV2::AdmissionProfileV2 { profile }) => state
                .resource_admission
                .validate_shared_put(self.key, profile),
        }
    }

    pub(super) fn affects_admission(self) -> bool {
        match self.content {
            Content::V1(definition) => matches!(
                definition,
                PublicDefinition::AdmissionProfile { .. } | PublicDefinition::ProfiledRoute { .. }
            ),
            Content::V2(_) => true,
        }
    }

    pub(super) fn apply(self, state: &mut State, reference: &ResourceRef) {
        match self.content {
            Content::V1(definition) => state.apply_put(reference, definition),
            Content::V2(DefinitionV2::AdmissionProfileV2 { profile }) => {
                state.apply_shared_put(reference, profile)
            }
        }
    }

    pub(super) fn definition(self) -> Result<Definition> {
        let definition = match self.content {
            Content::V1(definition) => definition,
            Content::V2(DefinitionV2::AdmissionProfileV2 { profile }) => {
                return Ok(Definition::AdmissionProfileV2 {
                    profile: profile.clone(),
                });
            }
        };
        Ok(match definition {
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
                document: serde_json::from_value(document.clone())
                    .map_err(|_| ErrorCode::InvalidRequest)?,
                name: name.clone(),
                dependencies: dependencies.clone(),
            },
        })
    }
}
