//! Validate complete inputs before acquiring the authoritative mutation fence.

use anyhow::Result;
use bitrouter_ai::gateway::execution::ResolveAttemptRequest;
use bitrouter_ai::gateway::operator::{
    Command, CredentialImport, CredentialInput, CredentialMetadata, Outcome, PrincipalPut,
    ResourcePut, Revision, TenantPut,
};
use sea_orm::DatabaseTransaction;

use crate::gateway::providers::auth::{
    credentials::{Credential, OAuthToken},
    store::CredentialKey,
};
use crate::gateway::shared::{
    authority::{Authority, Head},
    credentials::{Credentials, PreparedImport},
    engine::{management, resolution},
    publication::{Configuration, CredentialBinding, PreparedPublication},
    store::Stored,
};

pub(super) enum Prepared<'a> {
    Tenant(&'a TenantPut),
    Resource(&'a ResourcePut),
    Principal(&'a PrincipalPut),
    Credential {
        input: &'a CredentialImport,
        material: PreparedImport,
    },
    Publication(PreparedPublication),
    Rotation(u64),
    Resolution {
        execution_id: &'a str,
        attempt_id: &'a str,
        request: &'a ResolveAttemptRequest,
    },
}

impl<'a> Prepared<'a> {
    pub fn new(authority: &Authority, operator_id: &str, command: &'a Command) -> Result<Self> {
        Ok(match command {
            Command::PutTenant(input) => {
                management::validate_tenant(input)?;
                Self::Tenant(input)
            }
            Command::PutResource(input) => {
                management::validate_resource(input)?;
                Self::Resource(input)
            }
            Command::PutPrincipal(input) => {
                super::principals::validate(input)?;
                Self::Principal(input)
            }
            Command::ImportCredential(input) => {
                let credential = match &input.credential {
                    CredentialInput::ApiKey { value } => Credential::ApiKey {
                        value: value.clone(),
                    },
                    CredentialInput::Oauth {
                        access_token,
                        refresh_token,
                        expires_at,
                    } => Credential::Oauth(OAuthToken {
                        access_token: access_token.clone(),
                        refresh_token: refresh_token.clone(),
                        expires_at: *expires_at,
                    }),
                };
                let slot = CredentialKey {
                    provider: input.provider.clone(),
                    account: input.account.clone(),
                };
                let material = PreparedImport::new(
                    &slot,
                    &input.resource_pool_id,
                    input.expected_version,
                    &credential,
                )?;
                Self::Credential { input, material }
            }
            Command::Publish(input) => {
                let config = &input.configuration;
                let configuration = Configuration {
                    format: config.format,
                    routing: config.routing.clone(),
                    policies: config.policies.clone(),
                    policy_lock: config
                        .policy_lock
                        .clone()
                        .map(serde_json::from_value)
                        .transpose()?,
                    credentials: config
                        .credentials
                        .iter()
                        .map(|binding| CredentialBinding {
                            provider: binding.provider.clone(),
                            account: binding.account.clone(),
                            credential_provider: binding.credential_provider.clone(),
                            credential_account: binding.credential_account.clone(),
                        })
                        .collect(),
                    require_known_pricing: config.require_known_pricing,
                };
                Self::Publication(
                    authority.prepare_publication(input.expected_generation, &configuration)?,
                )
            }
            Command::RotateCorrelation {
                expected_generation,
            } => Self::Rotation(*expected_generation),
            Command::ResolveExecution {
                execution_id,
                attempt_id,
                resolution: request,
            } => {
                resolution::validate_resolution(operator_id, execution_id, attempt_id, request)?;
                Self::Resolution {
                    execution_id,
                    attempt_id,
                    request,
                }
            }
        })
    }

    pub fn exclusive(&self) -> bool {
        matches!(self, Self::Publication(_) | Self::Rotation(_))
    }

    pub async fn apply(
        &self,
        authority: &Authority,
        tx: &DatabaseTransaction,
        head: &mut Stored<Head>,
        operator_id: &str,
    ) -> Result<Outcome> {
        Ok(match self {
            Self::Tenant(input) => {
                Outcome::Tenant(management::put_tenant(authority, tx, input).await?)
            }
            Self::Resource(input) => {
                Outcome::Resource(management::put_resource(authority, tx, input).await?)
            }
            Self::Principal(input) => {
                Outcome::Principal(super::principals::put(authority, tx, input).await?)
            }
            Self::Credential { input, material } => {
                let stamp = Credentials::new(authority.clone())
                    .import_in(tx, material)
                    .await?;
                Outcome::Credential(CredentialMetadata {
                    provider: input.provider.clone(),
                    account: input.account.clone(),
                    resource_pool_id: stamp.pool_id,
                    version: stamp.version,
                })
            }
            Self::Publication(input) => {
                authority.publish_in(tx, head, input).await?;
                Outcome::Published(revision(&head.value))
            }
            Self::Rotation(expected) => {
                authority.rotate_correlation_in(tx, head, *expected).await?;
                Outcome::CorrelationRotated(revision(&head.value))
            }
            Self::Resolution {
                execution_id,
                attempt_id,
                request,
            } => Outcome::Execution(
                resolution::resolve_in(
                    authority,
                    tx,
                    operator_id,
                    execution_id,
                    attempt_id,
                    request,
                )
                .await?,
            ),
        })
    }
}

pub(super) fn revision(head: &Head) -> Revision {
    Revision {
        config_generation: head.config_generation,
        key_generation: head.key_generation,
    }
}
