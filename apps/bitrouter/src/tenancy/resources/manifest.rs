//! Privileged JSON import format. No public tenant request accepts this shape.

use crate::{gateway::providers::auth::credentials::Credential, policy_lock::PolicyLock};
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    identity::InstallationId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, io::Write};

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

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Definition {
    Route {
        /// Complete explicit routing source, retained rather than reread.
        routing: Value,
        selector: String,
        policy: Option<ResourceRef>,
        credentials: Vec<CredentialBinding>,
        #[serde(default)]
        exploration: Vec<String>,
    },
    Policy {
        document: PolicyLock,
        name: String,
        #[serde(default)]
        dependencies: Vec<ResourceRef>,
    },
    Credential {
        credential: Credential,
        /// Token generation is separate from the ResourceRef's immutable
        /// authority revision. Only static credentials/generation zero qualify.
        #[serde(default)]
        oauth_generation: u64,
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
            Definition::Route {
                selector,
                credentials,
                exploration,
                ..
            } => {
                ensure!(
                    !selector.is_empty() && selector.len() <= 512,
                    "invalid route selector"
                );
                ensure!(
                    credentials.len() <= 256 && exploration.len() <= 256,
                    "route capacity exceeded"
                );
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
                ensure!(
                    exploration.iter().all(|s| !s.is_empty() && s.len() <= 512),
                    "invalid exploration selector"
                );
                ResourceKind::Route
            }
            Definition::Policy {
                document,
                name,
                dependencies,
            } => {
                crate::policy_lock::validate_document(document)
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
                    dependencies.iter().all(|r| matches!(
                        r.resource_kind,
                        ResourceKind::Credential | ResourceKind::Checkpoint
                    )),
                    "invalid policy dependency kind"
                );
                ResourceKind::Policy
            }
            Definition::Credential {
                credential,
                oauth_generation,
            } => {
                ensure!(
                    *oauth_generation == 0,
                    "OAuth generations are not qualified for tenancy"
                );
                match credential {
                    Credential::ApiKey { value } => ensure!(
                        !value.is_empty() && value.len() <= 65_536,
                        "static credential exceeds bounds"
                    ),
                    _ => anyhow::bail!(
                        "OAuth and ambient credential backends are not qualified for tenancy"
                    ),
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
    pub fn policy_reference(&self) -> Option<&ResourceRef> {
        match self {
            Self::Route { policy, .. } => policy.as_ref(),
            _ => None,
        }
    }

    pub fn dependencies(&self) -> Vec<ResourceRef> {
        let candidates: Vec<_> = match self {
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
