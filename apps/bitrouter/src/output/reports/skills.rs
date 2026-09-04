//! Reports for the `skills` commands.
//!
//! Only `list` and `init` remain. The `add` / `remove` / `find` / `update`
//! reports were removed with the skills package manager — see `crate::skills`.

use bitrouter_mcp::actions::skills::SkillsReport;
use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;
#[cfg(test)]
use crate::output::human::Theme;

/// Human rendering for the shared `skills_search` report.
///
/// The type is the MCP crate's — `impl CliReport for <foreign report>` is legal
/// because the trait is ours — so `bitrouter skills list --json` is byte-for-byte
/// the tool's structured content, and only this rendering is CLI-only.
impl CliReport for SkillsReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if self.skills.is_empty() {
            return h.line("no skills installed");
        }
        for s in &self.skills {
            // `dir` is what a reader wants to `cd` into; the `SKILL.md` is one
            // predictable level down and would only make the line noisier.
            h.line(&format!("{}\t{}", s.name, s.dir))?;
            // An unusable skill says so on its own line rather than being
            // silently absent, which is what the agent surface used to do.
            if let Some(problem) = &s.problem {
                h.line(&format!("  ! unusable: {problem}"))?;
            }
        }
        Ok(())
    }
}

/// Result of `bitrouter skills init <name>`.
#[derive(Serialize)]
pub struct SkillInitReport {
    pub path: String,
    pub created: bool,
}

impl CliReport for SkillInitReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!("wrote {}", self.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bitrouter_mcp::actions::skills::SkillRow;

    fn rendered(report: &SkillsReport) -> String {
        let mut buf = Vec::new();
        let mut human = Human::new(&mut buf, Theme::none());
        report.render(&mut human).expect("render");
        String::from_utf8_lossy(&buf).to_string()
    }

    #[test]
    fn empty_list_says_so_rather_than_printing_nothing() {
        let out = rendered(&SkillsReport { skills: vec![] });
        assert_eq!(out.trim(), "no skills installed");
    }

    #[test]
    fn list_renders_name_and_directory_per_skill() {
        let out = rendered(&SkillsReport {
            skills: vec![SkillRow {
                name: "alpha".into(),
                description: "d".into(),
                dir: "/p/.claude/skills/alpha".into(),
                skill_md: "/p/.claude/skills/alpha/SKILL.md".into(),
                valid: true,
                problem: None,
            }],
        });
        assert!(out.contains("alpha"), "{out}");
        assert!(out.contains("/p/.claude/skills/alpha"), "{out}");
        assert!(!out.contains("unusable"), "{out}");
    }

    /// D3, rendered: a skill you can see on disk and cannot load says why,
    /// rather than being dropped so the two surfaces agree by subtraction.
    #[test]
    fn an_invalid_skill_is_listed_with_its_problem() {
        let out = rendered(&SkillsReport {
            skills: vec![SkillRow {
                name: "broken".into(),
                description: String::new(),
                dir: "/p/.claude/skills/broken".into(),
                skill_md: "/p/.claude/skills/broken/SKILL.md".into(),
                valid: false,
                problem: Some("frontmatter parse error: missing field `description`".into()),
            }],
        });
        assert!(out.contains("broken"), "{out}");
        assert!(out.contains("unusable"), "{out}");
        assert!(out.contains("missing field"), "{out}");
    }
}
