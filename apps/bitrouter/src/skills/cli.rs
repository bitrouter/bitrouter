//! The `bitrouter skills` subcommand tree and its dispatcher.
//!
//! Two verbs, both local and read-mostly: [`SkillsAction::List`] reads the
//! installed-skills directory, [`SkillsAction::Init`] scaffolds a `SKILL.md`.
//!
//! `add`, `remove`, `find`, and `update` were removed: installing skills is the
//! ecosystem's job (`npx skills add`, the Claude Code and Codex plugin
//! marketplaces), not a router's. See [`super`] for the reasoning.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use crate::commands;

/// `bitrouter skills …`. All variants land in [`run`].
#[derive(Debug, Subcommand)]
pub enum SkillsAction {
    /// List installed skills.
    List(ScopeArgs),
    /// Scaffold a new `SKILL.md` in the current directory.
    Init(InitArgs),
}

#[derive(Debug, clap::Args)]
pub struct ScopeArgs {
    /// Operate on the global skills directory (`~/.claude/skills/`).
    #[arg(long, short = 'g')]
    pub global: bool,
}

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Skill name written into the generated frontmatter.
    pub name: String,
    /// Output path for the SKILL.md (default: `<NAME>/SKILL.md`).
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,
}

/// Entry point dispatched by `apps/bitrouter/src/main.rs`.
pub fn run(action: SkillsAction, output: &crate::output::Output) -> Result<()> {
    match action {
        SkillsAction::List(args) => {
            output.emit(&commands::skills_list(args.global)?)?;
            Ok(())
        }
        SkillsAction::Init(args) => {
            let target = args
                .output
                .unwrap_or_else(|| PathBuf::from(&args.name).join("SKILL.md"));
            output.emit(&commands::skills_init(&args.name, &target)?)?;
            Ok(())
        }
    }
}
