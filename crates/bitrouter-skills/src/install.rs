//! Installing a resolved [`crate::source::SkillSource`] into an agent's skills
//! directory.
//!
//! The flow is: clone the source into a temporary directory, discover the
//! `SKILL.md` within it, validate the skill name, then copy the skill's
//! directory into the target (`~/.claude/skills/<name>` for global installs,
//! `./.claude/skills/<name>` for project installs).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::source::SkillSource;
use crate::{Error, Result, frontmatter};

/// Where a skill is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallTarget {
    /// `~/.claude/skills/` — user-global skills.
    Global,
    /// `<project_root>/.claude/skills/` — project-local skills.
    Project { project_root: PathBuf },
}

/// The outcome of an [`install`] call, for user-facing output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    /// The canonical skill name (from frontmatter).
    pub name: String,
    /// The directory the skill was installed into.
    pub dest: PathBuf,
    /// Whether an existing install was replaced.
    pub was_updated: bool,
}

impl InstallTarget {
    /// The `.claude/skills` directory for this target.
    fn skills_root(&self) -> Result<PathBuf> {
        match self {
            InstallTarget::Global => Ok(crate::home_dir()?.join(".claude").join("skills")),
            InstallTarget::Project { project_root } => {
                Ok(project_root.join(".claude").join("skills"))
            }
        }
    }

    /// The directory a skill named `name` installs into.
    pub fn skill_dir(&self, name: &str) -> Result<PathBuf> {
        validate_skill_name(name)?;
        Ok(self.skills_root()?.join(name))
    }
}

/// Reject names that could escape the skills directory or carry illegal
/// characters. Allowed: ASCII letters, digits, `-`, `_`, `.`; must be
/// non-empty and may not start with `.` or contain a path separator or `..`.
pub fn validate_skill_name(name: &str) -> Result<()> {
    let invalid = name.is_empty()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if invalid {
        return Err(Error::InvalidSkillName(name.to_string()));
    }
    Ok(())
}

/// A temporary directory removed on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bitrouter-skills-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).map_err(|e| Error::Io(format!("creating temp dir: {e}")))?;
        Ok(Self { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A staging directory created as a sibling of the final install destination
/// (so the subsequent rename is on the same filesystem and atomic). It is
/// removed on drop unless [`Staging::disarm`] is called after a successful
/// rename moves it away.
struct Staging {
    path: PathBuf,
    armed: bool,
}

impl Staging {
    fn new(parent: &std::path::Path, name: &str) -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{name}.tmp.{}.{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Ok(Self { path, armed: true })
    }

    /// Cancel cleanup-on-drop (the directory has been renamed into place).
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Install (or update) a skill from `source` into `target`.
///
/// `select` picks which skill to install when the source exposes several
/// (matched against the frontmatter `name`); with `None` and more than one
/// skill present, the call errors rather than guessing.
///
/// `install_as` overrides the installed directory name; with `None` the skill
/// installs under its frontmatter `name`. This lets `update` re-install to the
/// same directory even if the upstream frontmatter name drifts.
pub async fn install(
    source: &SkillSource,
    target: &InstallTarget,
    overwrite: bool,
    select: Option<&str>,
    install_as: Option<&str>,
) -> Result<InstallOutcome> {
    let temp = TempDir::new("clone")?;
    // Materialise the source into a child of the temp dir: git sources are
    // shallow-cloned, local sources are copied. (`git clone` requires the
    // destination to not already exist, hence the child path.)
    let clone_dir = temp.path.join("repo");
    match source.local_path() {
        Some(path) => {
            if !path.is_dir() {
                return Err(Error::Io(format!(
                    "local source {} is not a directory",
                    path.display()
                )));
            }
            copy_dir(path, &clone_dir)?;
        }
        None => source.clone_into(&clone_dir).await?,
    }

    // Locate the SKILL.md: a subdir source narrows the search, otherwise walk
    // the conventional locations. Re-validate the subdir at the join site —
    // the parse-time guard is not enough for sources built by other callers.
    let search_root = match source.subdir() {
        Some(sub) => {
            if !crate::source::subdir_is_safe(sub) {
                return Err(Error::InvalidSource(format!("unsafe subdir path: {sub}")));
            }
            clone_dir.join(sub)
        }
        None => clone_dir.clone(),
    };
    let discovered = frontmatter::discover_all_skills(&search_root);
    let (skill_md, fm) = match select {
        Some(name) => discovered
            .into_iter()
            .find(|(_, fm)| fm.name == name)
            .ok_or_else(|| Error::NoSkillFound(format!("{name} in {}", search_root.display())))?,
        None => {
            if discovered.len() > 1 {
                let mut names: Vec<&str> =
                    discovered.iter().map(|(_, fm)| fm.name.as_str()).collect();
                names.sort_unstable();
                return Err(Error::AmbiguousSkill(names.join(", ")));
            }
            discovered
                .into_iter()
                .next()
                .ok_or_else(|| Error::NoSkillFound(search_root.display().to_string()))?
        }
    };
    let skill_src_dir = skill_md
        .parent()
        .ok_or_else(|| Error::Io("SKILL.md has no parent directory".to_string()))?
        .to_path_buf();

    let install_name = install_as.unwrap_or(&fm.name);
    let dest = target.skill_dir(install_name)?;
    let was_updated = dest.exists();
    if was_updated && !overwrite {
        return Err(Error::AlreadyInstalled(
            install_name.to_string(),
            dest.display().to_string(),
        ));
    }
    let parent = dest
        .parent()
        .ok_or_else(|| Error::Io(format!("{} has no parent", dest.display())))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| Error::Io(format!("creating {}: {e}", parent.display())))?;

    // Stage into a sibling directory then atomically rename into place, so a
    // mid-copy failure never destroys a previously-working install.
    let staging = Staging::new(parent, install_name)?;
    copy_dir(&skill_src_dir, &staging.path)?;
    if was_updated {
        std::fs::remove_dir_all(&dest)
            .map_err(|e| Error::Io(format!("replacing {}: {e}", dest.display())))?;
    }
    std::fs::rename(&staging.path, &dest)
        .map_err(|e| Error::Io(format!("installing into {}: {e}", dest.display())))?;
    staging.disarm();

    Ok(InstallOutcome {
        name: install_name.to_string(),
        dest,
        was_updated,
    })
}

/// Remove an installed skill by name. Errors when it is not installed.
pub fn remove(name: &str, target: &InstallTarget) -> Result<PathBuf> {
    let dir = target.skill_dir(name)?;
    if !dir.exists() {
        return Err(Error::Io(format!(
            "skill {name:?} is not installed at {}",
            dir.display()
        )));
    }
    std::fs::remove_dir_all(&dir)
        .map_err(|e| Error::Io(format!("removing {}: {e}", dir.display())))?;
    Ok(dir)
}

/// List installed skills under `target` as `(name, path)` pairs. Returns an
/// empty list when the skills directory does not exist.
pub fn list_installed(target: &InstallTarget) -> Result<Vec<(String, PathBuf)>> {
    let root = target.skills_root()?;
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.join("SKILL.md").is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            out.push((name.to_string(), path));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Recursively copy `src` into `dst`, skipping any `.git` directory.
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .map_err(|e| Error::Io(format!("creating {}: {e}", dst.display())))?;
    let entries =
        std::fs::read_dir(src).map_err(|e| Error::Io(format!("reading {}: {e}", src.display())))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let file_type = entry
            .file_type()
            .map_err(|e| Error::Io(format!("statting {}: {e}", from.display())))?;
        // Skip symlinks: `std::fs::copy` follows them, so a planted link (e.g.
        // to `~/.ssh/id_rsa`) would otherwise exfiltrate file contents into the
        // installed skill. A skill never legitimately needs a symlink.
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            copy_dir(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)
                .map_err(|e| Error::Io(format!("copying {}: {e}", from.display())))?;
        }
        // Non-regular files (FIFOs, sockets, device nodes) are skipped:
        // `std::fs::copy` on them can block or error, and a skill never needs
        // them.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names_accepted() {
        for name in ["skill", "my-skill", "my_skill", "skill.v2", "abc123"] {
            assert!(validate_skill_name(name).is_ok(), "{name} should be valid");
        }
    }

    #[test]
    fn traversal_and_illegal_names_rejected() {
        for name in [
            "",
            ".hidden",
            "../evil",
            "a/b",
            "a\\b",
            "..",
            "with space",
            "emoji😀",
        ] {
            assert!(
                matches!(validate_skill_name(name), Err(Error::InvalidSkillName(_))),
                "{name:?} should be rejected"
            );
        }
    }

    #[test]
    fn project_skill_dir_is_deterministic() {
        let target = InstallTarget::Project {
            project_root: PathBuf::from("/tmp/proj"),
        };
        assert_eq!(
            target.skill_dir("alpha").unwrap(),
            PathBuf::from("/tmp/proj/.claude/skills/alpha")
        );
    }

    #[test]
    fn skill_dir_rejects_bad_name() {
        let target = InstallTarget::Project {
            project_root: PathBuf::from("/tmp/proj"),
        };
        assert!(matches!(
            target.skill_dir("../escape"),
            Err(Error::InvalidSkillName(_))
        ));
    }

    #[test]
    fn list_installed_empty_when_missing() {
        let target = InstallTarget::Project {
            project_root: PathBuf::from("/tmp/does-not-exist-brskills"),
        };
        assert!(list_installed(&target).unwrap().is_empty());
    }

    #[test]
    fn list_and_remove_roundtrip() {
        let root = unique_dir("listremove");
        let target = InstallTarget::Project {
            project_root: root.clone(),
        };
        let skill_dir = target.skill_dir("alpha").unwrap();
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: alpha\ndescription: d\n---\n",
        )
        .unwrap();

        let listed = list_installed(&target).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "alpha");

        let removed = remove("alpha", &target).unwrap();
        assert!(!removed.exists());
        assert!(list_installed(&target).unwrap().is_empty());

        assert!(remove("alpha", &target).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn installs_from_a_local_git_repo() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let base = unique_dir("install-git");
        // Build a source repo with a SKILL.md under skills/demo/.
        let repo = base.join("source-repo");
        let skill = repo.join("skills").join("demo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: demo\ndescription: A demo skill.\n---\n\n# Demo\n",
        )
        .unwrap();
        std::fs::write(skill.join("extra.md"), "sidecar").unwrap();
        git_init_commit(&repo);

        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let target = InstallTarget::Project {
            project_root: project.clone(),
        };
        // Clone the whole repo (no subdir narrowing) and let discovery find it.
        let source = SkillSource::Url {
            url: repo.display().to_string(),
            git_ref: None,
        };

        let outcome = install(&source, &target, false, None, None)
            .await
            .expect("install ok");
        assert_eq!(outcome.name, "demo");
        assert!(!outcome.was_updated);
        let dest = project.join(".claude/skills/demo");
        assert!(dest.join("SKILL.md").is_file());
        assert!(dest.join("extra.md").is_file());
        // .git must not be copied into the install.
        assert!(!dest.join(".git").exists());

        // Re-install without overwrite fails; with overwrite updates.
        assert!(matches!(
            install(&source, &target, false, None, None).await,
            Err(Error::AlreadyInstalled(_, _))
        ));
        let again = install(&source, &target, true, None, None)
            .await
            .expect("overwrite ok");
        assert!(again.was_updated);

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn installs_selected_skill_from_multi_skill_repo() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let base = unique_dir("install-select");
        let repo = base.join("source-repo");
        for name in ["one", "two"] {
            let skill = repo.join("skills").join(name);
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(
                skill.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: d\n---\n"),
            )
            .unwrap();
        }
        git_init_commit(&repo);

        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let target = InstallTarget::Project {
            project_root: project.clone(),
        };
        let source = SkillSource::Url {
            url: repo.display().to_string(),
            git_ref: None,
        };

        let outcome = install(&source, &target, false, Some("two"), None)
            .await
            .expect("install selected");
        assert_eq!(outcome.name, "two");
        assert!(project.join(".claude/skills/two/SKILL.md").is_file());
        assert!(!project.join(".claude/skills/one").exists());

        // Selecting a non-existent skill errors.
        assert!(matches!(
            install(&source, &target, false, Some("nope"), None).await,
            Err(Error::NoSkillFound(_))
        ));

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn installs_from_a_local_directory() {
        let base = unique_dir("install-local");
        // A plain (non-git) local skill directory.
        let skill = base.join("src").join("skills").join("local-demo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: local-demo\ndescription: d\n---\n",
        )
        .unwrap();

        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let target = InstallTarget::Project {
            project_root: project.clone(),
        };
        let source = SkillSource::Local {
            path: base.join("src"),
        };

        let outcome = install(&source, &target, false, None, None)
            .await
            .expect("local install ok");
        assert_eq!(outcome.name, "local-demo");
        assert!(project.join(".claude/skills/local-demo/SKILL.md").is_file());

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn ambiguous_multi_skill_errors_without_select() {
        let base = unique_dir("ambiguous");
        let src = base.join("src").join("skills");
        for name in ["one", "two"] {
            let d = src.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: d\n---\n"),
            )
            .unwrap();
        }
        let target = InstallTarget::Project {
            project_root: base.join("project"),
        };
        let source = SkillSource::Local {
            path: base.join("src"),
        };
        let err = install(&source, &target, false, None, None)
            .await
            .expect_err("ambiguous");
        assert!(matches!(err, Error::AmbiguousSkill(_)));
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn install_as_overrides_directory_name() {
        let base = unique_dir("install-as");
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // Frontmatter name differs from the directory we install under.
        std::fs::write(
            src.join("SKILL.md"),
            "---\nname: upstream-name\ndescription: d\n---\n",
        )
        .unwrap();
        let project = base.join("project");
        let target = InstallTarget::Project {
            project_root: project.clone(),
        };
        let source = SkillSource::Local { path: src };

        let outcome = install(&source, &target, true, None, Some("pinned-dir"))
            .await
            .expect("install_as");
        assert_eq!(outcome.name, "pinned-dir");
        assert!(project.join(".claude/skills/pinned-dir/SKILL.md").is_file());
        assert!(!project.join(".claude/skills/upstream-name").exists());
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn install_skips_symlinks() {
        let base = unique_dir("install-symlink");
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("SKILL.md"),
            "---\nname: linky\ndescription: d\n---\n",
        )
        .unwrap();
        // Plant a secret outside the source and a symlink to it inside.
        let secret = base.join("secret.txt");
        std::fs::write(&secret, "TOP SECRET").unwrap();
        std::os::unix::fs::symlink(&secret, src.join("leak.txt")).unwrap();

        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let target = InstallTarget::Project {
            project_root: project.clone(),
        };
        let source = SkillSource::Local { path: src };
        install(&source, &target, false, None, None)
            .await
            .expect("install");

        let dest = project.join(".claude/skills/linky");
        assert!(dest.join("SKILL.md").is_file());
        // The symlink must not have been copied (no leaked content).
        assert!(!dest.join("leak.txt").exists());
        std::fs::remove_dir_all(&base).ok();
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git_init_commit(repo: &Path) {
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
    }

    fn unique_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("brskills-inst-{label}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
