//! Per-session git worktree lifecycle.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// How a session should treat its worktree at shutdown.
///
/// The worktree holds the agent's output — `Remove` runs `git worktree remove
/// --force`, which **destroys uncommitted work**, so retention is the expected
/// default and removal is an explicit opt-in (`--rm-worktree` on the CLI).
///
/// `name` and `branch` may contain the `{record16}` placeholder, replaced at
/// provisioning time with the first 16 hex characters of the session's record
/// id — this is how fleet managers get `bitrouter/<agent>-<record16>` branches
/// that stay traceable to their session record.
#[derive(Debug, Clone)]
pub struct WorktreeSpec {
    /// Worktree directory name; also the branch name unless `branch` is set.
    pub name: String,
    /// Branch to attach or create in the worktree. `None` = same as `name`.
    pub branch: Option<String>,
    /// Remove the worktree at shutdown. Only honored when the worktree was
    /// newly created by this session — a pre-existing worktree is never
    /// removed.
    pub remove_on_shutdown: bool,
}

/// The result of provisioning a worktree for a session.
#[derive(Debug)]
pub struct ProvisionedWorktree {
    pub path: PathBuf,
    /// `false` when an existing registered worktree at the same path was
    /// reused instead of created. Reused worktrees are never removed by the
    /// session (neither on launch failure nor at shutdown).
    pub newly_created: bool,
    /// The branch checked out in the worktree, when it could be determined
    /// (queried from the worktree itself on reuse, so a reused tree that
    /// moved branches reports the truth).
    pub branch: Option<String>,
    /// The base repo `HEAD` commit a **newly created** branch was cut from.
    /// `None` when an existing branch or worktree was attached — its true
    /// base is not knowable at provisioning time.
    pub base_ref: Option<String>,
}

/// Creates/removes git worktrees rooted at the session's base repo.
pub struct WorktreeManager {
    base_repo: PathBuf,
}

impl WorktreeManager {
    pub fn new(base_repo: PathBuf) -> Self {
        Self { base_repo }
    }

    /// The repository this manager roots its worktrees (and the session's
    /// default cwd) at.
    pub fn base_repo(&self) -> &Path {
        &self.base_repo
    }

    /// Provision the worktree named `name` under `.bitrouter/worktrees/`,
    /// on branch `branch` (`None` = same as `name`).
    ///
    /// - An existing **registered** worktree at that path is reused as-is
    ///   (relaunching a session with the same name attaches to its work).
    /// - Otherwise the worktree is created, attaching to the branch when it
    ///   already exists and creating it (`-b`, based on the current `HEAD`)
    ///   when it does not.
    pub async fn create(&self, name: &str, branch: Option<&str>) -> Result<ProvisionedWorktree> {
        let branch = branch.unwrap_or(name);
        let path = self
            .base_repo
            .join(".bitrouter")
            .join("worktrees")
            .join(name);

        if path.exists() {
            // A registered worktree carries a `.git` link file; anything else
            // at the path is unexpected and must not be silently adopted.
            if path.join(".git").exists() {
                // Report the branch the reused tree is actually on — it may
                // have moved since it was provisioned.
                let branch = git_stdout(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
                return Ok(ProvisionedWorktree {
                    path,
                    newly_created: false,
                    branch,
                    base_ref: None,
                });
            }
            anyhow::bail!(
                "worktree path {} exists but is not a git worktree",
                path.display()
            );
        }

        let branch_exists = tokio::process::Command::new("git")
            .current_dir(&self.base_repo)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}"))
            .status()
            .await
            .context("spawning `git rev-parse`")?
            .success();
        // A new branch is cut from the current HEAD — capture it now so the
        // session record can persist the diff/merge base durably.
        let base_ref = if branch_exists {
            None
        } else {
            git_stdout(&self.base_repo, &["rev-parse", "HEAD"]).await
        };

        let mut cmd = tokio::process::Command::new("git");
        cmd.current_dir(&self.base_repo)
            .args(["worktree", "add", "-q"])
            .arg(&path);
        if branch_exists {
            // Attach the existing branch (fails clearly if it is already
            // checked out in another worktree).
            cmd.arg(branch);
        } else {
            cmd.args(["-b", branch]);
        }
        let st = cmd.status().await.context("spawning `git worktree add`")?;
        if !st.success() {
            anyhow::bail!("`git worktree add` failed for '{name}' (status {st})");
        }
        Ok(ProvisionedWorktree {
            path,
            newly_created: true,
            branch: Some(branch.to_string()),
            base_ref,
        })
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

/// Trimmed stdout of a git query, `None` on any failure — provisioning must
/// not fail because a metadata read did.
async fn git_stdout(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
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
        let p = mgr.create("feature-x", None).await.expect("create");
        assert!(p.newly_created);
        assert!(p.path.exists());
        mgr.remove(&p.path).await.expect("remove");
        assert!(!p.path.exists());
    }

    #[tokio::test]
    async fn create_attaches_to_existing_branch() {
        let repo = init_repo();
        // Create the branch outside any worktree.
        std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["branch", "feat-1"])
            .status()
            .expect("git branch");
        let mgr = WorktreeManager::new(repo.path().to_path_buf());
        let p = mgr
            .create("feat-1", None)
            .await
            .expect("attach existing branch");
        assert!(p.newly_created);
        assert!(p.path.exists());
        // The worktree is on the existing branch, not a duplicate.
        let head = std::process::Command::new("git")
            .current_dir(&p.path)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("git rev-parse");
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "feat-1");
    }

    #[tokio::test]
    async fn create_with_distinct_branch_name() {
        let repo = init_repo();
        let mgr = WorktreeManager::new(repo.path().to_path_buf());
        let p = mgr
            .create("codex-abc123", Some("bitrouter/codex-abc123"))
            .await
            .expect("create with branch");
        assert!(p.path.ends_with(".bitrouter/worktrees/codex-abc123"));
        let head = std::process::Command::new("git")
            .current_dir(&p.path)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("git rev-parse");
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            "bitrouter/codex-abc123",
            "branch name is independent of the dir name"
        );
        // Provisioning reports the branch and the HEAD it was cut from —
        // the durable diff/merge base for the session record.
        assert_eq!(p.branch.as_deref(), Some("bitrouter/codex-abc123"));
        let head_sha = std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse");
        assert_eq!(
            p.base_ref.as_deref(),
            Some(String::from_utf8_lossy(&head_sha.stdout).trim()),
            "base_ref is the base repo HEAD at provisioning"
        );
    }

    #[tokio::test]
    async fn create_reuses_existing_worktree() {
        let repo = init_repo();
        let mgr = WorktreeManager::new(repo.path().to_path_buf());
        let first = mgr.create("feat-2", None).await.expect("create");
        assert!(first.newly_created);
        // Leave uncommitted work behind, then provision the same name again.
        std::fs::write(first.path.join("wip"), "uncommitted").expect("write");
        let second = mgr.create("feat-2", None).await.expect("reuse");
        assert!(!second.newly_created, "existing worktree must be reused");
        assert_eq!(second.path, first.path);
        assert_eq!(
            second.branch.as_deref(),
            Some("feat-2"),
            "reuse reports the branch the tree is actually on"
        );
        assert_eq!(
            second.base_ref, None,
            "a reused tree's true base is unknowable"
        );
        assert!(
            second.path.join("wip").exists(),
            "reuse must not clobber existing work"
        );
    }

    #[tokio::test]
    async fn create_rejects_non_worktree_path() {
        let repo = init_repo();
        let clash = repo.path().join(".bitrouter").join("worktrees").join("x");
        std::fs::create_dir_all(&clash).expect("mkdir");
        let mgr = WorktreeManager::new(repo.path().to_path_buf());
        assert!(
            mgr.create("x", None).await.is_err(),
            "a non-worktree dir at the path must not be silently adopted"
        );
    }
}
