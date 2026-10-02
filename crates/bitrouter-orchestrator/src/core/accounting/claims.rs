//! Immutable monetary observations, correlated with durable request ownership.

use super::work::{CostWorkKind, RunCostWork};
use crate::core::checkpoint::sha256;
use crate::core::protocol::{CoreError, ErrorCode, validate_id};
use bitrouter_sdk::language_model::native_accounting::{
    NativeCostBasis, NativeCostObservation, NativeCostScope,
};
use std::collections::{BTreeMap, BTreeSet};

/// A source's bill belongs to one request and run across every evidence basis.
/// Retired runs retain ownership even after the scheduler replaces their turns.
pub(crate) fn validate_bill_ownership(
    ledgers: &BTreeMap<String, RunCostWork>,
    run_id: &str,
    observations: &[NativeCostObservation],
) -> Result<(), CoreError> {
    let mut owners = BTreeMap::new();
    for (owner_run, ledger) in ledgers {
        for claim in ledger.charges.values() {
            owners.insert(
                (&claim.source, &claim.bill_id),
                (owner_run.as_str(), &claim.request_id),
            );
        }
    }
    for claim in observations
        .iter()
        .flat_map(|observation| &observation.claims)
    {
        if owners
            .insert((&claim.source, &claim.bill_id), (run_id, &claim.request_id))
            .is_some_and(|owner| owner != (run_id, &claim.request_id))
        {
            return Err(CoreError::rejected(
                ErrorCode::OperationConflict,
                "monetary bill belongs to another request or run",
            ));
        }
    }
    Ok(())
}

pub(crate) fn request_ids(ledger: &RunCostWork) -> BTreeSet<String> {
    ledger
        .work
        .values()
        .filter(|work| work.kind == CostWorkKind::ProviderAttempt)
        .filter_map(|work| work.request_id.clone())
        .collect()
}

pub(crate) fn apply(
    ledger: &mut RunCostWork,
    requested: &BTreeSet<String>,
    observations: &[NativeCostObservation],
) -> Result<(), CoreError> {
    let fail = || {
        CoreError::rejected(
            ErrorCode::OperationConflict,
            "monetary evidence conflicts with request ownership or prior evidence",
        )
    };
    if observations.len() != requested.len() {
        return Err(fail());
    }
    let mut seen = BTreeSet::new();
    for observation in observations {
        if !requested.contains(&observation.request_id)
            || !seen.insert(&observation.request_id)
            || observation.claims.len() > 8
        {
            return Err(fail());
        }
        if let Some(reason) = &observation.unknown_reason {
            validate_id(reason)?;
            ledger
                .charge_unknown
                .insert(observation.request_id.clone(), reason.clone());
        } else {
            ledger.charge_unknown.remove(&observation.request_id);
        }
        if observation.claims.is_empty() && observation.unknown_reason.is_none() {
            return Err(fail());
        }
        for claim in &observation.claims {
            validate_id(&claim.source)?;
            validate_id(&claim.bill_id)?;
            if claim.request_id != observation.request_id
                || claim
                    .evidence_sha256
                    .strip_prefix("sha256:")
                    .is_none_or(|digest| {
                        digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit())
                    })
                || claim.provider.len() > 256
                || claim.model.len() > 256
                || matches!(
                    claim.basis,
                    NativeCostBasis::Reported | NativeCostBasis::Reconciled
                ) && claim.scope != NativeCostScope::RequestBill
            {
                return Err(fail());
            }
            let identity = serde_json::to_vec(&(&claim.source, &claim.bill_id, claim.basis))
                .map_err(|_| fail())?;
            let key = sha256(&identity);
            if ledger.charges.get(&key).is_some_and(|prior| prior != claim) {
                return Err(fail());
            }
            ledger.charges.insert(key, claim.clone());
        }
    }
    Ok(())
}
