use super::*;

#[tokio::test]
async fn file_checks_use_actual_content_and_preserve_evidence_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    tokio::fs::write(root.path().join("result.txt"), b"verified behavior\n").await?;
    let mut check = CheckSpec {
        id: "content".into(),
        description: "Required result text".into(),
        recipe: CheckRecipe::FileContains {
            path: "result.txt".into(),
            text: "verified behavior".into(),
        },
        timeout_seconds: 1,
    };
    let first = run(root.path(), &check, "check", &CancellationToken::new())
        .await
        .map_err(std::io::Error::other)?;
    assert!(first.supports(&check));
    tokio::fs::write(root.path().join("result.txt"), b"unsupported assertion\n").await?;
    let second = run(root.path(), &check, "check-2", &CancellationToken::new())
        .await
        .map_err(std::io::Error::other)?;
    assert!(!second.supports(&check));
    assert_ne!(first.output["sha256"], second.output["sha256"]);
    check.recipe = CheckRecipe::FileExists {
        path: "missing.txt".into(),
    };
    assert!(
        !run(root.path(), &check, "missing", &CancellationToken::new())
            .await
            .map_err(std::io::Error::other)?
            .supports(&check)
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn file_checks_reject_external_symlink_targets() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    tokio::fs::write(outside.path().join("external.txt"), b"outside").await?;
    std::os::unix::fs::symlink(
        outside.path().join("external.txt"),
        root.path().join("linked.txt"),
    )?;
    let check = CheckSpec {
        id: "file".into(),
        description: "Must stay in workspace".into(),
        recipe: CheckRecipe::FileExists {
            path: "linked.txt".into(),
        },
        timeout_seconds: 1,
    };
    assert!(
        run(root.path(), &check, "check", &CancellationToken::new())
            .await
            .is_err()
    );
    Ok(())
}
