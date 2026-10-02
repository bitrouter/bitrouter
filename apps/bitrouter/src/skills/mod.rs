//! `SKILL.md` format support and the `bro skills …` CLI.
//!
//! ## What this is, and what it deliberately is not
//!
//! BitRouter's local skill support is a CLI inspection/scaffolding surface.
//! The independent MCP gateway can relay skills from configured upstream MCP
//! servers, but the OSS daemon does not originate installed local skills. It
//! is not a skills **host** (it never decides what enters a model's context)
//! and, as of the package-manager cut, it is not a skills **installer** either.
//!
//! Getting a skill onto disk is the ecosystem's job — `npx skills add`, Claude
//! Code's and Codex's plugin marketplaces. BitRouter's CLI reads the directory
//! those tools populate.
//!
//! The former `bitrouter-skills` crate held both halves. Its package-manager
//! half (git clone, source resolution, install-to-disk, registry client) was
//! removed along with the `add` / `remove` / `find` / `update` verbs; its
//! format half moved here, where its only consumers live.
//!
//! - [`mod@format`] — `SKILL.md` frontmatter parsing and discovery.
//! - [`root`] — which `.claude/skills` directory to read, and what is in it.
//! - [`cli`] — the surviving `skills list` / `skills init` verbs.
//!
//! Consumers: [`crate::actions::skills`] and [`cli`].

pub mod cli;
pub mod format;
pub mod root;

/// Errors from `SKILL.md` parsing and skills-directory reads.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A `SKILL.md` was missing its YAML frontmatter block.
    #[error("SKILL.md has no YAML frontmatter block")]
    MissingFrontmatter,
    /// The YAML frontmatter failed to deserialize.
    #[error("frontmatter parse error: {0}")]
    Frontmatter(String),
    /// A skill name failed the Agent Skills format rules.
    #[error(
        "invalid skill name {0:?}: use 1-64 lowercase ASCII letters, digits, or single hyphens; hyphens may not lead, trail, or repeat"
    )]
    InvalidSkillName(String),
    /// A filesystem operation failed.
    #[error("io error: {0}")]
    Io(String),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, Error>;

/// Whether `name` satisfies the Agent Skills name grammar.
pub(crate) fn is_valid_skill_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// Whether `description` satisfies the Agent Skills length rule.
pub(crate) fn is_valid_skill_description(description: &str) -> bool {
    (1..=1024).contains(&description.chars().count())
}

/// Reject a name that the Agent Skills format, and therefore the SEP catalog,
/// cannot publish. Keeping the CLI and catalog on this one validator prevents
/// `skills init` from scaffolding a skill the server later skips.
pub fn validate_skill_name(name: &str) -> Result<()> {
    if !is_valid_skill_name(name) {
        return Err(Error::InvalidSkillName(name.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        for name in ["alpha", "my-skill", "a1"] {
            assert!(validate_skill_name(name).is_ok(), "{name} should be valid");
        }
    }

    #[test]
    fn rejects_names_outside_the_agent_skills_grammar() {
        for name in [
            "",
            ".hidden",
            "UPPER",
            "my_skill",
            "v1.2",
            "a/b",
            "a\\b",
            "a--b",
            "-leading",
            "trailing-",
            "sp ace",
            "é",
        ] {
            assert!(
                matches!(validate_skill_name(name), Err(Error::InvalidSkillName(_))),
                "{name:?} should be rejected"
            );
        }
        assert!(matches!(
            validate_skill_name(&"a".repeat(65)),
            Err(Error::InvalidSkillName(_))
        ));
    }
}
