use super::*;

async fn repository() -> Result<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    git(directory.path(), &["init", "--quiet"]).await?;
    tokio::fs::write(directory.path().join("main.rs"), "fn main() {}\n").await?;
    tokio::fs::write(directory.path().join(".gitignore"), "ignored/\n").await?;
    git(directory.path(), &["add", "--all"]).await?;
    git(
        directory.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "baseline",
        ],
    )
    .await?;
    Ok(directory)
}

async fn snapshot(root: &Path) -> Result<(Artifact, Workspace)> {
    let artifact = capture(Some(root.to_path_buf()), BTreeSet::new(), BTreeSet::new()).await?;
    let workspace = Workspace::from_artifact(&artifact)?;
    Ok((artifact, workspace))
}

fn ordinary_checkout_gaps(gaps: &BTreeSet<String>) {
    #[cfg(unix)]
    assert!(gaps.is_empty(), "{gaps:?}");
    #[cfg(not(unix))]
    {
        assert!(gaps.contains("workspace_file_modes_unavailable"));
        assert!(gaps.iter().all(|gap| matches!(
            gap.as_str(),
            "workspace_file_modes_unavailable" | "workspace_git_symlinks_unavailable"
        )));
    }
}

#[tokio::test]
async fn snapshots_keep_dirty_baselines_untracked_binary_files_and_deletions() -> Result<()> {
    let directory = repository().await?;
    let root = directory.path();
    tokio::fs::write(root.join("main.rs"), "// preexisting dirty work\n").await?;
    tokio::fs::write(root.join("raw.bin"), [0u8, 255, 10, 13]).await?;
    tokio::fs::create_dir(root.join("ignored")).await?;
    tokio::fs::write(root.join("ignored/private"), "not code evidence").await?;
    let (before, original) = snapshot(root).await?;
    ordinary_checkout_gaps(&original.gaps);
    assert_eq!(
        STANDARD.decode(&original.files["main.rs"].base64)?,
        b"// preexisting dirty work\n"
    );
    assert_eq!(
        STANDARD.decode(&original.files["raw.bin"].base64)?,
        [0, 255, 10, 13]
    );
    assert!(!original.files.contains_key("ignored/private"));
    tokio::fs::remove_file(root.join("main.rs")).await?;
    tokio::fs::write(root.join("result.rs"), "// written through any tool\n").await?;
    let (after, result) = snapshot(root).await?;
    assert!(!result.files.contains_key("main.rs"));
    assert!(result.files.contains_key("result.rs"));
    assert_eq!(original.head, result.head);
    assert_ne!(before.digest, after.digest);
    git(root, &["restore", "main.rs"]).await?;
    assert_eq!(Workspace::from_artifact(&before)?, original);
    assert_eq!(Workspace::from_artifact(&after)?, result);
    Ok(())
}

#[tokio::test]
async fn linked_worktrees_have_separate_artifact_roots() -> Result<()> {
    let directory = repository().await?;
    let linked = tempfile::tempdir()?;
    let path = linked.path().join("fork");
    let path_string = path.to_str().context("worktree path")?;
    git(
        directory.path(),
        &["worktree", "add", "--detach", path_string],
    )
    .await?;
    tokio::fs::write(path.join("main.rs"), "// forked work\n").await?;
    let (_, parent) = snapshot(directory.path()).await?;
    let (_, fork) = snapshot(&path).await?;
    assert_eq!(parent.head, fork.head);
    assert_ne!(parent.repository, fork.repository);
    assert_ne!(parent.files["main.rs"], fork.files["main.rs"]);
    ordinary_checkout_gaps(&parent.gaps);
    ordinary_checkout_gaps(&fork.gaps);
    Ok(())
}

#[tokio::test]
async fn runtime_data_exclusions_and_oversized_files_are_explicit_partial_coverage() -> Result<()> {
    let directory = repository().await?;
    let runtime = directory.path().join("runtime");
    tokio::fs::create_dir(&runtime).await?;
    tokio::fs::write(runtime.join("evidence.db"), b"mutable journal").await?;
    let file = tokio::fs::File::create(directory.path().join("oversized.bin")).await?;
    file.set_len(MAX_FILE_BYTES as u64 + 1).await?;
    let artifact = capture(
        Some(directory.path().into()),
        BTreeSet::from([tokio::fs::canonicalize(&runtime).await?]),
        BTreeSet::new(),
    )
    .await?;
    let workspace = Workspace::from_artifact(&artifact)?;
    assert!(workspace.gaps.contains("workspace_runtime_data_excluded"));
    assert!(workspace.gaps.contains("workspace_file_unavailable"));
    assert!(!workspace.files.contains_key("runtime/evidence.db"));
    assert!(!workspace.files.contains_key("oversized.bin"));
    assert!(workspace.files.contains_key("main.rs"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn executable_mode_and_symlink_targets_are_preserved_without_following_links() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = repository().await?;
    let external = tempfile::tempdir()?;
    tokio::fs::write(external.path().join("secret"), "outside").await?;
    symlink(
        external.path().join("secret"),
        directory.path().join("link"),
    )?;
    tokio::fs::set_permissions(
        directory.path().join("main.rs"),
        std::fs::Permissions::from_mode(0o755),
    )
    .await?;
    let (_, workspace) = snapshot(directory.path()).await?;
    assert_eq!(workspace.files["main.rs"].mode, "100755");
    assert_eq!(workspace.files["link"].mode, "120000");
    assert_eq!(
        STANDARD.decode(&workspace.files["link"].base64)?,
        external
            .path()
            .join("secret")
            .to_str()
            .context("target")?
            .as_bytes()
    );
    Ok(())
}

#[tokio::test]
async fn absent_scope_and_non_repository_are_retained_as_uncertainty() -> Result<()> {
    let missing = capture(None, BTreeSet::new(), BTreeSet::new()).await?;
    assert!(
        Workspace::from_artifact(&missing)?
            .gaps
            .contains("workspace_scope_unavailable")
    );
    let directory = tempfile::tempdir()?;
    let (_, workspace) = snapshot(directory.path()).await?;
    assert!(workspace.gaps.contains("workspace_capture_failed"));
    assert!(workspace.files.is_empty());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn intermediate_directory_symlinks_cannot_collect_excluded_runtime_content() -> Result<()> {
    let directory = repository().await?;
    let root = directory.path();
    tokio::fs::create_dir(root.join("tracked")).await?;
    tokio::fs::write(root.join("tracked/file"), b"original source").await?;
    git(root, &["add", "tracked/file"]).await?;
    let runtime = root.join("runtime");
    tokio::fs::create_dir(&runtime).await?;
    tokio::fs::write(runtime.join("file"), b"private runtime fixture").await?;
    tokio::fs::remove_dir_all(root.join("tracked")).await?;
    std::os::unix::fs::symlink(&runtime, root.join("tracked"))?;
    let artifact = capture(
        Some(root.into()),
        BTreeSet::from([tokio::fs::canonicalize(runtime).await?]),
        BTreeSet::new(),
    )
    .await?;
    let workspace = Workspace::from_artifact(&artifact)?;
    assert!(workspace.gaps.contains("workspace_file_unavailable"));
    assert!(!workspace.files.contains_key("tracked/file"));
    assert!(!workspace.files.contains_key("runtime/file"));
    for file in workspace.files.values() {
        assert_ne!(STANDARD.decode(&file.base64)?, b"private runtime fixture");
    }
    Ok(())
}

#[tokio::test]
async fn skip_worktree_and_non_native_symlinks_cannot_claim_complete_coverage() -> Result<()> {
    let directory = repository().await?;
    git(
        directory.path(),
        &["update-index", "--skip-worktree", "main.rs"],
    )
    .await?;
    tokio::fs::remove_file(directory.path().join("main.rs")).await?;
    let (_, sparse) = snapshot(directory.path()).await?;
    assert!(
        sparse
            .gaps
            .contains("workspace_sparse_checkout_unavailable")
    );
    git(directory.path(), &["config", "core.symlinks", "false"]).await?;
    let (_, links) = snapshot(directory.path()).await?;
    assert!(links.gaps.contains("workspace_git_symlinks_unavailable"));
    Ok(())
}

#[test]
fn oversized_serialization_preserves_scope_and_gaps_without_rejecting_the_prompt() -> Result<()> {
    let root = std::env::current_dir()?;
    let mut workspace = Workspace::empty(Some(root.clone()));
    workspace.repository = Some(root);
    workspace.head = Some("0".repeat(40));
    workspace
        .gaps
        .insert("workspace_additional_directories_unavailable".into());
    let content = FileContent {
        mode: "100644".into(),
        base64: STANDARD.encode(vec![0; 1677]),
    };
    // All file, raw content and Git name-list limits are met. Escaping the
    // filenames together with base64 content still exceeds the artifact cap.
    for index in 0..10_000 {
        workspace
            .files
            .insert(format!("{index:05}{}", "\"".repeat(600)), content.clone());
    }
    assert!(workspace.files.len() < MAX_RECORDS);
    assert!(
        workspace
            .files
            .keys()
            .map(|name| name.len() + 1)
            .sum::<usize>()
            < MAX_COMMAND_BYTES
    );
    assert!(workspace.files.len() * STANDARD.decode(&content.base64)?.len() < MAX_CONTENT_BYTES);
    assert!(serde_json::to_string(&workspace)?.len() > MAX_RECORD_BYTES);
    let original_cwd = workspace.cwd.clone();
    let artifact = workspace.artifact()?;
    let partial = Workspace::from_artifact(&artifact)?;
    assert_eq!(partial.cwd, original_cwd);
    assert!(partial.files.is_empty());
    assert_eq!(
        partial.gaps,
        BTreeSet::from([
            "workspace_artifact_limit".into(),
            "workspace_additional_directories_unavailable".into()
        ])
    );
    Ok(())
}
