//! Structured management of BitRouter-owned skills, with unmanaged inspection.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use super::managed::{Scope, Store};
use crate::commands;
use bitrouter_orchestrator::core::skills::{SkillConstraints, SkillSelector};
use bitrouter_orchestrator::harness::skills::library::LibraryScope;

#[derive(Debug, Default, clap::Args)]
pub struct ConstraintArgs {
    /// Require project:ID, global:ID or mcp:SERVER:URI (repeatable).
    #[arg(long = "skill", value_name = "SELECTOR", value_parser = parse_selector)]
    pub include: Vec<SkillSelector>,
    /// Exclude an exact scoped skill identity (repeatable).
    #[arg(long = "exclude-skill", value_name = "SELECTOR", value_parser = parse_selector)]
    pub exclude: Vec<SkillSelector>,
}

impl ConstraintArgs {
    pub fn constraints(&self) -> Result<Option<SkillConstraints>> {
        if self.include.is_empty() && self.exclude.is_empty() {
            return Ok(None);
        }
        let constraints = SkillConstraints {
            include: self.include.clone(),
            exclude: self.exclude.clone(),
        };
        constraints.validate()?;
        Ok(Some(constraints))
    }
}

fn parse_selector(value: &str) -> std::result::Result<SkillSelector, String> {
    let (kind, identity) = value
        .split_once(':')
        .ok_or("use project:ID, global:ID or mcp:SERVER:URI")?;
    match kind {
        "project" | "global" => {
            crate::control_output::validate_id(identity).map_err(|error| error.to_string())?;
            Ok(SkillSelector::Managed {
                scope: if kind == "project" {
                    LibraryScope::Project
                } else {
                    LibraryScope::Global
                },
                installation_id: identity.into(),
            })
        }
        "mcp" => {
            let (server, uri) = identity.split_once(':').ok_or("use mcp:SERVER:URI")?;
            crate::control_output::validate_id(server).map_err(|error| error.to_string())?;
            bitrouter_sdk::mcp::skills::validate_skill_resource_uri(uri)?;
            Ok(SkillSelector::Mcp {
                server: server.into(),
                uri: uri.into(),
            })
        }
        _ => Err("use project:ID, global:ID or mcp:SERVER:URI".into()),
    }
}

#[derive(Debug, Subcommand)]
pub enum SkillsAction {
    /// Explain original native skill choices and prepared model context, read-only.
    Explain(super::explain::Options),
    /// List managed installations and unmanaged discoveries in one scope.
    List(ScopeArgs),
    /// Show exact installation/source/version metadata; never activates a skill.
    Show {
        name: String,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Scaffold source files only; does not register an installation.
    Init(InitArgs),
    /// Snapshot a local skill directory or Git repository; never runs scripts.
    Install {
        source: String,
        #[arg(long)]
        skill: Option<String>,
        #[arg(long = "ref")]
        reference: Option<String>,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Prepare and validate a new immutable version before moving its alias.
    Update {
        name: String,
        #[arg(long = "ref")]
        reference: Option<String>,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Remove the managed alias and retain all immutable versions.
    Uninstall {
        name: String,
        #[command(flatten)]
        scope: ScopeArgs,
    },
}

#[derive(Debug, clap::Args)]
pub struct ScopeArgs {
    /// Select the current user's BitRouter home skills library.
    #[arg(long, short = 'g', conflicts_with = "workspace")]
    pub global: bool,
    /// Explicit project root; default is the nearest Git root or current directory.
    #[arg(long, conflicts_with = "global")]
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    pub name: String,
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,
}

pub async fn run(action: SkillsAction, output: &crate::output::Output) -> Result<()> {
    let report = match action {
        SkillsAction::Explain(options) => {
            output.emit(&super::explain::read(options).await?)?;
            return Ok(());
        }
        SkillsAction::Init(args) => {
            let target = args
                .output
                .unwrap_or_else(|| PathBuf::from(&args.name).join("SKILL.md"));
            output.emit(&commands::skills_init(&args.name, &target)?)?;
            return Ok(());
        }
        SkillsAction::List(args) => Store::load(
            Scope::resolve(args.global, args.workspace.as_deref()).await?,
        )?
        .report(None, "list", "inspected")?,
        SkillsAction::Show { name, scope } => Store::load(
            Scope::resolve(scope.global, scope.workspace.as_deref()).await?,
        )?
        .report(Some(&name), "show", "inspected")?,
        SkillsAction::Install {
            source,
            skill,
            reference,
            scope,
        } => {
            Store::load(Scope::resolve(scope.global, scope.workspace.as_deref()).await?)?
                .install(&source, skill.as_deref(), reference)
                .await?
        }
        SkillsAction::Update {
            name,
            reference,
            scope,
        } => {
            Store::load(Scope::resolve(scope.global, scope.workspace.as_deref()).await?)?
                .update(&name, reference)
                .await?
        }
        SkillsAction::Uninstall { name, scope } => {
            Store::load(Scope::resolve(scope.global, scope.workspace.as_deref()).await?)?
                .uninstall(&name)?
        }
    };
    output.emit(&report)?;
    Ok(())
}
