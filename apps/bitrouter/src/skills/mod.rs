//! BitRouter-owned versioned skills and compatible unmanaged inspection.
//!
//! The shared format parser and discovery rules are owned by
//! `bitrouter_orchestrator::harness::skills`. This module snapshots local/Git
//! bundles into scope-owned libraries and atomically publishes their receipts.
//! Unmanaged discovery and source scaffolding remain available. Installation
//! does not select/activate skills, change harness configuration or run scripts.

mod bundle;
pub mod cli;
pub mod delivery;
pub mod explain;
mod managed;
pub(crate) mod native;
pub mod root;
mod source;

/// Reuse the owned installer from setup without exposing its internal store.
pub(crate) async fn install_for_setup(
    source: &str,
    selected: Option<&str>,
    reference: Option<String>,
    global: bool,
    workspace: Option<&std::path::Path>,
    interactive: bool,
) -> anyhow::Result<serde_json::Value> {
    let store = managed::Store::load(managed::Scope::resolve(global, workspace).await?)?;
    Ok(serde_json::to_value(
        store
            .install_with_interaction(source, selected, reference, interactive)
            .await?,
    )?)
}

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
    bitrouter_orchestrator::harness::skills::validate_skill_name(name)
        .map_err(|_| Error::InvalidSkillName(name.to_string()))
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
