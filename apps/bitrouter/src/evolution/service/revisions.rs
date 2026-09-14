//! Fenced registration of the next experiment for an existing policy block.

use super::*;

pub(crate) struct RevisionRegistration {
    pub definition: BlockDefinition,
    pub routing_digest: String,
    pub predecessor: String,
    pub reset_to_configured: bool,
    pub expected_control: (u64, u64),
}

impl EvolutionService {
    pub(crate) async fn revise_checked(
        &self,
        request: RevisionRegistration,
        precondition: impl FnOnce() -> Result<()>,
    ) -> Result<ControlState> {
        ensure!(
            !request.routing_digest.is_empty(),
            "routing dependency digest is required"
        );
        let tx = self.store.db.begin().await?;
        let (row, mut state): (_, ControlState) =
            self.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
        if state
            .registration(
                &request.definition,
                &request.routing_digest,
                Some(&request.predecessor),
            )?
            .is_some()
        {
            tx.commit().await?;
            return Ok(state);
        }
        ensure!(
            (state.generation, state.mode_epoch) == request.expected_control,
            "evolution settings changed; review the experiment revision again"
        );
        precondition()?;
        state.revise(
            request.definition,
            request.routing_digest,
            &request.predecessor,
            request.reset_to_configured,
        )?;
        self.store.save(&tx, row, &state).await?;
        tx.commit().await?;
        Ok(state)
    }
}
