//! `SKILL.md` format support and the `bitrouter skills …` CLI.
//!
//! ## What this is, and what it deliberately is not
//!
//! BitRouter is a skills **server** and **gateway**: it serves the skills that
//! are installed and proxies the skills upstream MCP servers hold. It is not a
//! skills **host** (it never decides what enters a model's context) and, as of
//! the package-manager cut, it is not a skills **installer** either.
//!
//! Getting a skill onto disk is the ecosystem's job — `npx skills add`, Claude
//! Code's and Codex's plugin marketplaces. BitRouter reads the directory those
//! tools populate. That is the same line as "server, not host", applied one
//! level out: BitRouter handles transport, not content lifecycle.
//!
//! The former `bitrouter-skills` crate held both halves. Its package-manager
//! half (git clone, source resolution, install-to-disk, registry client) was
//! removed along with the `add` / `remove` / `find` / `update` verbs; its
//! format half moved here, where its only consumers live.
//!
//! - [`format`] — `SKILL.md` frontmatter parsing and discovery.
//! - [`root`] — which `.claude/skills` directory to read, and what is in it.
//! - [`cli`] — the surviving `skills list` / `skills init` verbs.
//!
//! Consumers: [`crate::skills_catalog`] (the SEP-2640 `skills/list` /
//! `skills/get` server), [`crate::skills_query`] (the `skills_search` /
//! `skills_get` tools), and [`cli`].

pub mod cli;
pub mod format;
pub mod root;

pub(crate) use root::home_dir;

/// Errors from `SKILL.md` parsing and skills-directory reads.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A `SKILL.md` was missing its YAML frontmatter block.
    #[error("SKILL.md has no YAML frontmatter block")]
    MissingFrontmatter,
    /// The YAML frontmatter failed to deserialize.
    #[error("frontmatter parse error: {0}")]
    Frontmatter(String),
    /// A skill name failed validation (path-traversal / illegal characters).
    #[error(
        "invalid skill name {0:?}: names may contain only ASCII letters, digits, '-', '_', '.' and may not start with '.' or contain path separators"
    )]
    InvalidSkillName(String),
    /// A filesystem operation failed.
    #[error("io error: {0}")]
    Io(String),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, Error>;

/// Reject names that could escape the skills directory or carry illegal
/// characters. Allowed: ASCII letters, digits, `-`, `_`, `.`; must be
/// non-empty and may not start with `.` or contain a path separator or `..`.
///
/// A *format* rule, not an install rule — it constrains what a skill may be
/// called, which is why it sits beside the frontmatter parser now that there is
/// nothing to install. It stays enforced because a skill name still reaches the
/// filesystem: `skills init` writes a file named after it, and the SEP-2640
/// catalog builds `skill://<name>/SKILL.md` URIs from it.
pub fn validate_skill_name(name: &str) -> Result<()> {
    let invalid = name.is_empty()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if invalid {
        return Err(Error::InvalidSkillName(name.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        for name in ["alpha", "my-skill", "my_skill", "v1.2", "a1"] {
            assert!(validate_skill_name(name).is_ok(), "{name} should be valid");
        }
    }

    #[test]
    fn rejects_names_that_could_escape_the_skills_directory() {
        for name in ["", ".hidden", "a/b", "a\\b", "..", "a..b", "sp ace", "é"] {
            assert!(
                matches!(validate_skill_name(name), Err(Error::InvalidSkillName(_))),
                "{name:?} should be rejected"
            );
        }
    }
}
