//! Sequential experiments retain their evidence and baseline provenance.

use super::*;
use anyhow::Context;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedExperiment {
    pub block: BlockState,
    pub superseded_by: String,
    pub retired_at: String,
}

impl ControlState {
    pub fn experiment(&self, block_id: &str, experiment_id: Option<&str>) -> Option<&BlockState> {
        let current = self.blocks.get(block_id)?;
        match experiment_id {
            None => Some(current),
            Some(id) if current.experiment_id == id => Some(current),
            Some(id) => self
                .archived_experiments
                .get(id)
                .map(|archive| &archive.block)
                .filter(|block| block.definition.block_id == block_id),
        }
    }

    pub fn original_experiment(&self, block_id: &str) -> Result<&BlockState> {
        let mut block = self.blocks.get(block_id).context("unknown policy block")?;
        let mut seen = BTreeSet::new();
        while let Some(previous) = &block.predecessor_experiment_id {
            ensure!(seen.insert(previous), "cyclic experiment history");
            block = self
                .experiment(block_id, Some(previous))
                .context("experiment predecessor missing")?;
        }
        Ok(block)
    }

    pub fn baseline_valid(&self, block: &BlockState) -> bool {
        block.baseline_ancestry.iter().all(|id| {
            self.experiment(&block.definition.block_id, Some(id))
                .is_some_and(|ancestor| {
                    ancestor.status == BlockStatus::Adopted
                        && self.external_dependencies_match(ancestor)
                })
        })
    }

    /// Return the last still-supported baseline. A withdrawn ancestor also
    /// withdraws its descendants; its own baseline predates the failed adoption.
    pub fn baseline_source<'a>(
        &'a self,
        block: &'a BlockState,
    ) -> Result<(&'a BlockState, Vec<String>)> {
        let mut ancestry = Vec::new();
        for id in &block.baseline_ancestry {
            let ancestor = self
                .experiment(&block.definition.block_id, Some(id))
                .context("baseline ancestor missing")?;
            if ancestor.status != BlockStatus::Adopted
                || !self.external_dependencies_match(ancestor)
            {
                return Ok((ancestor, ancestry));
            }
            ancestry.push(id.clone());
        }
        Ok((block, ancestry))
    }
}
