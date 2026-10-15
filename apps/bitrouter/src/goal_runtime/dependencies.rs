//! Current checked handoffs are supplied and frozen before dependent dispatch.
use super::*;

impl Coordinator {
    pub(super) async fn checked_dependencies(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
    ) -> Result<Vec<serde_json::Value>> {
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let mut dependencies = Vec::new();
        for id in &task.spec.dependencies {
            let upstream = goal
                .workflow
                .tasks
                .iter()
                .find(|task| &task.spec.id == id)
                .context("dependency task missing")?;
            let execution = upstream
                .executions
                .last()
                .context("dependency execution missing")?;
            let handoff = execution
                .handoff
                .as_ref()
                .context("dependency handoff missing")?;
            anyhow::ensure!(
                handoff.task_revision == upstream.requirements_revision,
                "dependency handoff is superseded"
            );
            let checks = checks::passed(&root, upstream, execution, &handoff.workspace).await?;
            let review: serde_json::Value = serde_json::from_slice(
                &crate::goal_artifacts::read(&root, &handoff.review).await?,
            )?;
            crate::goal_artifacts::read(&root, &handoff.workspace).await?;
            dependencies.push(
                serde_json::json!({"task_id":id,"task_revision":upstream.requirements_revision,
                "checked_handoff":handoff,"actual_checks":checks,"semantic_review":review}),
            );
        }
        anyhow::ensure!(
            serde_json::to_vec(&dependencies)?.len() <= 128 * 1024,
            "checked dependency evidence needs bounded context preparation"
        );
        Ok(dependencies)
    }

    pub(super) async fn supplied_dependencies(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
    ) -> Result<Vec<serde_json::Value>> {
        let expected = self.checked_dependencies(goal, task).await?;
        if expected.is_empty() {
            return Ok(expected);
        }
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let reference = execution
            .resources
            .as_ref()
            .and_then(|resources| resources.checked_dependencies.as_ref())
            .context("checked dependency reports were not supplied before dispatch")?;
        let actual: Vec<serde_json::Value> =
            serde_json::from_slice(&crate::goal_artifacts::read(&root, reference).await?)?;
        anyhow::ensure!(
            actual == expected,
            "supplied dependency evidence is stale or changed"
        );
        Ok(actual)
    }
}
