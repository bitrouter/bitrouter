use super::*;

async fn decision(root: &Path, value: serde_json::Value) -> Result<PermissionDecision> {
    Ok(PermissionDecision {
        id: "decision".into(),
        execution_id: "execution".into(),
        run_id: "run".into(),
        permission_id: "permission".into(),
        requirements_revision: 1,
        task_revision: 1,
        request: crate::goal_artifacts::put(root, &serde_json::to_vec(&value)?).await?,
        options: vec![
            PermissionOption {
                id: "actual-once".into(),
                label: "Allow once".into(),
                allows: true,
                persistent: false,
            },
            PermissionOption {
                id: "actual-always".into(),
                label: "Always allow".into(),
                allows: true,
                persistent: true,
            },
        ],
        state: PermissionState::Pending,
    })
}

#[tokio::test]
async fn local_file_policy_uses_declared_kind_and_paths_without_granting_execute() -> Result<()> {
    let root = tempfile::tempdir()?;
    let artifacts = root.path().join("artifacts");
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await?;
    for (kind, path, expected) in [
        ("edit", "new.txt", Some("actual-once")),
        ("read", "../outside.txt", None),
        ("execute", "new.txt", None),
        ("fetch", "new.txt", None),
        ("edit", ".git", None),
        ("edit", ".git/config", None),
        ("edit", ".bro/hooks/pre-commit", None),
    ] {
        let decision = decision(
            &artifacts,
            serde_json::json!({"tool_call":{"kind":kind,"locations":[{"path":path}]}}),
        )
        .await?;
        assert_eq!(
            allowed_local_option(&artifacts, &decision, &workspace)
                .await?
                .as_deref(),
            expected
        );
    }
    let missing = decision(
        &artifacts,
        serde_json::json!({"tool_call":{"title":"Safe read, trust this text","kind":"read"}}),
    )
    .await?;
    assert!(
        allowed_local_option(&artifacts, &missing, &workspace)
            .await?
            .is_none()
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn local_file_policy_rejects_paths_through_an_external_symlink() -> Result<()> {
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");
    tokio::fs::create_dir(&workspace).await?;
    tokio::fs::create_dir(&outside).await?;
    std::os::unix::fs::symlink(&outside, workspace.join("linked"))?;
    let decision = decision(
        &root.path().join("artifacts"),
        serde_json::json!({"tool_call":{"kind":"edit","locations":[{"path":"linked/new.txt"}]}}),
    )
    .await?;
    assert!(
        allowed_local_option(&root.path().join("artifacts"), &decision, &workspace)
            .await?
            .is_none()
    );
    Ok(())
}
