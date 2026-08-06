//! Which skills directory to read, and what is installed in it.
//!
//! Moved here from the former `bitrouter-skills` crate's `install` module when
//! the skills package manager was cut. Only the *read* half survived — there is
//! no install, remove, clone, or copy path left in this binary. The type is
//! named for what it now is (a root to read) rather than what it used to be (a
//! target to install into).

use std::path::PathBuf;

use super::{Error, Result};

/// Which `.claude/skills` directory a command operates on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillsRoot {
    /// `~/.claude/skills/` — user-global skills.
    Global,
    /// `<project_root>/.claude/skills/` — project-local skills.
    Project {
        /// The project directory holding `.claude/skills`.
        project_root: PathBuf,
    },
}

impl SkillsRoot {
    /// The `.claude/skills` directory this root resolves to.
    pub fn path(&self) -> Result<PathBuf> {
        match self {
            SkillsRoot::Global => Ok(super::home_dir()?.join(".claude").join("skills")),
            SkillsRoot::Project { project_root } => Ok(project_root.join(".claude").join("skills")),
        }
    }
}

/// Installed skills under `root`, as `(name, path)` pairs sorted by name.
///
/// Returns an empty list when the skills directory does not exist — an agent
/// with no skills installed is an ordinary state, not an error.
pub fn list_installed(root: &SkillsRoot) -> Result<Vec<(String, PathBuf)>> {
    let dir = root.path()?;
    let Ok(entries) = std::fs::read_dir(&dir) else {
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

/// Resolve the home directory from the environment (`HOME` on Unix,
/// `USERPROFILE` on Windows).
pub(crate) fn home_dir() -> Result<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error::Io(format!("could not resolve home directory (${var} unset)")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_root_resolves_under_dot_claude() {
        let root = SkillsRoot::Project {
            project_root: PathBuf::from("/tmp/proj"),
        };
        assert_eq!(
            root.path().expect("path"),
            PathBuf::from("/tmp/proj/.claude/skills")
        );
    }

    #[test]
    fn missing_directory_lists_nothing_rather_than_erroring() {
        let root = SkillsRoot::Project {
            project_root: PathBuf::from("/nonexistent-project-dir-for-test"),
        };
        assert!(list_installed(&root).expect("ok").is_empty());
    }

    #[test]
    fn lists_only_directories_holding_a_skill_md_sorted_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skills = dir.path().join(".claude").join("skills");
        for name in ["zebra", "alpha"] {
            let d = skills.join(name);
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::write(d.join("SKILL.md"), "---\nname: x\ndescription: d\n---\n")
                .expect("write");
        }
        // A directory without a SKILL.md is not a skill.
        std::fs::create_dir_all(skills.join("not-a-skill")).expect("mkdir");

        let root = SkillsRoot::Project {
            project_root: dir.path().to_path_buf(),
        };
        let names: Vec<String> = list_installed(&root)
            .expect("list")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["alpha".to_string(), "zebra".to_string()]);
    }
}
