//! Read the exact checked artifacts and declared local effects used at completion.
use super::*;

pub(super) async fn task(
    coordinator: &Coordinator,
    goal: &GoalSnapshot,
    task: &Task,
) -> Result<serde_json::Value> {
    let root = crate::goal_artifacts::root(&coordinator.workspaces, &goal.id);
    let execution = task.executions.last().context("missing completed task")?;
    let handoff = execution.handoff.as_ref().context("missing task handoff")?;
    let review: serde_json::Value =
        serde_json::from_slice(&crate::goal_artifacts::read(&root, &handoff.review).await?)?;
    let material = crate::goal_artifacts::material(&root, &handoff.workspace).await?;
    let verifier_inputs = super::super::review_inputs::collect(
        &root,
        &execution.workspace,
        execution
            .base_commit
            .as_deref()
            .context("handoff baseline missing")?,
        &task.spec.checks,
    )
    .await?;
    let checks = super::super::checks::passed(&root, task, execution, &handoff.workspace).await?;
    let local_audit = match coordinator
        .local_recovery_evidence(goal, task, execution)
        .await
    {
        Ok((workspace, transcript)) if workspace == handoff.workspace => {
            let audit: serde_json::Value =
                serde_json::from_slice(&crate::goal_artifacts::read(&root, &transcript).await?)?;
            serde_json::json!({"status":"observed","evidence":transcript,"workspace":workspace,"approved_local_tools":audit["approved_local_tools"],"permission_submissions":audit["permission_submissions"],"failed_local_tools_without_requests":audit["failed_local_tools_without_requests"],"scope":"Owned process termination, complete original protocol history, actual resolutions for offered permissions and finished declared workspace read/edit/search were validated. Separate failed local calls without permission requests may have partial local effects captured in this stable workspace; they are neither user consent nor proof of no effects. This audits declared protocol effects only; ambient initialization, undeclared effects, external bills and actual provider identity remain unreported. It is not a global machine sandbox attestation."})
        }
        Ok(_) => {
            serde_json::json!({"status":"unavailable","reason":"stopped task workspace differs from its checked handoff"})
        }
        Err(error) => serde_json::json!({"status":"unavailable","reason":bounded_error(&error)}),
    };
    let runtime = super::super::runtime_evidence::collect(coordinator, goal, task, execution).await;
    let historical = super::super::runtime_evidence::historical(coordinator, goal, task).await;
    Ok(
        serde_json::json!({"id":format!("task:{}",task.spec.id),"task":task.spec,"artifact":handoff.workspace,"material":material,"original_verifier_inputs":verifier_inputs,"checked_handoff":handoff,"actual_checks":checks,"review":review,"review_request_id":handoff.review_request_id,"execution_stop":execution.stop,"runtime_observations":runtime,"historical_runtime_observations":historical,"declared_local_effect_audit":local_audit}),
    )
}

pub(super) async fn source(goal: &GoalSnapshot) -> Result<serde_json::Value> {
    let execution = goal
        .workflow
        .tasks
        .first()
        .and_then(|task| task.executions.last())
        .context("missing source baseline execution")?;
    let base = execution
        .base_commit
        .as_deref()
        .context("missing frozen source baseline")?;
    let original = tokio::fs::canonicalize(&goal.workspace).await?;
    let (stable, diff_bytes, extra) = compare_source(&execution.workspace, base, &original).await?;
    Ok(
        serde_json::json!({"status":"observed","source_workspace":original,"baseline_execution":execution.id,"baseline_commit":base,"stable_at_observation":stable,"git_visible_source_matches_frozen_input":stable && diff_bytes == 0 && extra==0,"diff_bytes":diff_bytes,"additional_nonignored_files":extra,"scope":"Actual read-only comparison of current Git-visible source files against the frozen isolated input commit, including initially nonignored untracked files in that commit. Ignored files, external systems and changes outside this observation remain unverified. This observation does not modify the original source or index."}),
    )
}

async fn compare_source(
    workspace: &Path,
    base: &str,
    original: &Path,
) -> Result<(bool, usize, usize)> {
    let tree = git(
        workspace,
        &[
            "ls-tree".as_ref(),
            "-r".as_ref(),
            "-z".as_ref(),
            "--name-only".as_ref(),
            base.as_ref(),
        ],
    )
    .await?
    .stdout;
    let paths: BTreeSet<_> = tree
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    let diff_args = [
        "--work-tree".as_ref(),
        original.as_os_str(),
        "diff".as_ref(),
        "--binary".as_ref(),
        "--no-ext-diff".as_ref(),
        "--no-textconv".as_ref(),
        base.as_ref(),
        "--".as_ref(),
    ];
    let untracked_args = [
        "ls-files".as_ref(),
        "--others".as_ref(),
        "--exclude-standard".as_ref(),
        "-z".as_ref(),
    ];
    let diff = git(workspace, &diff_args).await?.stdout;
    let untracked = git(original, &untracked_args).await?.stdout;
    let extra = untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty() && !paths.contains(path))
        .count();
    let stable = diff == git(workspace, &diff_args).await?.stdout
        && untracked == git(original, &untracked_args).await?.stdout;
    Ok((stable, diff.len(), extra))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn source_observation_uses_frozen_dirty_input_and_detects_later_changes() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let original = temporary.path().join("original");
        let worker = temporary.path().join("worker");
        tokio::fs::create_dir_all(&original).await?;
        git(&original, &["init".as_ref(), "-q".as_ref()]).await?;
        tokio::fs::write(original.join("tracked.txt"), "committed\n").await?;
        git(&original, &["add".as_ref(), "tracked.txt".as_ref()]).await?;
        git(
            &original,
            &[
                "-c".as_ref(),
                "user.name=Fixture".as_ref(),
                "-c".as_ref(),
                "user.email=fixture@example.invalid".as_ref(),
                "commit".as_ref(),
                "-qm".as_ref(),
                "fixture".as_ref(),
            ],
        )
        .await?;
        tokio::fs::write(original.join("tracked.txt"), "dirty before Goal\n").await?;
        tokio::fs::write(original.join("untracked.txt"), "included original input\n").await?;
        let base = prepare_workspace(&original, &worker, "source-observation-fixture", 1).await?;
        tokio::fs::write(worker.join("worker-result.txt"), "isolated output\n").await?;
        git(&worker, &["add".as_ref(), "worker-result.txt".as_ref()]).await?;
        assert_eq!(
            compare_source(&worker, &base, &original).await?,
            (true, 0, 0)
        );
        tokio::fs::write(original.join("tracked.txt"), "changed later\n").await?;
        assert!(compare_source(&worker, &base, &original).await?.1 > 0);
        tokio::fs::write(original.join("tracked.txt"), "dirty before Goal\n").await?;
        tokio::fs::write(original.join("unexpected.txt"), "new source file\n").await?;
        assert_eq!(
            compare_source(&worker, &base, &original).await?,
            (true, 0, 1)
        );
        tokio::fs::remove_file(original.join("unexpected.txt")).await?;
        tokio::fs::remove_file(original.join("untracked.txt")).await?;
        assert!(compare_source(&worker, &base, &original).await?.1 > 0);
        Ok(())
    }
}
