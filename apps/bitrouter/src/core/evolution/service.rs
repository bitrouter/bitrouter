//! Core-owned mutation of the durable policy-block control document.

use crate::gateway::routing::blocks::control::{BlockDefinition, ControlState, EvolutionMode};
use crate::gateway::routing::blocks::store::{BlockPolicyStore, CONTROL_KEY, CONTROL_KIND};
use anyhow::{Result, ensure};
use sea_orm::TransactionTrait;
pub mod revisions;

impl BlockPolicyStore {
    async fn initialize_control(&self) -> Result<()> {
        self.store
            .initialize(CONTROL_KIND, CONTROL_KEY, None, &ControlState::default())
            .await?;
        Ok(())
    }
    pub async fn set_mode(
        &self,
        mode: EvolutionMode,
        judge_model: Option<String>,
    ) -> Result<ControlState> {
        self.initialize_control().await?;
        let tx = self.store.db.begin().await?;
        let (row, mut state): (_, ControlState) =
            self.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
        state.set_mode(mode, judge_model)?;
        self.store.save(&tx, row, &state).await?;
        tx.commit().await?;
        Ok(state)
    }
    /// Changing the evaluator must preserve the mode current at the write,
    /// including an Off selected by another local client.
    pub async fn set_judge_model(&self, model: String) -> Result<ControlState> {
        self.initialize_control().await?;
        let tx = self.store.db.begin().await?;
        let (row, mut state): (_, ControlState) =
            self.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
        state.set_mode(state.mode, Some(model))?;
        self.store.save(&tx, row, &state).await?;
        tx.commit().await?;
        Ok(state)
    }
    /// An operator withdrawal is serialized with admission and publication.
    /// Keep assignments, evaluations, unrelated blocks and mode settings intact.
    pub async fn restore(
        &self,
        request: &super::control::restoration::RestoreRequest,
    ) -> Result<crate::gateway::routing::blocks::control::Publication> {
        self.initialize_control().await?;
        let tx = self.store.db.begin().await?;
        let (row, mut state): (_, ControlState) =
            self.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
        let previous_generation = state.generation;
        let publication = state.restore(request)?;
        if state.generation != previous_generation {
            self.store.save(&tx, row, &state).await?;
        }
        tx.commit().await?;
        Ok(publication)
    }
    /// The routing adapter supplies a validated, nonsecret dependency digest of
    /// the complete baseline and candidate routes. This primitive never accepts
    /// an unvalidated route directly from a model-produced rubric.
    pub async fn register(
        &self,
        definition: BlockDefinition,
        routing_config_digest: String,
    ) -> Result<ControlState> {
        self.register_checked(definition, routing_config_digest, None, || Ok(()))
            .await
    }
    /// Reviewed UI registration uses a control/mode fence and an idempotent
    /// block identity. A lost response can be retried without enabling a mode,
    /// resetting learned evidence or creating a second experiment.
    pub(crate) async fn register_checked(
        &self,
        definition: BlockDefinition,
        routing_config_digest: String,
        expected_control: Option<(u64, u64)>,
        precondition: impl FnOnce() -> Result<()>,
    ) -> Result<ControlState> {
        ensure!(
            !routing_config_digest.is_empty(),
            "routing dependency digest is required"
        );
        self.initialize_control().await?;
        let tx = self.store.db.begin().await?;
        let (row, mut state): (_, ControlState) =
            self.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
        if let Some((generation, mode_epoch)) = expected_control {
            if state
                .registration(&definition, &routing_config_digest, None)?
                .is_some()
            {
                tx.commit().await?;
                return Ok(state);
            }
            ensure!(
                state.generation == generation && state.mode_epoch == mode_epoch,
                "evolution settings changed since preview; review the candidate again"
            );
        }
        precondition()?;
        state.register(definition, routing_config_digest)?;
        self.store.save(&tx, row, &state).await?;
        tx.commit().await?;
        Ok(state)
    }
}

#[cfg(test)]
mod tests;
