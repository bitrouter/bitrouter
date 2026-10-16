//! Carry known local work across worker replacement without replaying tools.
use super::*;
use bitrouter_orchestrator::goal::resources::WorkerHandoff;

impl Coordinator {
    pub(super) async fn prepare_worker_handoff(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        workspace: &Path,
        base_commit: &str,
        dependencies: &[serde_json::Value],
    ) -> Result<Option<WorkerHandoff>> {
        let Some(handoff) =
            task.worker_handoff(task.executions.len(), goal.control_source_revision)?
        else {
            return Ok(None);
        };
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        // Dependency versions are data input to the old workspace. An old
        // dependency closure cannot silently overwrite the current one.
        if !dependencies.is_empty() {
            let source = task
                .executions
                .iter()
                .find(|execution| execution.id == handoff.execution_id)
                .or_else(|| {
                    task.executions.iter().rev().find(|execution| {
                        execution.resources.as_ref().is_some_and(|resources| {
                            resources.worker_handoff.as_ref() == Some(&handoff)
                        })
                    })
                })
                .context("worker handoff source input is unavailable")?;
            let reference = source
                .resources
                .as_ref()
                .and_then(|resources| resources.checked_dependencies.as_ref())
                .context("old worker dependency versions are unconfirmed")?;
            let supplied: Vec<serde_json::Value> =
                serde_json::from_slice(&crate::goal_artifacts::read(&root, reference).await?)?;
            anyhow::ensure!(
                supplied == dependencies,
                "worker handoff dependency versions changed; reconciliation is required"
            );
        }
        crate::goal_artifacts::read(&root, &handoff.transcript).await?;
        crate::goal_artifacts::apply(&root, &handoff.workspace, workspace, base_commit).await?;
        Ok(Some(handoff))
    }
}
