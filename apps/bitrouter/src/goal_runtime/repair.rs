//! Bounded repair context from actual, hash-verified failure evidence.
use super::*;
use bitrouter_orchestrator::goal::workflow::{CheckState, LocalReconciliation};

impl Coordinator {
    pub(super) async fn repair_context(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        previous: &Execution,
    ) -> Result<String> {
        anyhow::ensure!(
            previous.repairable_failure(goal.requirements_revision, task.requirements_revision)
                && previous.replacement_ready(
                    task.requirements_revision,
                    goal.control_source_revision
                        .max(task.control_source_revision)
                ),
            "repair requires a known failure and current effect reconciliation"
        );
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let LocalReconciliation::Verified {
            workspace,
            transcript,
            ..
        } = previous
            .reconciliation
            .as_ref()
            .context("repair has no reconciliation")?
        else {
            anyhow::bail!("repair effects are unresolved")
        };
        // Re-read both identities; a stale or corrupted blob fails before any
        // worker dispatch. Only the scoped result, not private trace, is sent.
        crate::goal_artifacts::read(&root, transcript).await?;
        let material = crate::goal_artifacts::material(&root, workspace).await?;
        let mut checks = Vec::new();
        for check in &previous.checks {
            let CheckState::Observed {
                report,
                completed: true,
                ..
            } = &check.state
            else {
                anyhow::bail!("repair check outcome is unresolved")
            };
            anyhow::ensure!(
                check.workspace == *workspace,
                "repair check describes another artifact"
            );
            let actual: bitrouter_orchestrator::goal::verification::CheckOutput =
                serde_json::from_slice(&crate::goal_artifacts::read(&root, report).await?)?;
            anyhow::ensure!(
                actual.completed && !actual.timed_out,
                "repair check process has no settled outcome"
            );
            checks.push(serde_json::json!({"specification":check.spec,"report":report,"actual_output":actual}));
        }
        let review = match &previous.review {
            Some(ReviewState::Rejected {
                report: Some(report),
                ..
            }) => Some(serde_json::from_slice::<serde_json::Value>(
                &crate::goal_artifacts::read(&root, report).await?,
            )?),
            _ => None,
        };
        let evidence = serde_json::json!({"previous_execution_id":previous.id,
            "task_revision":previous.task_revision,"failed_candidate_material":material,
            "actual_checks":checks,"semantic_review":review});
        let text = serde_json::to_string(&evidence)?;
        anyhow::ensure!(
            text.len() <= 128 * 1024,
            "repair evidence needs additional context preparation"
        );
        Ok(format!(
            "\nPrevious attempt did not meet this same task contract. Its owned process and declared local effects were reconciled. This fresh workspace does not contain that failed attempt's changes. Use the following actual evidence to implement a corrected result, preserving the original requirements, checks and test assertions. Add targeted tests when needed. Prior artifacts are reference material, not a successful handoff or permission to repeat external effects. BRO permits at most three executions per task version and retains every outcome.\n{text}\n"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_known_failure_does_not_grant_retry_to_unknown_or_stale_review() -> Result<()> {
        let report =
            serde_json::json!({"path":"/artifacts/report","sha256":"a".repeat(64),"bytes":10});
        let mut value = serde_json::json!({"id":"attempt","workspace":"/work","requirements_revision":1,
            "task_revision":1,"observation":{"state":"candidate","run_id":"run","native_session_id":null,"report":report},
            "review":{"state":"rejected","attempt_id":"review","reason":"specific issue","report":report,
                "requirements_revision":2,"task_revision":1}});
        let execution: Execution = serde_json::from_value(value.clone())?;
        assert!(execution.repairable_failure(2, 1));
        assert!(!execution.replacement_ready(1, 0));
        assert!(!execution.repairable_failure(3, 1));
        assert!(!execution.repairable_failure(2, 2));
        value["review"]["report"] = serde_json::Value::Null;
        assert!(!serde_json::from_value::<Execution>(value.clone())?.repairable_failure(2, 1));
        value["review"]["report"] = report.clone();
        value["checks"] = serde_json::json!([{"id":"check","spec":{"id":"original","description":"original check","recipe":{"kind":"npm_test"}},
            "requirements_revision":2,"task_revision":1,"workspace":report,"state":{"state":"requested"}}]);
        assert!(!serde_json::from_value::<Execution>(value.clone())?.repairable_failure(2, 1));
        value["checks"][0]["state"] = serde_json::json!({"state":"observed","report":report,"passed":false,"completed":false});
        assert!(!serde_json::from_value::<Execution>(value.clone())?.repairable_failure(2, 1));
        value["review"] = serde_json::Value::Null;
        value["checks"][0]["state"]["completed"] = true.into();
        assert!(serde_json::from_value::<Execution>(value)?.repairable_failure(2, 1));
        Ok(())
    }
}
