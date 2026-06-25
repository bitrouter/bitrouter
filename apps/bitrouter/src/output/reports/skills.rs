//! Reports for the `skills` commands.

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;

/// Result of `bitrouter skills add <source>`.
#[derive(Serialize)]
pub struct SkillAddReport {
    /// `installed` or `updated`.
    pub action: &'static str,
    pub name: String,
    pub dest: String,
}

impl CliReport for SkillAddReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!("{} {} → {}", self.action, self.name, self.dest))
    }
}

/// One installed skill.
#[derive(Serialize)]
pub struct SkillEntry {
    pub name: String,
    pub path: String,
}

/// Result of `bitrouter skills list`.
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

/// Result of `bitrouter skills remove <name>`.
#[derive(Serialize)]
pub struct SkillRemoveReport {
    pub name: String,
    pub path: String,
    pub removed: bool,
}

impl CliReport for SkillRemoveReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!("removed {} ({})", self.name, self.path))
    }
}

/// One registry search hit.
#[derive(Serialize)]
pub struct SkillHit {
    pub name: String,
    pub version: String,
    pub description: String,
}

/// Result of `bitrouter skills find <query>`.
#[derive(Serialize)]
pub struct SkillsFindReport {
    pub query: String,
    pub registry: String,
    pub results: Vec<SkillHit>,
}

impl CliReport for SkillsFindReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if self.results.is_empty() {
            return h.line(&format!(
                "no skills matching {:?} in {}",
                self.query, self.registry
            ));
        }
        for r in &self.results {
            h.line(&format!("{}\t{}\t{}", r.name, r.version, r.description))?;
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

/// A skill updated in `skills update`.
#[derive(Serialize)]
pub struct UpdatedSkill {
    pub name: String,
    pub dest: String,
}

/// A skill skipped in `skills update` (not in the registry).
#[derive(Serialize)]
pub struct SkippedSkill {
    pub name: String,
    pub reason: String,
}

/// A skill that failed to update.
#[derive(Serialize)]
pub struct FailedSkill {
    pub name: String,
    pub error: String,
}

/// Result of `bitrouter skills update [name]`. Exits non-zero if any skill
/// failed (parity with the legacy behavior).
#[derive(Serialize)]
pub struct SkillsUpdateReport {
    pub updated: Vec<UpdatedSkill>,
    pub skipped: Vec<SkippedSkill>,
    pub failed: Vec<FailedSkill>,
}

impl CliReport for SkillsUpdateReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if self.updated.is_empty() && self.skipped.is_empty() && self.failed.is_empty() {
            return h.line("no skills installed to update");
        }
        for u in &self.updated {
            h.line(&format!("updated {} → {}", u.name, u.dest))?;
        }
        for s in &self.skipped {
            h.line(&format!("skipped {} ({})", s.name, s.reason))?;
        }
        for f in &self.failed {
            h.line(&format!("failed {}: {}", f.name, f.error))?;
        }
        Ok(())
    }

    fn exit_code(&self) -> i32 {
        if self.failed.is_empty() { 0 } else { 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::CliReport;

    #[test]
    fn skills_update_exit_code() {
        let clean = SkillsUpdateReport {
            updated: vec![],
            skipped: vec![],
            failed: vec![],
        };
        assert_eq!(clean.exit_code(), 0);
        let failed = SkillsUpdateReport {
            updated: vec![],
            skipped: vec![],
            failed: vec![FailedSkill {
                name: "x".into(),
                error: "e".into(),
            }],
        };
        assert_eq!(failed.exit_code(), 1);
    }
}
