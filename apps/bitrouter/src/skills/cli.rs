//! The `bitrouter skills` subcommand tree and its dispatcher.
//!
//! Each leaf is a thin shim over a `commands::skills_*` function (kept in the
//! lib so the logic is testable and reusable), matching how `cloud/cli.rs`
//! delegates to the management client.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use crate::commands;

/// `bitrouter skills …`. All variants land in [`run`].
#[derive(Debug, Subcommand)]
pub enum SkillsAction {
    /// Install a skill from a source (GitHub `owner/repo`, a git URL, or a
    /// registry skill name).
    Add(AddArgs),
    /// List installed skills.
    List(ScopeArgs),
    /// Remove an installed skill by name.
    Remove(RemoveArgs),
    /// Search the configured registry for skills.
    Find(FindArgs),
    /// Scaffold a new `SKILL.md` in the current directory.
    Init(InitArgs),
    /// Re-install installed skills from the registry to their latest version.
    Update(UpdateArgs),
}

#[derive(Debug, clap::Args)]
pub struct AddArgs {
    /// Source: `owner/repo`, a full git URL, or a registry skill name.
    pub source: String,
    /// When the source exposes several skills, install the one with this
    /// frontmatter `name`.
    #[arg(long = "skill", value_name = "NAME")]
    pub skill: Option<String>,
    /// Install into `~/.claude/skills/` instead of `./.claude/skills/`.
    #[arg(long, short = 'g')]
    pub global: bool,
    /// Overwrite an existing install of the same skill.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Registry base URL used when `source` is a bare skill name.
    #[arg(long, value_name = "URL")]
    pub registry: Option<String>,
    /// Namespace id whose registry hub to query (required for registry operations).
    #[arg(long, short = 'n', value_name = "NSID")]
    pub namespace: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct ScopeArgs {
    /// Operate on the global skills directory (`~/.claude/skills/`).
    #[arg(long, short = 'g')]
    pub global: bool,
}

#[derive(Debug, clap::Args)]
pub struct RemoveArgs {
    /// The skill name to remove.
    pub name: String,
    /// Remove from the global skills directory.
    #[arg(long, short = 'g')]
    pub global: bool,
}

#[derive(Debug, clap::Args)]
pub struct FindArgs {
    /// Query matched against name, description, and tags.
    pub query: String,
    /// Registry base URL to search.
    #[arg(long, value_name = "URL")]
    pub registry: Option<String>,
    /// Namespace id whose registry hub to query (required for registry operations).
    #[arg(long, short = 'n', value_name = "NSID")]
    pub namespace: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Skill name written into the generated frontmatter.
    pub name: String,
    /// Output path for the SKILL.md.
    #[arg(long, short = 'o', default_value = "SKILL.md")]
    pub output: PathBuf,
}

#[derive(Debug, clap::Args)]
pub struct UpdateArgs {
    /// Update only this skill. When omitted, updates every installed skill
    /// found in the registry.
    pub name: Option<String>,
    /// Update the global skills directory.
    #[arg(long, short = 'g')]
    pub global: bool,
    /// Registry base URL to update from.
    #[arg(long, value_name = "URL")]
    pub registry: Option<String>,
    /// Namespace id whose registry hub to query (required for registry operations).
    #[arg(long, short = 'n', value_name = "NSID")]
    pub namespace: Option<String>,
}

/// Entry point dispatched by `apps/bitrouter/src/main.rs`.
pub async fn run(action: SkillsAction, output: &crate::output::Output) -> Result<()> {
    match action {
        SkillsAction::Add(args) => {
            output.emit(
                &commands::skills_add(
                    &args.source,
                    args.skill.as_deref(),
                    args.global,
                    args.yes,
                    args.registry.as_deref(),
                    args.namespace.as_deref(),
                )
                .await?,
            )?;
            Ok(())
        }
        SkillsAction::List(args) => {
            output.emit(&commands::skills_list(args.global)?)?;
            Ok(())
        }
        SkillsAction::Remove(args) => {
            output.emit(&commands::skills_remove(&args.name, args.global)?)?;
            Ok(())
        }
        SkillsAction::Find(args) => {
            output.emit(
                &commands::skills_find(
                    &args.query,
                    args.registry.as_deref(),
                    args.namespace.as_deref(),
                )
                .await?,
            )?;
            Ok(())
        }
        SkillsAction::Init(args) => {
            output.emit(&commands::skills_init(&args.name, &args.output)?)?;
            Ok(())
        }
        SkillsAction::Update(args) => {
            let report = commands::skills_update(
                args.name.as_deref(),
                args.global,
                args.registry.as_deref(),
                args.namespace.as_deref(),
            )
            .await?;
            output.emit(&report)?;
            // Parity with the legacy behavior: a partial-failure run exits 1.
            if !report.failed.is_empty() {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
