//! Ordered immutable terminal facts and original evidence share the authority writer.
use super::store::{
    Disposition, StoredDisposition,
    format::{Format, Terminal},
};
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    execution_v2::ExecutionReceiptV2, usage_evidence_v2::ExecutionUsageEvidenceV2,
    validation::Validate,
};

#[path = "ordered_intent.rs"]
pub(in crate::tenancy) mod intent;

pub(in crate::tenancy) struct CandidateV2 {
    pub receipt: ExecutionReceiptV2,
    pub(super) json: String,
    pub(super) private_json: String,
    pub(super) digest: String,
    pub(super) evidence: super::usage_evidence::storage::EvidenceV2,
}
impl CandidateV2 {
    pub fn new(
        receipt: ExecutionReceiptV2,
        disposition: Disposition,
        document: ExecutionUsageEvidenceV2,
    ) -> Result<Self> {
        receipt.validate()?;
        ensure!(
            disposition.auxiliary_digest.is_none(),
            "ordered receipt has auxiliary work"
        );
        ensure!(
            disposition.statuses
                == receipt
                    .attempts
                    .iter()
                    .map(|a| a.observed_http_status)
                    .collect::<Vec<_>>(),
            "ordered disposition status mismatch"
        );
        let evidence = super::usage_evidence::storage::EvidenceV2::new(document, &receipt)?;
        let mut disposition: StoredDisposition = disposition.into();
        disposition.usage_evidence_digest = Some(evidence.digest.clone());
        let json = serde_json::to_string(&receipt)?;
        let private_json = serde_json::to_string(&disposition)?;
        ensure!(
            json.len() <= super::store::MAX_RECEIPT
                && private_json.len() <= super::store::MAX_DISPOSITION,
            "ordered receipt bytes"
        );
        let digest = super::store::digest_for(Format::V2, &json, &private_json);
        Ok(Self {
            receipt,
            json,
            private_json,
            digest,
            evidence,
        })
    }
}

pub(in crate::tenancy) async fn decode(
    db: &impl sea_orm::ConnectionTrait,
    row: sea_orm::QueryResult,
    installation: &bitrouter_ai::tenancy::identity::InstallationId,
    tenant: &bitrouter_ai::tenancy::identity::TenantId,
    id: &bitrouter_ai::tenancy::identity::ExecutionId,
    charge: Option<&super::store::archive::Charge<'_>>,
) -> Result<Terminal> {
    let json: String = row.try_get("", "receipt_json")?;
    let private: String = row.try_get("", "disposition_json")?;
    ensure!(
        json.len() <= super::store::MAX_RECEIPT && private.len() <= super::store::MAX_DISPOSITION,
        "ordered receipt bytes"
    );
    let receipt: ExecutionReceiptV2 = serde_json::from_str(&json)?;
    receipt.validate()?;
    ensure!(
        &receipt.installation_id == installation
            && &receipt.tenant_id == tenant
            && &receipt.execution_id == id,
        "ordered receipt namespace"
    );
    let disposition: StoredDisposition = serde_json::from_str(&private)?;
    let digest = super::store::digest_for(Format::V2, &json, &private);
    let evidence = super::usage_evidence::storage::load_v2(
        db,
        &receipt,
        &digest,
        disposition.usage_evidence_digest.as_deref(),
        charge,
    )
    .await?;
    let candidate = CandidateV2::new(
        receipt,
        Disposition {
            dispatch: disposition.dispatch,
            statuses: disposition.statuses,
            auxiliary_digest: disposition.auxiliary_digest,
        },
        evidence.document,
    )?;
    ensure!(
        candidate.json == json && candidate.private_json == private,
        "ordered canonical bytes"
    );
    let terminal = Terminal::V2(std::sync::Arc::new(candidate));
    super::store::validate_projection(&row, &terminal)?;
    // The ordered mode cannot silently ignore an auxiliary sidecar.
    let foreign = sea_orm::sea_query::Query::select()
        .expr(sea_orm::sea_query::Expr::val(1))
        .from(sea_orm::sea_query::Alias::new("tenancy_receipt_auxiliary"))
        .and_where(
            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("installation_id"))
                .eq(installation.as_str()),
        )
        .and_where(
            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("tenant_id"))
                .eq(tenant.as_str()),
        )
        .and_where(
            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("execution_id"))
                .eq(id.as_str()),
        )
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(db.get_database_backend().build(&foreign))
            .await?
            .is_none(),
        "ordered auxiliary sidecar"
    );
    Ok(terminal)
}

pub(in crate::tenancy) struct IntentJob {
    pub owner: std::sync::Arc<super::owner::Owner>,
    pub reply: tokio::sync::oneshot::Sender<Result<(), crate::tenancy::error::Failure>>,
}
impl crate::tenancy::Service {
    pub(in crate::tenancy) fn ordered_dispatch_ready(
        &self,
        state: &crate::tenancy::model::Snapshot,
    ) -> bool {
        self.startup.is_some() && self.dispatch_ready(state)
    }
    /// Reserve the bounded writer slot before creating an admitted receipt owner.
    pub(in crate::tenancy) fn reserve_ordered_intent(
        &self,
    ) -> Result<
        tokio::sync::mpsc::OwnedPermit<crate::tenancy::actor::Job>,
        crate::tenancy::error::Failure,
    > {
        self.commands
            .clone()
            .try_reserve_owned()
            .map_err(|_| bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded.into())
    }
    /// The existing actor must acknowledge the intent before any provider work.
    pub(in crate::tenancy) async fn prepare_ordered_intent(
        &self,
        execution: &std::sync::Arc<crate::tenancy::execution::TenantExecution>,
        slot: Option<tokio::sync::mpsc::OwnedPermit<crate::tenancy::actor::Job>>,
    ) -> Result<(), crate::tenancy::error::Failure> {
        use crate::tenancy::error::Failure;
        use bitrouter_ai::tenancy::operation::ErrorCode;
        let owner = execution
            .receipt_owner()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        if owner.identity.ordered.is_none() {
            return Ok(());
        }
        if !std::sync::Arc::ptr_eq(&self.cleanup, &owner.origin) {
            return Err(ErrorCode::Forbidden.into());
        }
        let slot = slot.ok_or_else(|| Failure::from(ErrorCode::AuthorityUnavailable))?;
        // Admission constructed the intent before registering its owner. Transfer
        // it without an intervening await or a mutable authority check: later
        // revocation stops provider sends, never cleanup of an admitted identity.
        let (reply, wait) = tokio::sync::oneshot::channel();
        slot.send(crate::tenancy::actor::Job::OrderedIntent(IntentJob {
            owner: owner.clone(),
            reply,
        }));
        let result = tokio::time::timeout_at(owner.attachment_deadline, wait)
            .await
            .map_err(|_| ErrorCode::AuthorityUnavailable)
            .and_then(|reply| reply.map_err(|_| ErrorCode::AuthorityUnavailable))
            .and_then(|result| result.map_err(|failure| failure.0.code));
        if let Err(code) = result {
            // A caller deadline or lost reply is not proof of storage failure.
            // The writer retains and classifies the original transaction.
            owner.cancel();
            return Err(code.into());
        }
        self.current()?;
        if !owner
            .intent_ready
            .load(std::sync::atomic::Ordering::Acquire)
            || owner.cancelled.is_cancelled()
            || owner.deadline.expired()
        {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        Ok(())
    }
}
