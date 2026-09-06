//! The `skills_search` action: *what skills do I have, and can I use them?*
//!
//! One report type, shared by `bitrouter skills list` and the MCP
//! `skills_search` tool, so the CLI's `--json` and the tool's structured
//! content are the same bytes.
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
//! So a [`SkillRow`] is emitted for every `SKILL.md` on disk, and one that
//! cannot be served carries `valid: false` plus the `problem` that stops it.
//! `skills/list` — the SEP-2640 catalog — still publishes only the valid ones,
//! because the SEP requires an entry's `frontmatter` to match the `SKILL.md` a
//! host fetches and its final URI segment to equal `frontmatter.name`; an
//! invalid skill has no conforming entry to publish. The tool and the CLI show
//! it marked, which is the difference between "you have a bug in this skill" and
//! "this skill does not exist".
//!
//! ## Why `dir` *and* `skill_md`
//!
//! The two surfaces both called this `path` and meant different things — the CLI
//! the skill's directory, the MCP tool its `SKILL.md`. Neither name was wrong;
//! sharing one key for both was. Both surfaces now carry both fields.

use crate::error::ToolError;

/// One skill found on disk, as both surfaces report it.
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
    /// The skills, ordered by name then directory so two surfaces reading the
    /// same disk emit the same bytes.
    pub skills: Vec<SkillRow>,
}

impl SkillsReport {
    /// Keep only the skills whose name or description contains `query`,
    /// case-insensitively; `None` keeps everything.
    ///
    /// The filter lives on the report rather than in the port, for the same
    /// reason `ModelsReport::filtered` does: `bitrouter skills list` and the
    /// tool's `query` argument are then the *same* filter over the *same* list,
    /// instead of each surface interpreting "matches" its own way.
    pub fn matching(mut self, query: Option<&str>) -> Self {
        if let Some(query) = query {
            let needle = query.to_lowercase();
            self.skills.retain(|s| {
                s.name.to_lowercase().contains(&needle)
                    || s.description.to_lowercase().contains(&needle)
            });
        }
        self
    }
}

/// One skill's full content — the `skills_get` tool's answer.
///
/// Not a second surface's report: `skills_get` has no CLI twin, and adding
/// `bitrouter skills show` for the table's sake would be dead surface. It is
/// typed all the same so the tool advertises an `output_schema` rather than an
/// opaque JSON blob, and so the body's frontmatter split is the one in
/// `SKILL.md`'s own format module instead of a fourth hand-rolled reader.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SkillDetail {
    /// The same row `skills_search` returns, so a caller that searched and then
    /// fetched sees one shape for the skill's identity.
    pub skill: SkillRow,
    /// `frontmatter.metadata`, when the frontmatter declared one.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// The markdown body: everything after the frontmatter fence.
    pub body: String,
}

/// Arguments to `skills_search`.
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct SkillsSearchArgs {
    /// Substring matched (case-insensitively) against installed skills' name
    /// and description. Omit to list every skill.
    pub query: Option<String>,
}

/// Arguments to `skills_get`.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct SkillsGetArgs {
    /// The skill's name (as returned by `skills_search`).
    pub name: String,
}

/// The `skills_search` / `skills_get` port.
///
/// Read-only. The app-side adapter resolves the skills roots and walks them;
/// this crate stays skills-free and owns only the shapes.
///
/// Filtering is deliberately absent from [`Self::list`]: an implementation
/// returns every skill it found and both surfaces narrow it with
/// [`SkillsReport::matching`].
#[async_trait::async_trait]
pub trait SkillsQuery: Send + Sync {
    /// Every skill under the resolved roots, valid or not.
    async fn list(&self) -> Result<SkillsReport, ToolError>;
    /// One skill's frontmatter, metadata and body, or a [`ToolError`] when no
    /// skill has that name.
    async fn get(&self, name: &str) -> Result<SkillDetail, ToolError>;
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn matching_narrows_on_name_or_description_case_insensitively() {
        let report = SkillsReport {
            skills: vec![
                row("refactor-rust", "Rustacean refactors"),
                row("write-docs", "Author documentation"),
            ],
        };
        assert_eq!(
            report
                .clone()
                .matching(Some("REFACTOR"))
                .skills
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            vec!["refactor-rust".to_string()]
        );
        assert_eq!(
            report
                .clone()
                .matching(Some("documentation"))
                .skills
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            vec!["write-docs".to_string()]
        );
        assert_eq!(report.clone().matching(None).skills.len(), 2);
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
