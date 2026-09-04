//! The `skills_search` action, implemented over the shared skills roots.
//!
//! One implementation, two surfaces: `bitrouter skills list` calls
//! [`InstalledSkills::report`] directly, and the origin MCP server's
//! `skills_search` tool calls it through the [`SkillsQuery`] port. Both get the
//! same [`SkillsReport`], so the CLI's `--json` and the tool's structured
//! content cannot drift.
//!
//! It replaces three discovery rules over two roots:
//!
//! - `skills::format::discover_all_skills` — kept, and now the only one.
//! - `skills::root::list_installed` — a second `read_dir` of
//!   `<root>/.claude/skills` that never parsed frontmatter, so it listed skills
//!   the agent could not load and missed the `./skills/foo` layout entirely.
//!   Deleted; this module is the filter that replaced it.
//!   `skills_catalog`'s dir-name/format validation — a third rule, now
//!   `DiscoveredSkill::problem`, applied identically here and there.
//!
//! Which roots are read is [`SkillsRoot`]'s decision, not this module's, so the
//! CLI's `-g` and the MCP surfaces' project-plus-global scope come from one
//! place.

use std::collections::BTreeSet;

use bitrouter_mcp::actions::skills::{SkillDetail, SkillRow, SkillsQuery, SkillsReport};
use bitrouter_mcp::error::ToolError;

use crate::skills::format::{DiscoveredSkill, discover_all_skills, skill_body};
use crate::skills::root::SkillsRoot;

/// Lists the skills installed under a set of roots.
pub struct InstalledSkills {
    roots: Vec<SkillsRoot>,
}

impl InstalledSkills {
    /// Read the given roots, in order. Use [`SkillsRoot::cli_scope`] or
    /// [`SkillsRoot::mcp_scope`] to build the set rather than assembling one
    /// here — that is the shared root resolution.
    pub fn new(roots: Vec<SkillsRoot>) -> Self {
        Self { roots }
    }

    /// Every skill under the configured roots, valid or not.
    ///
    /// Blocking filesystem work — call it from a blocking context, or through
    /// [`Self::report_off_thread`].
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
        // One order for both surfaces, and stable across filesystem iteration
        // order. `dir` breaks a name tie, which is what a project-local and a
        // user-global skill of the same name produce.
        skills.sort_by(|a, b| (&a.name, &a.dir).cmp(&(&b.name, &b.dir)));
        SkillsReport { skills }
    }

    /// [`Self::report`] off the async runtime — discovery and frontmatter
    /// parsing are blocking filesystem work, and a large skills tree must not
    /// stall the reactor.
    async fn report_off_thread(&self) -> Result<SkillsReport, ToolError> {
        let roots = self.roots.clone();
        tokio::task::spawn_blocking(move || InstalledSkills { roots }.report())
            .await
            .map_err(|e| ToolError::new(format!("skills discovery task failed: {e}")))
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

#[async_trait::async_trait]
impl SkillsQuery for InstalledSkills {
    async fn list(&self) -> Result<SkillsReport, ToolError> {
        self.report_off_thread().await
    }

    async fn get(&self, name: &str) -> Result<SkillDetail, ToolError> {
        let report = self.report_off_thread().await?;
        // Resolution is by *name over the listing*, never by path arithmetic on
        // caller input, so `get` can only ever reach a file discovery already
        // vouched for under one of the configured roots.
        let skill = report
            .skills
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| ToolError::new(format!("no installed skill named '{name}'")))?;
        let path = std::path::PathBuf::from(&skill.skill_md);
        let content = tokio::task::spawn_blocking(move || std::fs::read_to_string(&path))
            .await
            .map_err(|e| ToolError::new(format!("skills fetch task failed: {e}")))?
            .map_err(|e| ToolError::new(format!("reading {}: {e}", skill.skill_md)))?;
        // `metadata` comes from the same parse the row's validity did, so a
        // skill whose frontmatter is broken yields an empty map rather than a
        // second, more forgiving reader's guess.
        let metadata = crate::skills::format::parse_frontmatter(&content)
            .ok()
            .map(|fm| {
                fm.metadata
                    .into_iter()
                    .collect::<serde_json::Map<String, serde_json::Value>>()
            })
            .unwrap_or_default();
        Ok(SkillDetail {
            body: skill_body(&content).to_string(),
            skill,
            metadata,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_mcp::capabilities::skill_catalog::SkillCatalog as _;

    fn install(root: &std::path::Path, rel: &str, dir: &str, body: &str) -> std::path::PathBuf {
        let skill_dir = root.join(rel).join(dir);
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

    #[tokio::test]
    async fn get_returns_the_body_without_the_frontmatter() {
        let dir = tempfile::tempdir().expect("tempdir");
        install(
            dir.path(),
            ".claude/skills",
            "alpha",
            &valid("alpha", "Does alpha"),
        );

        let detail = project(dir.path()).get("alpha").await.expect("get");
        assert_eq!(detail.skill.name, "alpha");
        assert_eq!(detail.skill.description, "Does alpha");
        assert!(detail.body.starts_with("# alpha"), "{:?}", detail.body);
        assert!(!detail.body.contains("description:"), "{:?}", detail.body);
    }

    #[tokio::test]
    async fn get_names_the_missing_skill() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = project(dir.path()).get("nope").await.expect_err("absent");
        assert!(err.0.contains("nope"), "{}", err.0);
    }

    /// **The phase, asserted end to end.** One disk, three surfaces, run
    /// together:
    ///
    /// - `bitrouter skills list` (the CLI's own path, via `report`),
    /// - `skills_search` (the MCP tool's path, via the port),
    /// - `skills/list` (the SEP-2640 catalog),
    ///
    /// over the three cases that used to disagree: a skill with malformed
    /// frontmatter (listed by the CLI, invisible to the agent), a `./skills/foo`
    /// skill (the reverse), and a user-global skill (CLI-only).
    ///
    /// The valid skills must appear identically everywhere; the invalid one must
    /// appear *marked* on the two tool/CLI surfaces and be absent from the SEP
    /// catalog, which is D3 as implemented.
    #[tokio::test]
    async fn one_disk_three_surfaces() {
        let project_dir = tempfile::tempdir().expect("project");
        let home_dir = tempfile::tempdir().expect("home");
        let project = project_dir.path();

        // (1) malformed frontmatter, under the project's `.claude/skills`.
        install(
            project,
            ".claude/skills",
            "broken",
            "---\nname: broken\n---\n",
        );
        // (2) the `./skills/foo` layout the CLI's old `read_dir` never saw.
        install(
            project,
            "skills",
            "bundled",
            &valid("bundled", "From ./skills"),
        );
        // (3) a user-global skill, under `<home>/.claude/skills`.
        install(
            home_dir.path(),
            ".claude/skills",
            "worldwide",
            &valid("worldwide", "From the global root"),
        );

        let global = SkillsRoot::Global {
            home: home_dir.path().to_path_buf(),
        };
        let project_root = SkillsRoot::Project {
            project_root: project.to_path_buf(),
        };

        // The CLI, as `bitrouter skills list` and `bitrouter skills list -g`
        // call it.
        let cli_project = InstalledSkills::new(vec![project_root.clone()]).report();
        let cli_global = InstalledSkills::new(vec![global.clone()]).report();

        // The MCP tool, through the port, over the scope MCP is wired with.
        let mcp_roots = vec![project_root, global];
        let tool = SkillsQuery::list(&InstalledSkills::new(mcp_roots.clone()))
            .await
            .expect("skills_search");

        // Every CLI row appears byte-identically in the tool's answer — which
        // is the unification, and covers the global skill by construction.
        for row in cli_project.skills.iter().chain(cli_global.skills.iter()) {
            assert!(
                tool.skills.contains(row),
                "`{}` differs between the CLI and skills_search:\n cli: {:?}\n mcp: {:?}",
                row.name,
                row,
                tool.skills.iter().find(|s| s.name == row.name),
            );
        }
        assert_eq!(
            tool.skills.len(),
            cli_project.skills.len() + cli_global.skills.len(),
            "the tool's scope is exactly the CLI's two scopes together"
        );

        // Invariant 6, stated positively: a project-scoped caller is handed
        // only the project root, so the global root is not merely filtered out
        // of its answer — it is never walked, and cannot be a path it reaches.
        assert!(
            cli_project
                .skills
                .iter()
                .all(|s| s.dir.starts_with(&project.display().to_string())),
            "a project-scoped listing must not reach outside the project root: {:?}",
            cli_project.skills
        );
        assert!(cli_project.skills.iter().all(|s| s.name != "worldwide"));

        let by_name = |name: &str| {
            tool.skills
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("`{name}` missing from skills_search: {:?}", tool.skills))
                .clone()
        };

        // (2) and (3) are valid and visible to the agent, which neither was.
        assert!(by_name("bundled").valid);
        assert!(by_name("worldwide").valid);
        // (1) is visible *and* explained, rather than silently absent.
        let broken = by_name("broken");
        assert!(!broken.valid);
        assert!(broken.problem.is_some(), "an invalid skill says why");

        // The SEP catalog: the same roots, the valid two, and not the broken
        // one — the SEP requires the entry's frontmatter to match the fetched
        // SKILL.md, which a skill with no parseable frontmatter cannot satisfy.
        let published = crate::skills_catalog::InstalledSkillCatalog::new(mcp_roots)
            .list()
            .await
            .expect("skills/list");
        // Ordered by URI rather than name — the catalog is keyed by URI, which
        // is what identifies a skill in the SEP.
        let mut names: Vec<&str> = published
            .skills
            .iter()
            .filter_map(|s| s.frontmatter.get("name").and_then(|n| n.as_str()))
            .collect();
        names.sort_unstable();
        assert_eq!(names, vec!["bundled", "worldwide"], "{published:?}");
        assert!(
            published
                .skills
                .iter()
                .any(|s| s.uri.starts_with("skill://global/")),
            "the global root gets its own organizational prefix: {published:?}"
        );
        // Every published entry carries a complete, sized manifest.
        for entry in &published.skills {
            let resources = entry
                .resources
                .entries()
                .expect("a filesystem skill is never dynamic");
            assert!(resources.iter().any(|r| r.uri == entry.uri));
            assert!(resources.iter().all(|r| r.size > 0));
        }
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
