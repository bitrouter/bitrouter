//! CLI inspection and scaffolding for installed Agent Skills.
//!
//! This module owns the shared format parser and offline discovery rules,
//! selects CLI roots and renders reports. Installing skills remains the
//! ecosystem's responsibility.
//! The managed harness publishes metadata and returns versioned materials to
//! Core. Ordinary `bro skills list` does not activate a skill or execute scripts.

pub mod cli;
#[cfg(feature = "harness")]
pub mod delivery;
pub mod directory;
pub mod explain;
mod files;
pub mod format;
pub mod library;
pub mod mcp;
pub(crate) mod native;
pub mod protocol;
pub mod root;

/// Errors from `SKILL.md` parsing and skills-directory reads.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A skill name failed the Agent Skills format rules.
    #[error(
        "invalid skill name {0:?}: use 1-64 lowercase Unicode letters, numbers, or single hyphens; hyphens may not lead, trail, or repeat"
    )]
    InvalidSkillName(String),
    /// A filesystem operation failed.
    #[error("io error: {0}")]
    Io(String),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, Error>;

/// Validate with the same rules used by the production harness.
pub fn validate_skill_name(name: &str) -> Result<()> {
    format::validate_skill_name(name).map_err(|_| Error::InvalidSkillName(name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        for name in ["alpha", "my-skill", "a1", "é", "审阅-٢"] {
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
            "É",
            "ǅ",
            "审阅-😀",
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
