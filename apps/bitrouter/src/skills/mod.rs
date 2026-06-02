//! `bitrouter skills …` — the client-side skill installer.
//!
//! Installs Claude Code skills from GitHub, a git URL, or a BitRouter registry
//! into an agent's skills directory (`~/.claude/skills/` or
//! `./.claude/skills/`). The parsing, fetching, and install logic lives in the
//! framework-agnostic [`bitrouter_skills`] crate; this module is the CLI
//! surface and dispatch glue, mirroring the `cloud` subcommand layout.

pub mod cli;
