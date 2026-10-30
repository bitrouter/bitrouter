//! One explicitly retained OAuth lineage. No ambient accounts or new lifecycle.
pub(in crate::tenancy) mod adapter;
pub(in crate::tenancy) mod crypto;
pub(in crate::tenancy) mod exchange;
pub(in crate::tenancy) mod profile;
pub(in crate::tenancy) mod refresh;
pub(in crate::tenancy) mod storage;

use bitrouter_ai::tenancy::{configuration::ResourceRef, operation::ErrorCode};
use std::{collections::BTreeMap, sync::RwLock};
use storage::Metadata;

/// Bounded, nonsecret generation stamps; publication shares the authority gate.
#[derive(Default)]
pub(in crate::tenancy) struct State {
    rows: RwLock<BTreeMap<String, Metadata>>,
}
impl State {
    pub fn initialize(&self, rows: Vec<Metadata>) -> Result<(), ErrorCode> {
        if rows.len() > profile::MAX_SLOTS {
            return Err(ErrorCode::CapacityExceeded);
        }
        let mut map = BTreeMap::new();
        let mut bytes = 0usize;
        for metadata in rows {
            metadata
                .validate()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            bytes = bytes
                .checked_add(
                    serde_json::to_vec(&metadata)
                        .map_err(|_| ErrorCode::AuthorityUnavailable)?
                        .len(),
                )
                .ok_or(ErrorCode::CapacityExceeded)?;
            if bytes > profile::METADATA_BYTES
                || map.insert(metadata.slot.clone(), metadata).is_some()
            {
                return Err(ErrorCode::CapacityExceeded);
            }
        }
        *self
            .rows
            .write()
            .map_err(|_| ErrorCode::AuthorityUnavailable)? = map;
        Ok(())
    }
    pub fn get(
        &self,
        installation: &bitrouter_ai::tenancy::identity::InstallationId,
        reference: &ResourceRef,
    ) -> Result<Option<Metadata>, ErrorCode> {
        let slot =
            storage::slot(installation, reference).map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let rows = self
            .rows
            .read()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let row = rows.get(&slot);
        if row.is_some_and(|row| row.reference != *reference || &row.installation != installation) {
            return Err(ErrorCode::CapabilityMismatch);
        }
        Ok(row.cloned())
    }
    pub fn publish(&self, old: &Metadata, new: Metadata) -> Result<(), ErrorCode> {
        let mut rows = self
            .rows
            .write()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if rows.get(&old.slot) != Some(old) {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        rows.insert(new.slot.clone(), new);
        Ok(())
    }
}

/// Minted by the actual selected adapter; exact request binding stays private.
#[derive(Clone)]
pub(crate) struct ModelStamp {
    pub(in crate::tenancy) metadata: Metadata,
    pub(in crate::tenancy) execution: bitrouter_ai::tenancy::identity::ExecutionId,
}
impl ModelStamp {
    pub(in crate::tenancy) fn matches(
        &self,
        execution: &super::execution::TenantExecution,
        metadata: &Metadata,
    ) -> bool {
        &self.execution == execution.id()
            && self.metadata == *metadata
            && matches!(metadata.state, storage::State::Ready)
    }
}
