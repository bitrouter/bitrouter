//! Reports for the `skills` commands.
//!
//! Only `list` and `init` remain. The `add` / `remove` / `find` / `update`
//! reports were removed with the skills package manager — see `crate::skills`.

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;
#[cfg(test)]
use crate::output::human::Theme;

/// One installed skill.
#[derive(Serialize)]
pub struct SkillEntry {
    pub name: String,
    pub path: String,
}

/// Result of `bro skills list`.
#[derive(Serialize)]
pub struct SkillsListReport {
    pub skills: Vec<SkillEntry>,
}

impl CliReport for SkillsListReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if self.skills.is_empty() {
            return h.line("no skills installed");
        }
        for s in &self.skills {
            h.line(&format!("{}\t{}", s.name, s.path))?;
        }
        Ok(())
    }
}

/// Result of `bro skills init <name>`.
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

    #[test]
    fn empty_list_says_so_rather_than_printing_nothing() {
        let report = SkillsListReport { skills: vec![] };
        let mut buf = Vec::new();
        let mut human = Human::new(&mut buf, Theme::none());
        report.render(&mut human).expect("render");
        assert_eq!(String::from_utf8_lossy(&buf).trim(), "no skills installed");
    }

    #[test]
    fn list_renders_name_and_path_per_skill() {
        let report = SkillsListReport {
            skills: vec![SkillEntry {
                name: "alpha".into(),
                path: "/p/.claude/skills/alpha".into(),
            }],
        };
        let mut buf = Vec::new();
        let mut human = Human::new(&mut buf, Theme::none());
        report.render(&mut human).expect("render");
        let out = String::from_utf8_lossy(&buf);
        assert!(out.contains("alpha"), "{out}");
        assert!(out.contains("/p/.claude/skills/alpha"), "{out}");
    }
}
