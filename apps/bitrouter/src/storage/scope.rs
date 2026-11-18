//! Internal storage namespaces and immutable provenance, never live authority.
//!
//! Global gateway keys retain their exact bytes. Tenant write namespaces come
//! from admitted executions or authenticated ledger identities. Retained-row
//! validation derives inert key namespaces without granting live authority.

use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::identity::{InstallationId, PrincipalId, TenantId};
use sea_orm::sea_query::{Condition, Expr, IntoColumnRef};

#[cfg(feature = "eval-publication")]
use crate::tenancy::execution::TenantExecution;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scope(Namespace);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Namespace {
    Legacy,
    Tenant {
        installation: InstallationId,
        tenant: TenantId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Provenance(Option<PrincipalId>);

pub(crate) struct MetadataRef<'a> {
    pub installation: Option<&'a str>,
    pub tenant: Option<&'a str>,
    pub creator: Option<&'a str>,
}

pub(crate) struct Metadata {
    pub installation: Option<String>,
    pub tenant: Option<String>,
    pub creator: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) enum KeyDomain {
    Owner,
    EvalSubject,
    EvalResult,
    EvalIdempotency,
    EvalSnapshot,
}

impl KeyDomain {
    fn token(self) -> &'static str {
        match self {
            Self::Owner => "owner.compat",
            Self::EvalSubject => "eval.subject",
            Self::EvalResult => "eval.result",
            Self::EvalIdempotency => "eval.idempotency",
            Self::EvalSnapshot => "eval.snapshot",
        }
    }
}

impl Scope {
    pub(crate) fn legacy() -> Self {
        Self(Namespace::Legacy)
    }

    #[cfg(feature = "eval-publication")]
    pub(crate) fn for_execution(execution: &TenantExecution) -> (Self, Provenance) {
        let (installation, tenant, principal) = execution.storage_identity();
        (
            Self(Namespace::Tenant {
                installation: installation.clone(),
                tenant: tenant.clone(),
            }),
            Provenance(Some(principal.clone())),
        )
    }

    pub(crate) fn for_eval(identity: &crate::tenancy::eval::EvalIdentity) -> (Self, Provenance) {
        let (installation, tenant, principal) = identity.storage_identity();
        (
            Self(Namespace::Tenant {
                installation: installation.clone(),
                tenant: tenant.clone(),
            }),
            Provenance(Some(principal.clone())),
        )
    }

    /// Inert namespace for canonical retained-row validation, never live authority.
    pub(crate) fn for_retained_eval(installation: InstallationId, tenant: TenantId) -> Self {
        Self(Namespace::Tenant {
            installation,
            tenant,
        })
    }

    pub(crate) fn is_legacy(&self) -> bool {
        matches!(self.0, Namespace::Legacy)
    }

    pub(crate) fn predicate(
        &self,
        installation_column: impl IntoColumnRef,
        tenant_column: impl IntoColumnRef,
    ) -> Condition {
        let installation_column = installation_column.into_column_ref();
        let tenant_column = tenant_column.into_column_ref();
        match &self.0 {
            Namespace::Legacy => Condition::all()
                .add(Expr::col(installation_column).is_null())
                .add(Expr::col(tenant_column).is_null()),
            Namespace::Tenant {
                installation,
                tenant,
            } => Condition::all()
                .add(Expr::col(installation_column).eq(installation.as_str()))
                .add(Expr::col(tenant_column).eq(tenant.as_str())),
        }
    }

    pub(crate) fn metadata(&self, provenance: &Provenance) -> Result<Metadata> {
        match (&self.0, provenance.creator()) {
            (Namespace::Legacy, None) => Ok(Metadata {
                installation: None,
                tenant: None,
                creator: None,
            }),
            (
                Namespace::Tenant {
                    installation,
                    tenant,
                },
                Some(creator),
            ) => Ok(Metadata {
                installation: Some(installation.to_string()),
                tenant: Some(tenant.to_string()),
                creator: Some(creator.to_string()),
            }),
            _ => anyhow::bail!("inconsistent storage provenance"),
        }
    }

    pub(crate) fn validate_metadata(&self, row: MetadataRef<'_>) -> Result<Provenance> {
        match (&self.0, row.installation, row.tenant, row.creator) {
            (Namespace::Legacy, None, None, None) => Ok(Provenance::legacy()),
            (
                Namespace::Tenant {
                    installation,
                    tenant,
                },
                Some(actual_installation),
                Some(actual_tenant),
                Some(creator),
            ) => {
                ensure!(
                    installation.as_str() == actual_installation
                        && tenant.as_str() == actual_tenant,
                    "storage namespace mismatch"
                );
                Ok(Provenance(Some(creator.try_into()?)))
            }
            _ => anyhow::bail!("invalid storage namespace metadata"),
        }
    }

    pub(crate) fn physical_key(
        &self,
        domain: KeyDomain,
        legacy_key: &str,
        logical_parts: &[&str],
    ) -> Result<String> {
        let Namespace::Tenant {
            installation,
            tenant,
        } = &self.0
        else {
            return Ok(legacy_key.to_owned());
        };
        physical_key_for_namespace(installation, tenant, domain, logical_parts)
    }

    /// Snapshot roots commit the namespace, independently of physical indexing.
    pub(crate) fn snapshot_namespace(&self) -> Option<(&str, &str)> {
        match &self.0 {
            Namespace::Legacy => None,
            Namespace::Tenant {
                installation,
                tenant,
            } => Some((installation.as_str(), tenant.as_str())),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        installation: InstallationId,
        tenant: TenantId,
        creator: PrincipalId,
    ) -> (Self, Provenance) {
        (
            Self(Namespace::Tenant {
                installation,
                tenant,
            }),
            Provenance(Some(creator)),
        )
    }
}

/// Inert key encoding shared with the offline converter. No authority is created.
pub(crate) fn physical_key_for_namespace(
    installation: &InstallationId,
    tenant: &TenantId,
    domain: KeyDomain,
    logical_parts: &[&str],
) -> Result<String> {
    super::partition::tenant_key(installation, tenant, domain.token(), logical_parts)
}

impl Provenance {
    pub(crate) fn legacy() -> Self {
        Self(None)
    }
    pub(crate) fn creator(&self) -> Option<&PrincipalId> {
        self.0.as_ref()
    }
}

#[cfg(test)]
mod tests;
