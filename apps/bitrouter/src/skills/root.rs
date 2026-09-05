//! Which skills directory to read.
//!
//! Moved here from the former `bitrouter-skills` crate's `install` module when
//! the skills package manager was cut. Only the *read* half survived — there is
//! no install, remove, clone, or copy path left in this binary. The type is
//! named for what it now is (a root to read) rather than what it used to be (a
//! target to install into).
//!
//! **The one root resolution.** Both the CLI (`bitrouter skills list`, with
//! `-g`) and both MCP surfaces resolve their roots through [`SkillsRoot`]. The
//! MCP surfaces used to be constructed over `current_dir()` alone, which is why
//! a user-global skill was reachable from the CLI and invisible to an agent;
//! they are now built over [`SkillsRoot::mcp_scope`], so they gain the global
//! root *by construction* rather than through a second constructor that could
//! drift again.
//!
//! ## Containment
//!
//! Each root is its own containment anchor: `format::is_safe_installed_path`
//! is applied with the root a path was discovered under, never with another
//! root. A project-scoped caller (`bitrouter skills list` with no `-g`) is
//! handed only the project root, so the global root cannot become a traversal
//! surface for it — a global skill is not merely filtered out of that caller's
//! answer, it is never walked.

use std::path::PathBuf;

use super::{Error, Result};

/// Which skills root a surface reads.
///
/// The home directory is *held*, not looked up on each call: resolving `$HOME`
/// inside `discovery_root` would make every surface's answer depend on the
/// process environment at the moment it was asked, and would leave no way to
/// exercise the global root in a test without mutating that environment for
/// every other test in the process. It is resolved once, at [`Self::global`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillsRoot {
    /// The user-global root, `<home>/.claude`, whose `skills/` directory is the
    /// conventional home for user-global skills.
    Global {
        /// The user's home directory.
        home: PathBuf,
    },
    /// A project root: the directory itself, plus its `skills/` and
    /// `.claude/skills/` conventional layouts.
    Project {
        /// The project directory.
        project_root: PathBuf,
    },
}

impl SkillsRoot {
    /// The user-global root, resolved from the environment.
    pub fn global() -> Result<SkillsRoot> {
        Ok(SkillsRoot::Global { home: home_dir()? })
    }

    /// The directory discovery anchors on, and against which containment is
    /// checked.
    ///
    /// `Global` resolves to `<home>/.claude` rather than `<home>/.claude/skills`
    /// because discovery's three conventional layouts are relative to a root:
    /// anchoring on `.claude` makes `.claude/skills/<name>/SKILL.md` the
    /// `<root>/skills/<child>` case, which is exactly the directory
    /// `list_installed` used to read on its own. Anchoring on the home directory
    /// itself would walk all of it.
    pub fn discovery_root(&self) -> PathBuf {
        match self {
            SkillsRoot::Global { home } => home.join(".claude"),
            SkillsRoot::Project { project_root } => project_root.clone(),
        }
    }

    /// The organizational prefix this root contributes to a published
    /// `skill://` URI, per SEP-2640 ("Preceding segments, if any, are a
    /// server-chosen organizational prefix").
    ///
    /// A project root contributes nothing, so project skills keep the URIs they
    /// already had. The global root contributes `global/`, which is what keeps
    /// a user-global `foo` and a project-local `foo` separately addressable now
    /// that one server serves both.
    pub fn uri_prefix(&self) -> &'static str {
        match self {
            SkillsRoot::Global { .. } => "global/",
            SkillsRoot::Project { .. } => "",
        }
    }

    /// The roots the CLI reads for `bitrouter skills list [-g]`.
    ///
    /// `-g` selects the global root *instead of* the project one, which is what
    /// the flag has always meant. Asking for it on a machine with no home
    /// directory is an error the user should see, because they named the root
    /// they wanted.
    pub fn cli_scope(global: bool, project_root: PathBuf) -> Result<Vec<SkillsRoot>> {
        if global {
            Ok(vec![SkillsRoot::global()?])
        } else {
            Ok(vec![SkillsRoot::Project { project_root }])
        }
    }

    /// The roots the MCP surfaces read: the project *and* the user-global root.
    ///
    /// An agent has no flag to pass and no reason to care which directory a
    /// skill was installed into, so it sees both. This is the "MCP gains
    /// user-global skills by construction" half of the unification.
    ///
    /// Unlike `-g`, an unresolvable home is not an error here: nobody asked for
    /// the global root specifically, and a server that refused to start over it
    /// would deny an agent the project skills it *can* read.
    pub fn mcp_scope(project_root: PathBuf) -> Vec<SkillsRoot> {
        let mut roots = vec![SkillsRoot::Project { project_root }];
        roots.extend(SkillsRoot::global());
        roots
    }
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
    fn a_project_root_anchors_on_the_project_directory() {
        let root = SkillsRoot::Project {
            project_root: PathBuf::from("/tmp/proj"),
        };
        assert_eq!(root.discovery_root(), PathBuf::from("/tmp/proj"));
        assert_eq!(root.uri_prefix(), "");
    }

    /// The global anchor must be `<home>/.claude`, so discovery's
    /// `<root>/skills` case lands on exactly `<home>/.claude/skills` — the
    /// directory the deleted `list_installed` read on its own.
    #[test]
    fn the_global_root_anchors_on_dot_claude_not_home() {
        let root = SkillsRoot::Global {
            home: PathBuf::from("/home/u"),
        };
        assert_eq!(root.discovery_root(), PathBuf::from("/home/u/.claude"));
        assert_eq!(
            root.discovery_root().join("skills"),
            PathBuf::from("/home/u/.claude/skills")
        );
        assert_eq!(root.uri_prefix(), "global/");
    }

    /// The scopes are the shared root resolution: `-g` swaps the project root
    /// for the global one, and MCP takes both — which is the only reason an
    /// agent can now see a user-global skill.
    #[test]
    fn scopes_are_what_each_surface_reads() {
        let project = PathBuf::from("/tmp/proj");
        assert_eq!(
            SkillsRoot::cli_scope(false, project.clone()).expect("project scope"),
            vec![SkillsRoot::Project {
                project_root: project.clone()
            }]
        );
        let global = SkillsRoot::global().expect("this machine has a home directory");
        assert_eq!(
            SkillsRoot::cli_scope(true, project.clone()).expect("global scope"),
            vec![global.clone()]
        );
        assert_eq!(
            SkillsRoot::mcp_scope(project.clone()),
            vec![
                SkillsRoot::Project {
                    project_root: project
                },
                global
            ],
            "the MCP scope is the CLI's two scopes together, not a third rule"
        );
    }
}
