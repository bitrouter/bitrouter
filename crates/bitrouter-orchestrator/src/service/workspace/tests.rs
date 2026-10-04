use std::time::{Duration, Instant};

use super::*;

fn fixture() -> Result<(tempfile::TempDir, PathBuf), Box<dyn std::error::Error>> {
    let root = tempfile::TempDir::new()?;
    let path = root.path().join("workspace");
    std::fs::create_dir(&path)?;
    Ok((root, path.canonicalize()?))
}

fn owner(id: &str) -> ExecutionOwner {
    ExecutionOwner {
        server_instance_id: id.into(),
        generation: 1,
        stopped_at_ms: None,
    }
}

fn code<T>(result: Result<T, ServiceError>) -> Result<ErrorCode, Box<dyn std::error::Error>> {
    Ok(result.err().ok_or("unexpected workspace acquisition")?.code)
}

#[test]
fn only_confirmed_release_allows_an_independent_owner() -> Result<(), Box<dyn std::error::Error>> {
    let (_root, path) = fixture()?;
    let first = WorkspaceFence::acquire(&path, &owner("one"), "turn-one")?;
    assert_eq!(
        code(WorkspaceFence::acquire(&path, &owner("two"), "turn-two"))?,
        ErrorCode::Conflict
    );
    first.finish()?;
    // Idle is written while the kernel lock is still held.
    assert_eq!(
        code(WorkspaceFence::acquire(&path, &owner("two"), "turn-two"))?,
        ErrorCode::Conflict
    );
    drop(first);
    let second = WorkspaceFence::acquire(&path, &owner("two"), "turn-two")?;
    second.validate()?;
    drop(second);
    assert_eq!(
        code(WorkspaceFence::acquire(
            &path,
            &owner("three"),
            "turn-three"
        ))?,
        ErrorCode::RecoveryRequired
    );
    Ok(())
}

#[test]
fn inspection_preserves_unknown_ownership_and_cannot_release_it()
-> Result<(), Box<dyn std::error::Error>> {
    let (_root, path) = fixture()?;
    let fence = WorkspaceFence::acquire(&path, &owner("lost"), "old-turn")?;
    drop(fence);
    let (_, marker) = paths(&path)?;
    let before = std::fs::read(&marker)?;
    let inspection =
        WorkspaceFence::inspect(&path, "reader", "old-thread")?.ok_or("inspection missing")?;
    assert_eq!(code(inspection.finish())?, ErrorCode::RecoveryRequired);
    assert_eq!(std::fs::read(marker)?, before);
    drop(inspection);
    assert_eq!(
        code(WorkspaceFence::acquire(&path, &owner("new"), "new-turn"))?,
        ErrorCode::RecoveryRequired
    );
    let (_root, fresh) = fixture()?;
    let inspection =
        WorkspaceFence::inspect(&fresh, "reader", "legacy-thread")?.ok_or("inspection missing")?;
    drop(inspection);
    assert_eq!(
        code(WorkspaceFence::acquire(&fresh, &owner("new"), "new-turn"))?,
        ErrorCode::RecoveryRequired
    );
    Ok(())
}

#[test]
fn invalid_missing_and_oversized_markers_never_authorize_execution()
-> Result<(), Box<dyn std::error::Error>> {
    let (_root, path) = fixture()?;
    let fence = WorkspaceFence::acquire(&path, &owner("old"), "old-turn")?;
    let (_, marker) = paths(&path)?;
    drop(fence);
    for bytes in [b"invalid".to_vec(), vec![b' '; MARKER_BYTES as usize + 1]] {
        std::fs::write(&marker, bytes)?;
        assert_eq!(
            code(WorkspaceFence::acquire(&path, &owner("new"), "new-turn"))?,
            ErrorCode::RecoveryRequired
        );
    }
    std::fs::remove_file(&marker)?;
    assert_eq!(
        code(WorkspaceFence::acquire(&path, &owner("new"), "new-turn"))?,
        ErrorCode::RecoveryRequired
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlinked_coordination_paths_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let (root, path) = fixture()?;
    let (lock, _) = paths(&path)?;
    let other = root.path().join("other");
    std::fs::write(&other, "")?;
    std::os::unix::fs::symlink(other, lock)?;
    assert_eq!(
        code(WorkspaceFence::acquire(&path, &owner("new"), "new-turn"))?,
        ErrorCode::RecoveryRequired
    );
    Ok(())
}

// The parent launches this same test binary as a distinct OS process.
#[test]
fn workspace_process_fixture() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::var_os("BRO_WORKSPACE_TEST_PATH") else {
        return Ok(());
    };
    let ready = std::env::var_os("BRO_WORKSPACE_TEST_READY").ok_or("ready path missing")?;
    let _fence = WorkspaceFence::acquire(&PathBuf::from(path), &owner("child"), "child-turn")?;
    std::fs::write(PathBuf::from(ready), "ready")?;
    std::io::stdin().read_to_end(&mut Vec::new())?;
    Ok(())
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn process_loss_releases_kernel_lock_but_never_confirms_effect_cleanup()
-> Result<(), Box<dyn std::error::Error>> {
    let (root, path) = fixture()?;
    let ready = root.path().join("ready");
    let mut child = Child(
        std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "service::workspace::tests::workspace_process_fixture",
                "--nocapture",
            ])
            .env("BRO_WORKSPACE_TEST_PATH", &path)
            .env("BRO_WORKSPACE_TEST_READY", &ready)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        if child.0.try_wait()?.is_some() {
            return Err("workspace fixture exited before locking".into());
        }
        if Instant::now() >= deadline {
            return Err("workspace fixture did not acquire its lock".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        code(WorkspaceFence::acquire(
            &path,
            &owner("parent"),
            "parent-turn"
        ))?,
        ErrorCode::Conflict
    );
    child.0.kill()?;
    child.0.wait()?;
    assert_eq!(
        code(WorkspaceFence::acquire(
            &path,
            &owner("parent"),
            "parent-turn"
        ))?,
        ErrorCode::RecoveryRequired
    );
    Ok(())
}
