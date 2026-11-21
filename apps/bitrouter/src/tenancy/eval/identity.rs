//! Validate claimed Eval ownership against immutable retained service records.

use super::*;
use bitrouter_ai::{
    eval::{EvalExecutionRef, EvalSubjectIdentity},
    tenancy::{authority::Permission, identity::RevisionId},
};
use sea_orm::ConnectionTrait;

/// Derive producer identity from admitted immutable facts, never from caller metadata.
#[cfg(feature = "eval-publication")]
pub(crate) fn for_execution(
    execution: &super::super::execution::TenantExecution,
) -> anyhow::Result<EvalSubjectIdentity> {
    let (installation, tenant, _) = execution.storage_identity();
    Ok(match execution.route().policy() {
        Some(policy) => EvalSubjectIdentity::TenantModelPolicy {
            installation_id: installation.clone(),
            tenant_id: tenant.clone(),
            configuration_revision: execution.configuration_revision().clone(),
            policy: policy.reference().clone(),
            name: policy.name().to_owned(),
            source_digest: Some(crate::model_policy::source_digest(policy.document())?),
            classifier_digest: policy
                .document()
                .policies
                .get(policy.name())
                .and_then(|selected| selected.classifier_digest.clone()),
            execution: None,
        },
        None => EvalSubjectIdentity::TenantGeneric {
            installation_id: installation.clone(),
            tenant_id: tenant.clone(),
            execution: None,
        },
    })
}

impl EvalIdentity {
    pub(super) fn validate_subject_namespace(&self, subject: &EvalSubject) -> Result<(), Failure> {
        let (installation, tenant) = subject
            .identity
            .tenant_namespace()
            .ok_or(ErrorCode::InvalidRequest)?;
        if installation != &self.installation {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        if tenant != &self.tenant {
            return Err(ErrorCode::Forbidden.into());
        }
        Ok(())
    }

    pub(in crate::tenancy) async fn validate_subject_in(
        &self,
        db: &impl ConnectionTrait,
        resources: &super::super::resources::Resources,
        subject: &EvalSubject,
        check: &(dyn Fn() -> Result<(), ErrorCode> + Sync),
    ) -> Result<(), Failure> {
        self.validate_subject_namespace(subject)?;
        check().map_err(Failure::from)?;
        match &subject.identity {
            EvalSubjectIdentity::TenantModelPolicy {
                configuration_revision,
                policy,
                name,
                source_digest,
                classifier_digest,
                execution,
                ..
            } => {
                let stored = self.configuration_in(db, configuration_revision).await?;
                let authority_revision = self
                    .authority
                    .read()
                    .map_err(Failure::from)?
                    .check(&self.lease, |view| Ok(view.facts().revision))
                    .map_err(Failure::from)?;
                for dependency in &stored.dependencies {
                    check().map_err(Failure::from)?;
                    super::super::resources::lifecycle::check_in(
                        db,
                        &self.installation,
                        authority_revision,
                        dependency,
                    )
                    .await
                    .map_err(Failure::from)?;
                }
                let retained = resources.open_execution(&stored).map_err(Failure::from)?;
                if !retained.matches_eval_policy(
                    policy,
                    name,
                    source_digest.as_deref(),
                    classifier_digest.as_deref(),
                ) {
                    return Err(ErrorCode::InvalidRequest.into());
                }
                self.authority
                    .read()
                    .map_err(Failure::from)?
                    .check(&self.lease, |view| {
                        super::super::configuration::authorize(
                            view.facts(),
                            &Caller::Credential(self.credential.clone()),
                            &self.tenant,
                            Permission::EvaluationSubmit,
                            &stored.dependencies,
                            Utc::now(),
                        )
                    })
                    .map_err(Failure::from)?;
                if let Some(execution) = execution {
                    let actual = self
                        .execution_in(db, execution, &retained)
                        .await?
                        .ok_or(ErrorCode::InvalidRequest)?;
                    if actual.reference() != policy || actual.name() != name {
                        return Err(ErrorCode::InvalidRequest.into());
                    }
                }
            }
            EvalSubjectIdentity::TenantGeneric {
                execution: Some(execution),
                ..
            } => {
                let stored = self
                    .configuration_in(db, &execution.configuration_revision)
                    .await?;
                let retained = resources.open_execution(&stored).map_err(Failure::from)?;
                if self.execution_in(db, execution, &retained).await?.is_some() {
                    return Err(ErrorCode::InvalidRequest.into());
                }
            }
            EvalSubjectIdentity::TenantGeneric {
                execution: None, ..
            } => {}
            EvalSubjectIdentity::GatewayGeneric {} => return Err(ErrorCode::InvalidRequest.into()),
        }
        check().map_err(Failure::from)
    }

    async fn configuration_in(
        &self,
        db: &impl ConnectionTrait,
        revision: &RevisionId,
    ) -> Result<super::super::configuration_store::StoredConfiguration, Failure> {
        super::super::configuration_store::configuration(db, &self.tenant, revision)
            .await
            .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?
            .ok_or_else(|| ErrorCode::InvalidRequest.into())
    }

    async fn execution_in<'a>(
        &self,
        db: &impl ConnectionTrait,
        execution: &EvalExecutionRef,
        retained: &'a super::super::resources::execution::RetainedExecutionRevision,
    ) -> Result<Option<&'a super::super::resources::execution::RetainedPolicy>, Failure> {
        let receipt = super::super::receipts::store::lookup_terminal(
            db,
            &self.installation,
            &self.tenant,
            &execution.execution_id,
        )
        .await
        .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?
        .ok_or(ErrorCode::InvalidRequest)?;
        let receipt = match &receipt {
            super::super::receipts::store::format::Terminal::V1(candidate) => &candidate.receipt,
            super::super::receipts::store::format::Terminal::V2(candidate) => &candidate.receipt,
        };
        let revision = &receipt.execution_revision;
        let route = match &receipt.dispatch {
            bitrouter_ai::tenancy::execution::Dispatch::Strict => None,
            bitrouter_ai::tenancy::execution::Dispatch::Ordered {
                accepted_selection, ..
            } => Some(&accepted_selection.route),
        };
        let targets: Vec<_> = receipt
            .attempts
            .iter()
            .filter_map(|attempt| attempt.target.as_ref())
            .collect();
        if revision != &execution.configuration_revision || revision != retained.revision_id() {
            return Err(ErrorCode::InvalidRequest.into());
        }
        retained
            .eval_receipt_policy(route, &targets)
            .map_err(Failure::from)
    }
}
