//! Fenced registration of the next experiment for an existing policy block.

use super::*;

pub(crate) struct RevisionRegistration {
    pub definition: BlockDefinition,
    pub routing_digest: String,
    pub predecessor: String,
    pub reset_to_configured: bool,
    pub expected_control: (u64, u64),
}

impl BlockPolicyStore {
    pub(crate) async fn revise_checked(
        &self,
        request: RevisionRegistration,
        precondition: impl FnOnce() -> Result<()>,
    ) -> Result<ControlState> {
        ensure!(
            !request.routing_digest.is_empty(),
            "routing dependency digest is required"
        );
        let (mut write, tx) = self.store.begin_write(false).await?;
        let result = async {
            let (row, mut state): (_, ControlState) = self
                .store
                .lock_charged(&mut write, &tx, CONTROL_KIND, CONTROL_KEY)
                .await?;
            if state
                .registration(
                    &request.definition,
                    &request.routing_digest,
                    Some(&request.predecessor),
                )?
                .is_some()
            {
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
            self.store
                .save_charged(&mut write, &tx, row, &state)
                .await?;
            Ok(state)
        }
        .await;
        write.finish(tx, result).await
    }
}
