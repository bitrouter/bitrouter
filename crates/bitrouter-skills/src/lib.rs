//! # bitrouter-skills
//!
//! Shared, framework-agnostic building blocks for BitRouter's skills gateway:
//!
//! - [`source`] — parse a skill source string (`owner/repo`, a git URL, or a
//!   subdirectory) into a [`source::SkillSource`] and fetch it onto disk.
//! - [`frontmatter`] — parse the YAML frontmatter of a `SKILL.md` and discover
//!   skills under a directory tree.
//! - [`marketplace`] — the `marketplace.json` wire types shared by the server
//!   registry and the client installer.
//! - [`install`] — clone/copy a resolved skill into an agent's skills
//!   directory.
//!
//! This crate has no dependency on `bitrouter-sdk`; it is consumed by both the
//! `bitrouter` CLI (client side) and, later, the server-side registry.

#![forbid(unsafe_code)]

pub mod frontmatter;
pub mod install;
pub mod marketplace;
pub mod source;

/// Errors produced across the skills crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A source string could not be parsed into a [`source::SkillSource`].
    #[error("invalid skill source: {0}")]
    InvalidSource(String),
    /// A `SKILL.md` was missing its YAML frontmatter block.
    #[error("SKILL.md has no YAML frontmatter block")]
    MissingFrontmatter,
    /// The YAML frontmatter failed to deserialize.
    #[error("frontmatter parse error: {0}")]
    Frontmatter(String),
    /// No `SKILL.md` was found while walking a fetched source tree.
    #[error("no SKILL.md found under {0}")]
    NoSkillFound(String),
    /// The source exposes several skills and none was selected.
    #[error("source exposes multiple skills ({0}); choose one with --skill <name>")]
    AmbiguousSkill(String),
    /// A skill name failed validation (path-traversal / illegal characters).
    #[error(
        "invalid skill name {0:?}: names may contain only ASCII letters, digits, '-', '_', '.' and may not start with '.' or contain path separators"
    )]
    InvalidSkillName(String),
    /// A skill was already installed and overwrite was not requested.
    #[error("skill {0:?} is already installed at {1}; pass --yes to overwrite")]
    AlreadyInstalled(String, String),
    /// The `git` binary failed or is unavailable.
    #[error("git error: {0}")]
    Git(String),
    /// An HTTP request to a registry failed.
    #[error("registry request error: {0}")]
    Http(String),
    /// A filesystem operation failed.
    #[error("io error: {0}")]
    Io(String),
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Resolve the current user's home directory from the environment
/// (`HOME` on Unix, `USERPROFILE` on Windows).
pub(crate) fn home_dir() -> Result<std::path::PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .ok_or_else(|| Error::Io(format!("could not resolve home directory (${var} unset)")))
}
