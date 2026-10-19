use super::*;

#[tokio::test]
async fn goal_artifacts_reject_changed_blob_and_foreign_path() -> Result<()> {
    let root = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    let reference = put(root.path(), b"original evidence").await?;
    assert_eq!(read(root.path(), &reference).await?, b"original evidence");
    let mut foreign = reference.clone();
    foreign.path = other.path().join(format!("{}.blob", foreign.sha256));
    tokio::fs::write(&foreign.path, b"original evidence").await?;
    assert!(read(root.path(), &foreign).await.is_err());
    tokio::fs::write(&reference.path, b"replaced evidence").await?;
    assert!(read(root.path(), &reference).await.is_err());
    Ok(())
}

async fn repository(source: &Path) -> Result<String> {
    tokio::fs::create_dir_all(source).await?;
    crate::goal_runtime::git(source, &["init".as_ref(), "-q".as_ref()]).await?;
    crate::goal_runtime::git(
        source,
        &[
            "config".as_ref(),
            "core.autocrlf".as_ref(),
            "false".as_ref(),
        ],
    )
    .await?;
    tokio::fs::write(source.join("tracked.txt"), b"before\n").await?;
    crate::goal_runtime::git(source, &["add".as_ref(), "-A".as_ref()]).await?;
    crate::goal_runtime::git(
        source,
        &[
            "-c".as_ref(),
            "user.name=Fixture".as_ref(),
            "-c".as_ref(),
            "user.email=fixture@localhost".as_ref(),
            "-c".as_ref(),
            "commit.gpgsign=false".as_ref(),
            "commit".as_ref(),
            "-qm".as_ref(),
            "test: artifact input".as_ref(),
        ],
    )
    .await?;
    let head = crate::goal_runtime::git(source, &["rev-parse".as_ref(), "HEAD".as_ref()]).await?;
    Ok(String::from_utf8(head.stdout)?.trim().to_string())
}

async fn worktree(source: &Path, consumer: &Path, head: &str) -> Result<()> {
    crate::goal_runtime::git(
        source,
        &[
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            consumer.as_os_str(),
            head.as_ref(),
        ],
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn goal_artifacts_transfer_real_tracked_and_untracked_changes() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = root.path().join("artifacts");
    let source = root.path().join("source");
    let consumer = root.path().join("consumer");
    let head = repository(&source).await?;
    worktree(&source, &consumer, &head).await?;
    tokio::fs::write(source.join("tracked.txt"), b"after\n").await?;
    tokio::fs::write(source.join("new.txt"), b"new result\n").await?;
    let reference = capture(&store, &source, &head).await?;
    apply(&store, &reference, &consumer, &head).await?;
    assert_eq!(
        tokio::fs::read(consumer.join("tracked.txt")).await?,
        b"after\n"
    );
    assert_eq!(
        tokio::fs::read(consumer.join("new.txt")).await?,
        b"new result\n"
    );
    assert!(
        apply(&store, &reference, &consumer, "different-version")
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn goal_artifacts_transfer_large_binary_patch_in_a_spaced_path() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = root.path().join("artifact store");
    let source = root.path().join("source repository");
    let consumer = root.path().join("consumer workspace");
    repository(&source).await?;
    let original = vec![0; 192 * 1024];
    tokio::fs::write(source.join("binary.dat"), &original).await?;
    crate::goal_runtime::git(&source, &["add".as_ref(), "-A".as_ref()]).await?;
    crate::goal_runtime::git(
        &source,
        &[
            "-c".as_ref(),
            "user.name=Fixture".as_ref(),
            "-c".as_ref(),
            "user.email=fixture@localhost".as_ref(),
            "-c".as_ref(),
            "commit.gpgsign=false".as_ref(),
            "commit".as_ref(),
            "-qm".as_ref(),
            "test: binary artifact input".as_ref(),
        ],
    )
    .await?;
    let head = crate::goal_runtime::git(&source, &["rev-parse".as_ref(), "HEAD".as_ref()]).await?;
    let head = String::from_utf8(head.stdout)?.trim().to_owned();
    worktree(&source, &consumer, &head).await?;
    let mut changed = Vec::with_capacity(original.len());
    let mut state = 1_u32;
    for _ in 0..original.len() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        changed.push(state.to_le_bytes()[0]);
    }
    tokio::fs::write(source.join("binary.dat"), &changed).await?;
    let reference = capture(&store, &source, &head).await?;
    let bundle: WorkspaceBundle = serde_json::from_slice(&read(&store, &reference).await?)?;
    assert!(bundle.patch.bytes > 64 * 1024);
    apply(&store, &reference, &consumer, &head).await?;
    assert_eq!(tokio::fs::read(consumer.join("binary.dat")).await?, changed);
    Ok(())
}

#[tokio::test]
async fn goal_artifacts_merge_diamond_dependencies_and_reject_conflicts() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = root.path().join("artifacts");
    let source = root.path().join("source");
    let left = root.path().join("left");
    let right = root.path().join("right");
    let consumer = root.path().join("consumer");
    let conflict = root.path().join("conflict");
    let head = repository(&source).await?;
    for path in [&left, &right, &consumer, &conflict] {
        worktree(&source, path, &head).await?;
    }
    tokio::fs::write(source.join("tracked.txt"), b"common upstream\n").await?;
    tokio::fs::write(source.join("common.txt"), b"shared artifact\n").await?;
    let ancestor = capture(&store, &source, &head).await?;
    for path in [&left, &right] {
        apply(&store, &ancestor, path, &head).await?;
    }
    tokio::fs::write(left.join("left.txt"), b"left result\n").await?;
    tokio::fs::write(right.join("right.txt"), b"right result\n").await?;
    let left_artifact = capture(&store, &left, &head).await?;
    let right_artifact = capture(&store, &right, &head).await?;
    apply(&store, &left_artifact, &consumer, &head).await?;
    apply(&store, &right_artifact, &consumer, &head).await?;
    for (path, bytes) in [
        ("tracked.txt", b"common upstream\n".as_slice()),
        ("common.txt", b"shared artifact\n".as_slice()),
        ("left.txt", b"left result\n".as_slice()),
        ("right.txt", b"right result\n".as_slice()),
    ] {
        assert_eq!(tokio::fs::read(consumer.join(path)).await?, bytes);
    }
    tokio::fs::write(conflict.join("tracked.txt"), b"incompatible change\n").await?;
    let conflict_artifact = capture(&store, &conflict, &head).await?;
    assert!(
        apply(&store, &conflict_artifact, &consumer, &head)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn goal_artifacts_integrate_dependent_overwrites_and_deletions() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = root.path().join("artifacts");
    let source = root.path().join("source");
    let downstream = root.path().join("downstream");
    let delivery = root.path().join("delivery");
    let head = repository(&source).await?;
    worktree(&source, &downstream, &head).await?;
    worktree(&source, &delivery, &head).await?;
    tokio::fs::write(source.join("tracked.txt"), b"intermediate\n").await?;
    tokio::fs::write(source.join("new.bin"), [0, 1, 2]).await?;
    tokio::fs::write(source.join("deleted.txt"), b"temporary\n").await?;
    let upstream = capture(&store, &source, &head).await?;
    apply(&store, &upstream, &downstream, &head).await?;
    tokio::fs::write(downstream.join("tracked.txt"), b"finished\n").await?;
    tokio::fs::write(downstream.join("new.bin"), [0, 3, 4]).await?;
    tokio::fs::remove_file(downstream.join("deleted.txt")).await?;
    let downstream = capture(&store, &downstream, &head).await?;
    // Deliberately not topologically ordered; dependency edges determine order.
    merge::integrate(
        &store,
        &[
            merge::Input {
                id: "b",
                dependencies: &["a".into()],
                artifact: &downstream,
            },
            merge::Input {
                id: "a",
                dependencies: &[],
                artifact: &upstream,
            },
        ],
        &delivery,
        &head,
    )
    .await?;
    assert_eq!(
        tokio::fs::read(delivery.join("tracked.txt")).await?,
        b"finished\n"
    );
    assert_eq!(tokio::fs::read(delivery.join("new.bin")).await?, [0, 3, 4]);
    assert!(!tokio::fs::try_exists(delivery.join("deleted.txt")).await?);
    Ok(())
}

#[tokio::test]
async fn goal_artifacts_integrate_diamond_using_actual_common_ancestor() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = root.path().join("artifacts");
    let source = root.path().join("source");
    let left = root.path().join("left");
    let right = root.path().join("right");
    let delivery = root.path().join("delivery");
    let head = repository(&source).await?;
    for path in [&left, &right, &delivery] {
        worktree(&source, path, &head).await?;
    }
    tokio::fs::write(source.join("tracked.txt"), b"common ancestor\n").await?;
    let ancestor = capture(&store, &source, &head).await?;
    apply(&store, &ancestor, &left, &head).await?;
    apply(&store, &ancestor, &right, &head).await?;
    tokio::fs::write(left.join("tracked.txt"), b"left updates ancestor\n").await?;
    tokio::fs::write(right.join("right.txt"), b"independent right edit\n").await?;
    let left_artifact = capture(&store, &left, &head).await?;
    let right_artifact = capture(&store, &right, &head).await?;
    merge::integrate(
        &store,
        &[
            merge::Input {
                id: "left",
                dependencies: &["ancestor".into()],
                artifact: &left_artifact,
            },
            merge::Input {
                id: "right",
                dependencies: &["ancestor".into()],
                artifact: &right_artifact,
            },
            merge::Input {
                id: "ancestor",
                dependencies: &[],
                artifact: &ancestor,
            },
        ],
        &delivery,
        &head,
    )
    .await?;
    assert_eq!(
        tokio::fs::read(delivery.join("tracked.txt")).await?,
        b"left updates ancestor\n"
    );
    assert_eq!(
        tokio::fs::read(delivery.join("right.txt")).await?,
        b"independent right edit\n"
    );
    // Genuine sibling conflicts still fail without changing the destination.
    tokio::fs::write(right.join("tracked.txt"), b"incompatible right edit\n").await?;
    let conflicting = capture(&store, &right, &head).await?;
    assert!(
        merge::integrate(
            &store,
            &[
                merge::Input {
                    id: "ancestor",
                    dependencies: &[],
                    artifact: &ancestor
                },
                merge::Input {
                    id: "left",
                    dependencies: &["ancestor".into()],
                    artifact: &left_artifact
                },
                merge::Input {
                    id: "right",
                    dependencies: &["ancestor".into()],
                    artifact: &conflicting
                },
            ],
            &delivery,
            &head
        )
        .await
        .is_err()
    );
    assert_eq!(
        tokio::fs::read(delivery.join("tracked.txt")).await?,
        b"left updates ancestor\n"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn goal_artifacts_never_write_through_workspace_symlinks() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = root.path().join("artifacts");
    let source = root.path().join("source");
    let consumer = root.path().join("consumer");
    let outside = root.path().join("outside");
    let head = repository(&source).await?;
    worktree(&source, &consumer, &head).await?;
    tokio::fs::create_dir(source.join("output")).await?;
    tokio::fs::write(source.join("output/result.txt"), b"result").await?;
    let reference = capture(&store, &source, &head).await?;
    tokio::fs::create_dir(&outside).await?;
    std::os::unix::fs::symlink(&outside, consumer.join("output"))?;
    assert!(apply(&store, &reference, &consumer, &head).await.is_err());
    assert!(!tokio::fs::try_exists(outside.join("result.txt")).await?);
    Ok(())
}
