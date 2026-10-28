//! Owned Choice links share the native journal replacement and its ACK.
use super::local::{Journal, NativePort};
use crate::{
    core::session::choice::wire,
    storage::checkpoint::{CheckpointBatchExt, sha256, snapshot::context::DecisionReceipt},
    tenancy::{execution::session::Binding, resources::execution::ChoiceProfile},
};
use bitrouter_ai::{
    core::{
        checkpoint::{CheckpointPayload, DurableHead},
        protocol::{CoreError, ErrorCode},
    },
    tenancy::{
        configuration::ResourceRef,
        identity::{ExecutionId, InstallationId, RevisionId, TenantId},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

mod capacity;

const MAX_LINKS: usize = 4096;
const MAX_LINK_BYTES: usize = 8192;
const MAX_LINK_TOTAL: usize = 8 * 1024 * 1024;
const MAX_RECEIPT: usize = 1024 * 1024;
const MAX_RECEIPT_TOTAL: usize = 8 * 1024 * 1024;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub(crate) version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) seeds: Option<(
        crate::tenancy::receipts::recovery::Seed,
        crate::tenancy::receipts::recovery::Seed,
    )>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) terminal_digests: Option<(String, String)>,
    pub(crate) installation: InstallationId,
    pub(crate) tenant: TenantId,
    pub(crate) revision: RevisionId,
    pub(crate) parent: ResourceRef,
    pub(crate) child: ResourceRef,
    pub(crate) parent_execution: ExecutionId,
    pub(crate) child_execution: ExecutionId,
    pub(crate) profile_digest: String,
    pub(crate) native_digest: String,
    pub(crate) decision_id: String,
    pub(crate) step_id: String,
    pub(crate) epoch: u64,
    pub(crate) intent_digest: String,
    pub(crate) outcome_digest: Option<String>,
}

pub(crate) fn intent(receipt: &DecisionReceipt) -> Result<String, CoreError> {
    // The mutable application/result fields are not part of the immutable intent.
    let mut stable = receipt.clone();
    stable.outcome = None;
    stable.elapsed_ms = None;
    stable.stale = false;
    stable.view_ids.clear();
    digest(&stable, MAX_RECEIPT)
}
pub(crate) fn outcome(receipt: &DecisionReceipt) -> Result<Option<String>, CoreError> {
    receipt
        .outcome
        .as_ref()
        .map(|value| digest(&(value, receipt.elapsed_ms, receipt.stale), MAX_RECEIPT))
        .transpose()
}
fn digest(value: &impl Serialize, limit: usize) -> Result<String, CoreError> {
    wire::encoded(value, limit)
        .map(|bytes| sha256(&bytes))
        .map_err(|()| invalid())
}
fn invalid() -> CoreError {
    CoreError::rejected(
        ErrorCode::CheckpointConflict,
        "owned Choice journal linkage is invalid",
    )
}

impl Record {
    pub(crate) fn profile(&self, receipt: &DecisionReceipt) -> ChoiceProfile {
        ChoiceProfile {
            parent: self.parent.clone(),
            child: self.child.clone(),
            policy: receipt.policy.clone(),
            digest: self.profile_digest.clone(),
        }
    }
    fn validate(&self, binding: &Binding, receipt: &DecisionReceipt) -> Result<(), CoreError> {
        let descriptor = binding.descriptor();
        let valid_version = match (&self.seeds, self.version) {
            (None, 1) => self.terminal_digests.is_none(),
            (Some((parent, child)), 2) => {
                parent.matches(descriptor, &self.parent_execution)
                    && child.matches(descriptor, &self.child_execution)
            }
            _ => false,
        };
        if !valid_version
            || self.epoch == 0
            || self.installation != descriptor.installation
            || self.tenant != descriptor.tenant
            || self.revision != descriptor.revision
            || self.parent_execution == self.child_execution
            || self.decision_id != receipt.decision_id
            || self.step_id.is_empty()
            || self.step_id.len() > 256
            || self.intent_digest != intent(receipt)?
            || self.outcome_digest != outcome(receipt)?
            || !binding.authority().accepts_choice(&self.profile(receipt))
        {
            return Err(invalid());
        }
        let translated = wire::prepare(
            &receipt.request,
            self.child.resource_id.as_str(),
            receipt.policy.max_request_bytes,
        )
        .map_err(|_| invalid())?;
        if translated.digest != self.native_digest {
            return Err(invalid());
        }
        digest(self, MAX_LINK_BYTES)?;
        Ok(())
    }
}

fn decisions(payload: &CheckpointPayload) -> Result<BTreeMap<String, DecisionReceipt>, CoreError> {
    let Some(values) = payload
        .checkpoint
        .state
        .get("context_store")
        .and_then(|value| value.get("decisions"))
    else {
        return Ok(BTreeMap::new());
    };
    let values = values.as_object().ok_or_else(invalid)?;
    if values.len() > MAX_LINKS {
        return Err(capacity::exhausted());
    }
    let mut decoded = BTreeMap::new();
    let mut remaining = MAX_RECEIPT_TOTAL;
    for (id, value) in values {
        // Every typed allocation consumes the same complete-read budget before
        // cloning; individually small receipts cannot multiply the allowance.
        let bytes =
            wire::encoded(value, remaining.min(MAX_RECEIPT)).map_err(|()| capacity::exhausted())?;
        let actual = bytes.len();
        drop(bytes);
        let receipt: DecisionReceipt =
            serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        if id != &receipt.decision_id {
            return Err(invalid());
        }
        // Charge every pending completion before admitting the next typed receipt.
        // The 64 MiB enclosing checkpoint cannot subsidize this 8 MiB sub-budget.
        remaining = remaining
            .checked_sub(capacity::receipt(&receipt, actual)?)
            .ok_or_else(capacity::exhausted)?;
        decoded.insert(id.clone(), receipt);
    }
    Ok(decoded)
}

pub(in crate::harness::resources::managed) fn validate(
    journal: &Journal,
    binding: Option<&Binding>,
) -> Result<(), CoreError> {
    let Some(binding) = binding else {
        return if journal.owned_choices.is_empty() {
            Ok(())
        } else {
            Err(invalid())
        };
    };
    if binding.authority().has_choices()
        != journal
            .manifest
            .required_features
            .iter()
            .any(|value| value == crate::storage::checkpoint::snapshot::context::FEATURE)
    {
        return Err(invalid());
    }
    let Some(batch) = &journal.checkpoint else {
        return if journal.owned_choices.is_empty() {
            Ok(())
        } else {
            Err(invalid())
        };
    };
    let payload = batch.decode(&journal.limits)?;
    let receipts = decisions(&payload)?;
    validate_links(&journal.owned_choices, binding, &receipts)
}

fn validate_links(
    links: &BTreeMap<String, Record>,
    binding: &Binding,
    receipts: &BTreeMap<String, DecisionReceipt>,
) -> Result<(), CoreError> {
    if links.len() > MAX_LINKS || links.len() != receipts.len() {
        return Err(invalid());
    }
    capacity::links(links)?;
    let mut executions = std::collections::BTreeSet::new();
    for (id, record) in links {
        let receipt = receipts.get(id).ok_or_else(invalid)?;
        record.validate(binding, receipt)?;
        if !executions.insert(&record.child_execution) {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) fn apply(
    journal: &Journal,
    payload: &CheckpointPayload,
    binding: Option<&Binding>,
) -> Result<BTreeMap<String, Record>, CoreError> {
    let Some(binding) = binding else {
        if !journal.owned_choices.is_empty()
            || payload
                .events
                .iter()
                .any(|event| event.payload.get("owned_choice").is_some())
        {
            return Err(invalid());
        }
        return Ok(BTreeMap::new());
    };
    let receipts = decisions(payload)?;
    let mut links = journal.owned_choices.clone();
    for event in &payload.events {
        if event.kind != "context.decision.intent" {
            if event.payload.get("owned_choice").is_some() {
                return Err(invalid());
            }
            continue;
        }
        if links.len() >= MAX_LINKS {
            return Err(capacity::exhausted());
        }
        let value = event.payload.get("owned_choice").ok_or_else(invalid)?;
        wire::encoded(value, MAX_LINK_BYTES).map_err(|()| invalid())?;
        let record: Record = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        let receipt = receipts.get(&record.decision_id).ok_or_else(invalid)?;
        if record.epoch != payload.identity.execution_epoch
            || record.outcome_digest.is_some()
            || event
                .payload
                .get("decision_id")
                .and_then(serde_json::Value::as_str)
                != Some(record.decision_id.as_str())
            || event.agent_id.as_deref() != Some(receipt.agent_id.as_str())
            || event.run_id.as_deref() != Some(receipt.run_id.as_str())
            || receipt.intent_state_revision != payload.checkpoint.state_revision
        {
            return Err(invalid());
        }
        let agent = &payload.checkpoint.state["agents"][&receipt.agent_id];
        let turn = &agent["turn"];
        if turn["agent_turn_id"].as_str() != Some(receipt.task_id.as_str())
            || !turn["steps"].as_array().is_some_and(|steps| {
                steps
                    .iter()
                    .any(|step| step["step_id"].as_str() == Some(record.step_id.as_str()))
            })
        {
            return Err(invalid());
        }
        record.validate(binding, receipt)?;
        if links.insert(record.decision_id.clone(), record).is_some() {
            return Err(invalid());
        }
    }
    for (id, record) in &mut links {
        let receipt = receipts.get(id).ok_or_else(invalid)?;
        let current = outcome(receipt)?;
        if current != record.outcome_digest {
            let completion = payload.events.iter().any(|event| {
                event.kind == "context.decision.outcome"
                    && event.payload["decision_id"].as_str() == Some(id.as_str())
            });
            let restored = payload.identity.execution_epoch > record.epoch && payload.events.iter().any(|event| event.kind == "session.restored" && event.payload["previous_owner_stopped"] == true)
                && receipt.outcome.as_ref().is_some_and(|value| matches!(value, Err(error) if error.kind == crate::gateway::model::decision::types::DecisionFailure::Interrupted && error.may_have_run && error.usage.is_none()));
            if record.outcome_digest.is_some() || current.is_none() || (!completion && !restored) {
                return Err(invalid());
            }
            record.outcome_digest = current;
        }
    }
    validate_links(&links, binding, &receipts)?;
    Ok(links)
}

/// A weak view of the same acknowledged row state; no copied index or authority.
#[derive(Clone)]
pub(crate) struct JournalView {
    port: Weak<NativePort>,
    store: Weak<crate::managed_store::owned::Store>,
}
impl JournalView {
    pub(in crate::harness::resources::managed) fn new(
        port: &Arc<NativePort>,
        store: &Arc<crate::managed_store::owned::Store>,
    ) -> Self {
        Self {
            port: Arc::downgrade(port),
            store: Arc::downgrade(store),
        }
    }
    /// Called with the actual Core commit lock before proposing a batch or writing
    /// artifacts. Ordinary capacity refusal must stay NotCommitted; after submit,
    /// the existing Core protocol deliberately treats any missing ACK as unknown.
    pub(crate) async fn preflight(
        &self,
        payload: &CheckpointPayload,
        head: &DurableHead,
    ) -> Result<(), CoreError> {
        let port = self.port.upgrade().ok_or_else(invalid)?;
        port.preflight_choices(payload, head).await
    }

    pub(crate) async fn readback(&self) -> Result<DurableHead, CoreError> {
        let port = self.port.upgrade().ok_or_else(invalid)?;
        let store = self.store.upgrade().ok_or_else(invalid)?;
        port.reconcile_owned(&store).await?;
        Ok(port.state.lock().await.journal.head.clone())
    }
    pub(crate) async fn preflight_experiment(
        &self,
        projection: &crate::core::session::native_capacity::Projection,
        head: &DurableHead,
    ) -> Result<(), CoreError> {
        let port = self.port.upgrade().ok_or_else(invalid)?;
        let state = port.state.lock().await;
        if port.failed.load(std::sync::atomic::Ordering::Acquire) || &state.journal.head != head {
            return Err(invalid());
        }
        super::model_capacity::check(&state.journal, projection)
    }

    pub(crate) async fn checked_model_step(
        &self,
        binding: &Arc<Binding>,
        agent: &str,
        step: &str,
    ) -> Result<Arc<super::model::Origin>, CoreError> {
        let port = self.port.upgrade().ok_or_else(invalid)?;
        let state = port.state.lock().await;
        if port.failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(invalid());
        }
        super::model::origin(&state.journal, binding, agent, step)
    }

    pub(crate) async fn choice(
        &self,
        id: &str,
        head: &DurableHead,
    ) -> Result<Option<Record>, CoreError> {
        let port = self.port.upgrade().ok_or_else(invalid)?;
        let state = port.state.lock().await;
        if &state.journal.head != head || port.failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(invalid());
        }
        Ok(state.journal.owned_choices.get(id).cloned())
    }
}
