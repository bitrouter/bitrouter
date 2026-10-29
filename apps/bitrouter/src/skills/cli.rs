//! Inspect and edit existing local skill directories; acquisition is external.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use super::directory;
use crate::commands;
use bitrouter_ai::core::skills::{SkillConstraints, SkillSelector};
use bitrouter_ai::harness::skills::LocalScope;

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
            Ok(SkillSelector::Local {
                scope: if kind == "project" {
                    LocalScope::Project
                } else {
                    LocalScope::Global
                },
                directory_id: identity.into(),
            })
        }
        "mcp" => {
            let (server, uri) = identity.split_once(':').ok_or("use mcp:SERVER:URI")?;
            crate::control_output::validate_id(server).map_err(|error| error.to_string())?;
            crate::skills::protocol::validate_skill_resource_uri(uri)?;
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
    /// Inspect recorded explicit native skill bindings without executing.
    Explain(super::explain::Options),
    /// Discover skills already present in local directories.
    List(directory::ScopeArgs),
    /// Show a local skill's path, identity and format validation.
    Show {
        name: String,
        #[command(flatten)]
        scope: directory::ScopeArgs,
    },
    /// Create source SKILL.md only; acquisition is handled by npx skills.
    Init(InitArgs),
    /// Read a UTF-8 file inside an existing skill directory.
    Read {
        name: String,
        #[arg(long, default_value = "SKILL.md")]
        file: String,
        #[command(flatten)]
        scope: directory::ScopeArgs,
    },
    /// Atomically write a local skill file from FILE or '-' for stdin.
    Write {
        name: String,
        #[arg(long, default_value = "SKILL.md")]
        file: String,
        #[arg(long, value_name = "FILE|-")]
        from: PathBuf,
        #[command(flatten)]
        scope: directory::ScopeArgs,
    },
}
#[derive(Debug, clap::Args)]
pub struct InitArgs {
    pub name: String,
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,
}
pub async fn run(action: SkillsAction, output: &crate::output::Output) -> Result<()> {
    match action {
        SkillsAction::Explain(options) => output.emit(&super::explain::read(options).await?)?,
        SkillsAction::Init(args) => {
            let target = args
                .output
                .unwrap_or_else(|| PathBuf::from(&args.name).join("SKILL.md"));
            output.emit(&commands::skills_init(&args.name, &target)?)?;
        }
        SkillsAction::List(scope) => output.emit(&scope.list()?)?,
        SkillsAction::Show { name, scope } => output.emit(&scope.show(&name)?)?,
        SkillsAction::Read { name, file, scope } => {
            output.emit(&directory::read(&scope.find(&name)?, &file)?)?
        }
        SkillsAction::Write {
            name,
            file,
            from,
            scope,
        } => {
            use std::io::Read;
            let input: Box<dyn Read> = if from == std::path::Path::new("-") {
                Box::new(std::io::stdin())
            } else {
                Box::new(std::fs::File::open(from)?)
            };
            let mut bytes = Vec::new();
            input.take(256 * 1024 + 1).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 256 * 1024, "skill file exceeds 256 KiB");
            output.emit(&directory::write(
                &scope.find(&name)?,
                &file,
                &String::from_utf8(bytes)?,
            )?)?;
        }
    }
    Ok(())
}
