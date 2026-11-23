//! Privileged JSON import format. No public tenant request accepts this shape.

use crate::gateway::providers::auth::credentials::Credential;
use anyhow::{Result, ensure};
use bitrouter_ai::policy::Document;
use bitrouter_ai::tenancy::{
    admission::{AdmissionAttachment, AdmissionProfile},
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    identity::InstallationId,
    validation::Validate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, io::Write};

mod decoding;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub format: u32,
    pub installation_id: InstallationId,
    pub resources: Vec<Import>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Import {
    pub reference: ResourceRef,
    pub definition: Definition,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Definition {
    AdmissionProfile {
        profile: AdmissionProfile,
    },
    ProfiledRoute {
        routing: Value,
        selector: String,
        policy: Option<ResourceRef>,
        credentials: Vec<CredentialBinding>,
        attachments: Vec<AdmissionAttachment>,
    },
    Route {
        /// Complete explicit routing source, retained rather than reread.
        routing: Value,
        selector: String,
        policy: Option<ResourceRef>,
        credentials: Vec<CredentialBinding>,
    },
    Policy {
        document: Document,
        name: String,
        #[serde(default)]
        dependencies: Vec<ResourceRef>,
    },
    Credential {
        credential: Credential,
        /// Token generation is separate from the ResourceRef's immutable
        /// authority revision. Imports seed generation zero; the private live
        /// OAuth slot owns every subsequent token generation.
        #[serde(default)]
        oauth_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<crate::tenancy::oauth::profile::Profile>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CredentialBinding {
    pub provider: String,
    /// Explicit account label, including "default" for a single-account provider.
    pub account: String,
    pub credential: ResourceRef,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Artifact {
    pub reference: ResourceRef,
    pub definition: Definition,
    pub envelope: Option<super::compiler::RouteEnvelope>,
}

impl Import {
    pub fn validate(&self) -> Result<()> {
        bounded_bytes(self, super::MAX_ARTIFACT_BYTES)?;
        let kind = match &self.definition {
            Definition::AdmissionProfile { profile } => {
                profile.validate()?;
                ensure!(
                    self.reference.owner_kind == OwnerKind::OperatorPool,
                    "admission profile requires an operator-pool owner"
                );
                ResourceKind::Policy
            }
            Definition::ProfiledRoute {
                routing,
                selector,
                policy,
                credentials,
                attachments,
            } => {
                bitrouter_ai::tenancy::resources::Definition::ProfiledRoute {
                    routing: routing.clone(),
                    selector: selector.clone(),
                    policy: policy.clone(),
                    credentials: credentials
                        .iter()
                        .map(
                            |binding| bitrouter_ai::tenancy::resources::CredentialBinding {
                                provider: binding.provider.clone(),
                                account: binding.account.clone(),
                                credential: binding.credential.clone(),
                            },
                        )
                        .collect(),
                    attachments: attachments.clone(),
                }
                .validate()?;
                ResourceKind::Route
            }
            Definition::Route {
                selector,
                credentials,
                ..
            } => {
                ensure!(
                    !selector.is_empty() && selector.len() <= 512,
                    "invalid route selector"
                );
                ensure!(credentials.len() <= 256, "route capacity exceeded");
                let mut slots = HashSet::new();
                for binding in credentials {
                    ensure!(
                        !binding.provider.is_empty()
                            && binding.provider.len() <= 128
                            && !binding.account.is_empty()
                            && binding.account.len() <= 128
                            && slots.insert((&binding.provider, &binding.account))
                            && binding.credential.resource_kind == ResourceKind::Credential,
                        "invalid route credential binding"
                    );
                }
                ResourceKind::Route
            }
            Definition::Policy {
                document,
                name,
                dependencies,
            } => {
                crate::policy_source::validate_document(document)
                    .map_err(|_| anyhow::anyhow!("invalid policy artifact"))?;
                ensure!(
                    document.policies.contains_key(name) && name.len() <= 256,
                    "missing selected named policy"
                );
                ensure!(
                    dependencies.len() <= 256,
                    "policy dependency capacity exceeded"
                );
                // A route/policy cycle cannot create an unbounded or implicit closure.
                ensure!(
                    dependencies
                        .iter()
                        .all(|r| matches!(r.resource_kind, ResourceKind::Credential)),
                    "invalid policy dependency kind"
                );
                ResourceKind::Policy
            }
            Definition::Credential {
                credential,
                oauth_generation,
                oauth,
            } => {
                ensure!(
                    *oauth_generation == 0,
                    "imported OAuth seed generation must be zero"
                );
                match credential {
                    Credential::ApiKey { value } => ensure!(
                        oauth.is_none() && !value.is_empty() && value.len() <= 65_536,
                        "static credential exceeds bounds"
                    ),
                    Credential::Oauth(token) => {
                        oauth
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("missing owned OAuth profile"))?
                            .validate()?;
                        crate::tenancy::oauth::profile::validate_token(token)?;
                    }
                    _ => anyhow::bail!("ambient credential backends are not qualified for tenancy"),
                }
                ResourceKind::Credential
            }
        };
        ensure!(
            self.reference.resource_kind == kind,
            "resource kind mismatch"
        );
        if let Some(policy) = self.definition.policy_reference() {
            ensure!(
                policy.resource_kind == ResourceKind::Policy,
                "invalid policy reference kind"
            );
        }
        ensure!(
            self.definition
                .dependencies()
                .iter()
                .all(|dependency| dependency.owner_kind != OwnerKind::Tenant
                    || (self.reference.owner_kind == OwnerKind::Tenant
                        && dependency.owner_id == self.reference.owner_id)),
            "resource depends on a foreign tenant"
        );
        Ok(())
    }
}

impl Definition {
    pub(super) fn uses_model_policy(&self) -> bool {
        matches!(self, Self::Policy { .. }) || self.policy_reference().is_some()
    }

    pub(super) fn is_admission_profile(&self) -> bool {
        matches!(self, Self::AdmissionProfile { .. })
    }

    pub(super) fn is_native_route(&self) -> bool {
        matches!(self, Self::ProfiledRoute { attachments, .. } if attachments.iter().any(|item| item.inbound_protocol == bitrouter_ai::tenancy::ordered::GenerationProtocol::GenerateContent))
    }

    pub(super) fn admission(&self) -> Option<(&Option<ResourceRef>, &[AdmissionAttachment])> {
        match self {
            Self::ProfiledRoute {
                policy,
                attachments,
                ..
            } => Some((policy, attachments)),
            _ => None,
        }
    }

    pub(super) fn is_profiled(&self) -> bool {
        matches!(
            self,
            Self::AdmissionProfile { .. } | Self::ProfiledRoute { .. }
        )
    }

    pub(super) fn is_route(&self) -> bool {
        matches!(self, Self::Route { .. } | Self::ProfiledRoute { .. })
    }

    pub fn policy_reference(&self) -> Option<&ResourceRef> {
        match self {
            Self::Route { policy, .. } | Self::ProfiledRoute { policy, .. } => policy.as_ref(),
            _ => None,
        }
    }

    pub fn dependencies(&self) -> Vec<ResourceRef> {
        let candidates: Vec<_> = match self {
            Self::AdmissionProfile { .. } => Vec::new(),
            Self::ProfiledRoute {
                policy,
                credentials,
                attachments,
                ..
            } => policy
                .iter()
                .cloned()
                .chain(credentials.iter().map(|binding| binding.credential.clone()))
                .chain(
                    attachments
                        .iter()
                        .map(|attachment| attachment.profile.clone()),
                )
                .collect(),
            Self::Route {
                policy,
                credentials,
                ..
            } => policy
                .iter()
                .cloned()
                .chain(credentials.iter().map(|binding| binding.credential.clone()))
                .collect(),
            Self::Policy { dependencies, .. } => dependencies.clone(),
            Self::Credential { .. } => Vec::new(),
        };
        let mut seen = HashSet::new();
        candidates
            .into_iter()
            .filter(|r| seen.insert(r.clone()))
            .collect()
    }
}

struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
}

/// Count serialized plaintext without allocating a second copy. Callers charge
/// each object before retaining it, so an aggregate cannot grow to count * size.
pub(super) struct ByteBudget {
    remaining: usize,
}

impl ByteBudget {
    pub fn new(limit: usize) -> Self {
        Self { remaining: limit }
    }

    pub fn charge(&mut self, value: &impl Serialize) -> Result<()> {
        serde_json::to_writer(self, value)
            .map_err(|_| anyhow::anyhow!("resource byte bound exceeded"))
    }

    pub fn remaining(&self) -> usize {
        self.remaining
    }

    pub fn separator(&mut self, needed: bool) -> Result<()> {
        if needed {
            self.write_all(b",")?;
        }
        Ok(())
    }
}

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("resource byte bound exceeded"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("resource byte bound exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn bounded_bytes(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    let mut writer = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| anyhow::anyhow!("resource byte bound exceeded"))?;
    Ok(writer.bytes)
}
