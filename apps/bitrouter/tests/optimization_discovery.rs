use std::fs;

use anyhow::Result;
use bitrouter::optimization::discovery::{
    GuidedWorkflow, WorkflowCandidateSource, discover_workflow_candidates, resolve_guided_workflow,
};

#[test]
fn discovers_semantic_package_scripts_with_the_project_package_manager() -> Result<()> {
    let project = tempfile::tempdir()?;
    fs::write(
        project.path().join("package.json"),
        r#"{"scripts":{"test":"vitest","eval:agent":"node eval.mjs","benchmark":"node bench.mjs"}}"#,
    )?;
    fs::write(
        project.path().join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\n",
    )?;

    let candidates = discover_workflow_candidates(project.path())?;

    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].id, "package:benchmark");
    assert_eq!(
        candidates[0].command,
        ["pnpm", "run", "benchmark"].map(String::from)
    );
    assert_eq!(candidates[1].id, "package:eval:agent");
    assert_eq!(candidates[1].source, WorkflowCandidateSource::PackageScript);
    Ok(())
}

#[cfg(unix)]
#[test]
fn discovers_executable_eval_entrypoints_but_not_generic_test_helpers() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let project = tempfile::tempdir()?;
    fs::create_dir(project.path().join("scripts"))?;
    for name in ["run-agent-eval", "test-helper"] {
        let path = project.path().join("scripts").join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n")?;
        let mut permissions = fs::metadata(&path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }

    let candidates = discover_workflow_candidates(project.path())?;

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, "executable:scripts/run-agent-eval");
    assert_eq!(
        candidates[0].command,
        ["./scripts/run-agent-eval"].map(String::from)
    );
    assert_eq!(candidates[0].source, WorkflowCandidateSource::Executable);
    Ok(())
}

#[cfg(unix)]
#[test]
fn skips_symlinked_entrypoints_and_sorts_results_deterministically() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let project = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let outside_eval = outside.path().join("agent-eval");
    fs::write(&outside_eval, "#!/bin/sh\nexit 0\n")?;
    let mut permissions = fs::metadata(&outside_eval)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&outside_eval, permissions)?;
    symlink(&outside_eval, project.path().join("agent-eval"))?;

    fs::write(
        project.path().join("package.json"),
        r#"{"scripts":{"z-eval":"true","a-bench":"true"}}"#,
    )?;

    let first = discover_workflow_candidates(project.path())?;
    let second = discover_workflow_candidates(project.path())?;

    assert_eq!(first, second);
    assert_eq!(
        first
            .iter()
            .map(|candidate| candidate.id.as_str())
            .collect::<Vec<_>>(),
        vec!["package:a-bench", "package:z-eval"]
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn skips_entrypoints_reached_through_a_symlinked_search_directory() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let project = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let outside_eval = outside.path().join("agent-eval");
    fs::write(&outside_eval, "#!/bin/sh\nexit 0\n")?;
    let mut permissions = fs::metadata(&outside_eval)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&outside_eval, permissions)?;
    symlink(outside.path(), project.path().join("scripts"))?;

    let candidates = discover_workflow_candidates(project.path())?;

    assert!(candidates.is_empty());
    Ok(())
}

#[test]
fn guided_workflow_uses_the_only_discovered_candidate() -> Result<()> {
    let project = tempfile::tempdir()?;
    fs::write(
        project.path().join("package.json"),
        r#"{"scripts":{"eval":"node eval.mjs"}}"#,
    )?;

    let guided = resolve_guided_workflow(project.path(), None, Vec::new())?;

    assert_eq!(
        guided,
        GuidedWorkflow::Resolved {
            command: ["npm", "run", "eval"].map(String::from).to_vec(),
            evidence: "package.json declares the `eval` script".into(),
        }
    );
    Ok(())
}

#[test]
fn guided_workflow_preserves_explicit_argv_and_reports_ambiguity() -> Result<()> {
    let project = tempfile::tempdir()?;
    fs::write(
        project.path().join("package.json"),
        r#"{"scripts":{"eval":"true","bench":"true"}}"#,
    )?;

    let explicit = resolve_guided_workflow(
        project.path(),
        Some("./quality-gate".into()),
        vec!["--case".into(), "agent flow".into()],
    )?;
    assert_eq!(
        explicit,
        GuidedWorkflow::Resolved {
            command: vec![
                "./quality-gate".into(),
                "--case".into(),
                "agent flow".into()
            ],
            evidence: "explicit --workflow-command argv".into(),
        }
    );

    let ambiguous = resolve_guided_workflow(project.path(), None, Vec::new())?;
    assert!(matches!(ambiguous, GuidedWorkflow::Choose(candidates) if candidates.len() == 2));
    Ok(())
}
