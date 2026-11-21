//! Serializable skill inspection rows used by service reports.

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
