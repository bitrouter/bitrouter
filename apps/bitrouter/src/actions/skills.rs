//! Installed Agent Skills inspection for `bro skills list`.
//!
//! ## Why the row carries `valid` / `problem`
//!
//! Three discovery rules over two roots used to answer this question, and they
//! disagreed in both directions: a skill with malformed frontmatter was listed
//! by the CLI (which never parsed one) and invisible to the agent, while a
//! `./skills/foo` skill was the reverse. Unifying them forces a single answer to
//! "is this skill usable?", and the honest one is *say so*, rather than making
//! the surfaces agree by subtraction.
//!
//! A [`SkillRow`] is emitted for every `SKILL.md` on disk, and one that cannot
//! be loaded carries `valid: false` plus the `problem` that stops it. Native
//! agent hosts own installation and activation; BitRouter only inspects and
//! validates the conventional local layouts.
//!
//! ## Why `dir` *and* `skill_md`
//!
//! The row carries both the skill directory and its `SKILL.md`, avoiding an
//! ambiguous `path` field.

/// One skill found on disk.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SkillRow {
    /// The skill's name: `frontmatter.name` when it parsed, and the directory
    /// name when it did not — so an unusable skill is still nameable in the
    /// message that explains why.
    pub name: String,
    /// `frontmatter.description`, or empty when the frontmatter did not parse.
    pub description: String,
    /// The skill's **directory**.
    pub dir: String,
    /// The skill's **`SKILL.md`** file, inside [`Self::dir`].
    pub skill_md: String,
    /// Whether this skill can actually be served and loaded.
    pub valid: bool,
    /// What stops it, when [`Self::valid`] is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// Every skill found under the resolved roots.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SkillsReport {
    /// The skills, ordered by name then directory.
    pub skills: Vec<SkillRow>,
}

#[cfg(test)]
mod report_tests {
    use super::*;

    fn row(name: &str, description: &str) -> SkillRow {
        SkillRow {
            name: name.into(),
            description: description.into(),
            dir: format!("/p/{name}"),
            skill_md: format!("/p/{name}/SKILL.md"),
            valid: true,
            problem: None,
        }
    }

    /// `problem` is omitted rather than `null` for a healthy skill, so the
    /// common row stays the shape it was before invalid skills became visible.
    #[test]
    fn a_valid_row_carries_no_problem_key() {
        let wire = serde_json::to_value(row("alpha", "d")).expect("ser");
        assert!(wire.get("problem").is_none(), "{wire}");
        assert_eq!(wire["dir"], "/p/alpha");
        assert_eq!(wire["skill_md"], "/p/alpha/SKILL.md");
    }
}

use std::collections::BTreeSet;

use crate::skills::format::{DiscoveredSkill, discover_all_skills};
use crate::skills::root::SkillsRoot;

/// Lists the skills installed under a set of roots.
pub struct InstalledSkills {
    roots: Vec<SkillsRoot>,
}

impl InstalledSkills {
    /// Read roots already resolved by [`SkillsRoot::cli_scope`].
    pub fn new(roots: Vec<SkillsRoot>) -> Self {
        Self { roots }
    }

    /// Every skill under the configured roots, valid or not.
    ///
    /// Blocking filesystem work; CLI callers run it as a bounded inspection.
    pub fn report(&self) -> SkillsReport {
        let mut skills = Vec::new();
        let mut seen: BTreeSet<std::path::PathBuf> = BTreeSet::new();
        for root in &self.roots {
            for found in discover_all_skills(&root.discovery_root()) {
                // Two roots can overlap (a project *inside* `~/.claude`), and
                // the same `SKILL.md` must not be reported twice.
                if !seen.insert(found.skill_md.clone()) {
                    continue;
                }
                skills.push(row(&found));
            }
        }
        // Stable across filesystem iteration order. `dir` breaks a name tie,
        // which is what project-local and user-global skills can produce.
        skills.sort_by(|a, b| (&a.name, &a.dir).cmp(&(&b.name, &b.dir)));
        SkillsReport { skills }
    }
}

/// One discovered skill as the shared row.
fn row(found: &DiscoveredSkill) -> SkillRow {
    let problem = found.problem();
    SkillRow {
        name: found.name(),
        description: found.description().to_string(),
        dir: found.dir.display().to_string(),
        skill_md: found.skill_md.display().to_string(),
        valid: problem.is_none(),
        problem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `rel` is written with `/` separators for readability, so push its
    /// segments one at a time rather than joining the whole string: on Windows
    /// `root.join(".claude/skills")` keeps the slash *inside* one component,
    /// and the helper's path then disagrees with the native one discovery
    /// builds — a test-only artefact that looks like a production bug.
    fn install(root: &std::path::Path, rel: &str, dir: &str, body: &str) -> std::path::PathBuf {
        let mut skill_dir = root.to_path_buf();
        for segment in rel.split('/').filter(|segment| !segment.is_empty()) {
            skill_dir.push(segment);
        }
        skill_dir.push(dir);
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        std::fs::write(skill_dir.join("SKILL.md"), body).expect("write");
        skill_dir
    }

    fn valid(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nDo it.\n")
    }

    fn project(root: &std::path::Path) -> InstalledSkills {
        InstalledSkills::new(
            SkillsRoot::cli_scope(false, root.to_path_buf()).expect("project scope"),
        )
    }

    #[test]
    fn every_conventional_layout_is_one_listing() {
        let dir = tempfile::tempdir().expect("tempdir");
        install(
            dir.path(),
            ".claude/skills",
            "installed",
            &valid("installed", "d"),
        );
        // The layout the CLI's old `read_dir` could not see at all.
        install(dir.path(), "skills", "bundled", &valid("bundled", "d"));

        let names: Vec<_> = project(dir.path())
            .report()
            .skills
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["bundled".to_string(), "installed".to_string()]);
    }

    /// The drift, from the other side: a skill with malformed frontmatter is
    /// listed *and* marked, rather than listed unmarked by one surface and
    /// dropped by the other.
    #[test]
    fn a_broken_skill_is_listed_and_marked() {
        let dir = tempfile::tempdir().expect("tempdir");
        install(dir.path(), ".claude/skills", "ok", &valid("ok", "d"));
        install(
            dir.path(),
            ".claude/skills",
            "broken",
            "---\nname: broken\n---\n",
        );

        let skills = project(dir.path()).report().skills;
        assert_eq!(skills.len(), 2);
        let broken = skills.iter().find(|s| s.name == "broken").expect("listed");
        assert!(!broken.valid);
        assert!(broken.problem.is_some());
        let ok = skills.iter().find(|s| s.name == "ok").expect("listed");
        assert!(ok.valid && ok.problem.is_none());
    }

    /// `path` used to mean the directory on one surface and the `SKILL.md` on
    /// the other. Both fields, on both surfaces, and they are related.
    #[test]
    fn a_row_carries_both_the_directory_and_the_skill_md() {
        let dir = tempfile::tempdir().expect("tempdir");
        let installed = install(dir.path(), ".claude/skills", "alpha", &valid("alpha", "d"));

        let row = project(dir.path()).report().skills.remove(0);
        assert_eq!(row.dir, installed.display().to_string());
        assert_eq!(
            row.skill_md,
            installed.join("SKILL.md").display().to_string()
        );
    }

    /// Containment survives the unification: a `SKILL.md` reached only through
    /// a symlink under the root is not listed, so a second root cannot be used
    /// to walk out of the first.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_skill_directory_is_not_listed() {
        let outside = tempfile::tempdir().expect("tempdir");
        install(outside.path(), "", "escape", &valid("escape", "d"));

        let dir = tempfile::tempdir().expect("tempdir");
        let skills = dir.path().join(".claude").join("skills");
        std::fs::create_dir_all(&skills).expect("mkdir");
        std::os::unix::fs::symlink(outside.path().join("escape"), skills.join("escape"))
            .expect("symlink");

        assert!(
            project(dir.path()).report().skills.is_empty(),
            "a symlink beneath the root must not be traversed"
        );
    }
}
