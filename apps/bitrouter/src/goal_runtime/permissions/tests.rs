use super::*;

#[test]
fn selected_option_requires_choice_origin_and_unique_actual_resolution() -> Result<()> {
    for (origin, expected) in [
        (None, false),
        (Some("teardown"), false),
        (Some("cancellation"), false),
        (Some("controller_choice"), true),
        (Some("configured_policy"), true),
    ] {
        let event: crate::supervisor::SessionEvent = serde_json::from_value(serde_json::json!({
            "seq":1,"at":"2026-10-14T00:00:00Z","kind":{"event":"permission_resolved",
                "permission_id":"original-request","option_id":"deny-once","automatic":origin == Some("configured_policy"),"origin":origin}
        }))?;
        let events = std::slice::from_ref(&event);
        assert_eq!(
            choice_was_selected(events, "original-request", "deny-once"),
            expected
        );
        assert!(!choice_was_selected(events, "other-request", "deny-once"));
        assert!(!choice_was_selected(
            events,
            "original-request",
            "allow-once"
        ));
        let mut contradictory = serde_json::to_value(&event)?;
        contradictory["kind"]["automatic"] = (origin != Some("configured_policy")).into();
        assert!(!choice_was_selected(
            &[serde_json::from_value(contradictory)?],
            "original-request",
            "deny-once"
        ));
        assert!(!choice_was_selected(
            &[event.clone(), event],
            "original-request",
            "deny-once"
        ));
    }
    Ok(())
}

async fn decision(root: &Path, mut value: serde_json::Value) -> Result<PermissionDecision> {
    if value.get("permission_id").is_none() {
        value["permission_id"] = "permission".into();
    }
    if value["tool_call"].get("toolCallId").is_none() {
        value["tool_call"]["toolCallId"] = "test-tool".into();
    }
    if value.get("options").is_none() {
        value["options"] = serde_json::json!([
        {"optionId":"actual-once","name":"Allow once","kind":"allow_once"},
        {"optionId":"actual-always","name":"Always allow","kind":"allow_always"}
        ]);
    }
    let permission: PendingPermissionSnapshot = serde_json::from_value(value.clone())?;
    Ok(PermissionDecision {
        id: "decision".into(),
        execution_id: "execution".into(),
        run_id: "run".into(),
        permission_id: permission.permission_id,
        requirements_revision: 1,
        task_revision: 1,
        request: crate::goal_artifacts::put(root, &serde_json::to_vec(&value)?).await?,
        options: permission
            .options
            .iter()
            .map(|option| {
                let kind = serde_json::to_value(option.kind)?;
                Ok(PermissionOption {
                    id: option.option_id.0.to_string(),
                    label: option.name.to_string(),
                    allows: matches!(kind.as_str(), Some("allow_once" | "allow_always")),
                    persistent: matches!(kind.as_str(), Some("allow_always" | "reject_always")),
                })
            })
            .collect::<Result<Vec<_>>>()?,
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

#[tokio::test]
async fn explicit_argument_paths_preserve_missing_scope_and_conflict_guards() -> Result<()> {
    let root = tempfile::tempdir()?;
    let artifacts = root.path().join("artifacts");
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");
    tokio::fs::create_dir(&workspace).await?;
    tokio::fs::create_dir(&outside).await?;
    for (tool, expected) in [
        (
            serde_json::json!({"kind":"search","rawInput":{"pattern":"needle","path":workspace.join("README.md"),"output_mode":"content"}}),
            true,
        ),
        (
            serde_json::json!({"kind":"search","locations":[],"rawInput":{"pattern":"**/*","path":workspace}}),
            true,
        ),
        (
            serde_json::json!({"kind":"read","rawInput":{"file_path":"seed.txt"}}),
            true,
        ),
        (
            serde_json::json!({"kind":"edit","rawInput":{"filePath":"README.md"}}),
            true,
        ),
        (
            serde_json::json!({"kind":"edit","locations":[{"path":workspace}],"rawInput":{"filePath":outside}}),
            false,
        ),
        (
            serde_json::json!({"kind":"search","rawInput":{"pattern":"**/*"}}),
            false,
        ),
        (
            serde_json::json!({"kind":"search","locations":[{"path":workspace}],"rawInput":{"path":outside}}),
            false,
        ),
        (
            serde_json::json!({"kind":"search","locations":[{"path":outside}],"rawInput":{"path":workspace}}),
            false,
        ),
        (
            serde_json::json!({"kind":"search","rawInput":{"path":"../outside"}}),
            false,
        ),
        (
            serde_json::json!({"kind":"search","rawInput":{"path":"https://outside.example/data"}}),
            false,
        ),
        (
            serde_json::json!({"kind":"search","rawInput":{"path":""}}),
            false,
        ),
        (
            serde_json::json!({"kind":"search","rawInput":{"path":workspace,"file_path":outside}}),
            false,
        ),
        (
            serde_json::json!({"kind":"search","locations":"invalid","rawInput":{"path":workspace}}),
            false,
        ),
        (
            serde_json::json!({"kind":"search","rawInput":{"path":[workspace]}}),
            false,
        ),
        (
            serde_json::json!({"kind":"execute","rawInput":{"path":workspace}}),
            false,
        ),
        (
            serde_json::json!({"kind":"edit","rawInput":{"file_path":".git/config"}}),
            false,
        ),
    ] {
        let decision = decision(&artifacts, serde_json::json!({"tool_call":tool})).await?;
        let result = allowed_local_option(&artifacts, &decision, &workspace).await?;
        assert_eq!(
            result.as_deref(),
            expected.then_some("actual-once"),
            "tool scope {tool}"
        );
    }
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
    let scoped = decision(
        &root.path().join("artifacts"),
        serde_json::json!({"tool_call":{"kind":"edit","locations":[{"path":"linked/new.txt"}]}}),
    )
    .await?;
    assert!(
        allowed_local_option(&root.path().join("artifacts"), &scoped, &workspace)
            .await?
            .is_none()
    );
    let argument = decision(
        &root.path().join("artifacts"),
        serde_json::json!({"tool_call":{"kind":"edit","rawInput":{"file_path":"linked/new.txt"}}}),
    )
    .await?;
    assert!(
        allowed_local_option(&root.path().join("artifacts"), &argument, &workspace)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn captured_context_allows_only_declared_workspace_reads_and_keeps_all_paths() -> Result<()> {
    use crate::supervisor::{ReplayBatch, SessionEventKind};
    let temporary = tempfile::tempdir()?;
    let workspace = temporary.path().join("workspace");
    let artifacts = temporary.path().join("artifacts");
    tokio::fs::create_dir(&workspace).await?;
    let fixture = include_str!("../../../tests/fixtures/acp-permission-context.json");
    let replay: ReplayBatch =
        serde_json::from_str(&fixture.replace("/fixture/workspace", &workspace.to_string_lossy()))?;
    let permission = replay
        .events
        .iter()
        .find_map(|event| match &event.kind {
            SessionEventKind::Permission { permission } if event.seq == 66 => Some(permission),
            _ => None,
        })
        .context("original permission missing")?;
    let bytes = crate::goal_permission_material::capture(permission, "run", &replay)?;
    let material: serde_json::Value = serde_json::from_slice(&bytes)?;
    let request = decision(&artifacts, material.clone()).await?;
    assert_eq!(
        allowed_local_option(&artifacts, &request, &workspace)
            .await?
            .as_deref(),
        Some("once")
    );
    let mut outside = material;
    // An earlier path stays relevant even when a later declaration is local.
    outside["earlier_tool_context"]["declarations"][0]["kind"]["update"]["locations"] =
        serde_json::json!([{"path": temporary.path().join("private")}]);
    let request = decision(&artifacts, outside).await?;
    assert!(
        allowed_local_option(&artifacts, &request, &workspace)
            .await?
            .is_none()
    );
    Ok(())
}
