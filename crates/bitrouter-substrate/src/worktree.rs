//! Per-session git worktree lifecycle.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Creates/removes git worktrees rooted at the session's base repo. Branch name
/// is the worktree dir name.
pub struct WorktreeManager {
    base_repo: PathBuf,
}

impl WorktreeManager {
    pub fn new(base_repo: PathBuf) -> Self {
        Self { base_repo }
    }

    pub async fn create(&self, name: &str) -> Result<PathBuf> {
        let path = self
            .base_repo
            .join(".bitrouter")
            .join("worktrees")
            .join(name);
        let st = tokio::process::Command::new("git")
            .current_dir(&self.base_repo)
            .args(["worktree", "add", "-q"])
            .arg(&path)
            .args(["-b", name])
            .status()
            .await
            .context("spawning `git worktree add`")?;
        if !st.success() {
            anyhow::bail!("`git worktree add` failed for '{name}' (status {st})");
        }
        Ok(path)
    }

    pub async fn remove(&self, path: &Path) -> Result<()> {
        let st = tokio::process::Command::new("git")
            .current_dir(&self.base_repo)
            .args(["worktree", "remove", "--force"])
            .arg(path)
            .status()
            .await
            .context("spawning `git worktree remove`")?;
        if !st.success() {
            anyhow::bail!(
                "`git worktree remove` failed for {} (status {st})",
                path.display()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().expect("tempdir");
        for a in [
            &["init", "-q"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
        ] {
            std::process::Command::new("git")
                .current_dir(d.path())
                .args(a)
                .status()
                .expect("git");
        }
        std::fs::write(d.path().join("f"), "x").expect("write");
        std::process::Command::new("git")
            .current_dir(d.path())
            .args(["add", "."])
            .status()
            .expect("git");
        std::process::Command::new("git")
            .current_dir(d.path())
            .args(["commit", "-qm", "init"])
            .status()
            .expect("git");
        d
    }

    #[tokio::test]
    async fn create_then_remove() {
        let repo = init_repo();
        let mgr = WorktreeManager::new(repo.path().to_path_buf());
        let p = mgr.create("feature-x").await.expect("create");
        assert!(p.exists());
        mgr.remove(&p).await.expect("remove");
        assert!(!p.exists());
    }
}
