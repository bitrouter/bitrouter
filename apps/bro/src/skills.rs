//! Local Agent Skills. A directory operation never opens a gateway or a thread store.
mod directory;

use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use directory::ScopeArgs;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// Inspect the skill revisions supplied to a recorded local turn.
    Explain {
        #[arg(long)]
        thread: uuid::Uuid,
        #[arg(long)]
        turn: uuid::Uuid,
    },
    /// Discover local skill directories and report frontmatter errors.
    List(ScopeArgs),
    /// Describe a skill and its source identity.
    Show {
        name: String,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Read a bounded UTF-8 material from one selected skill.
    Read {
        name: String,
        #[arg(long, default_value = "SKILL.md")]
        file: String,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Replace a material with UTF-8 input from FILE or standard input.
    Write {
        name: String,
        #[arg(long, default_value = "SKILL.md")]
        file: String,
        #[arg(long, value_name = "FILE|-")]
        from: PathBuf,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Create a new SKILL.md in a same-named directory.
    Init {
        name: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

fn global_roots() -> Result<Vec<PathBuf>> {
    let home = std::env::home_dir().context("user home unavailable")?;
    let mut roots = vec![
        home.join(".agents"),
        home.join(".claude"),
        home.join(".codex"),
    ];
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        roots.push(path.into());
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn directory_id(path: &Path) -> String {
    format!(
        "skill_{}",
        hex::encode(Sha256::digest(path.to_string_lossy().as_bytes()))
    )
}

pub(crate) fn run(action: Action, data_dir: Option<PathBuf>) -> Result<()> {
    match action {
        Action::Explain { thread, turn } => {
            let root = crate::data_dir(data_dir)?;
            ensure!(root.is_dir(), "BRO home unavailable");
            let saved = crate::checkpoint::Home::open(&root)?.load(thread)?;
            ensure!(
                saved.turns.iter().any(|t| t.turn_id == turn),
                "unknown turn"
            );
            crate::emit(
                &json!({"thread":thread,"turn":turn,"skills":saved.skills.iter().map(Selection::identity).collect::<Vec<_>>() }),
            )
        }
        Action::List(scope) => crate::emit(&scope.list()?),
        Action::Show { name, scope } => crate::emit(&scope.show(&name)?),
        Action::Read { name, file, scope } => {
            crate::emit(&directory::read(&scope.find(&name)?, &file)?)
        }
        Action::Write {
            name,
            file,
            from,
            scope,
        } => {
            let input: Box<dyn Read> = if from == Path::new("-") {
                Box::new(std::io::stdin())
            } else {
                Box::new(File::open(from)?)
            };
            let mut bytes = Vec::new();
            input.take(256 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 256 * 1024, "skill file exceeds 256 KiB");
            crate::emit(&directory::write(
                &scope.find(&name)?,
                &file,
                std::str::from_utf8(&bytes)?,
            )?)
        }
        Action::Init { name, output } => {
            bitrouter_skills::validate_skill_name(&name)?;
            let path = std::path::absolute(
                output.unwrap_or_else(|| PathBuf::from(&name).join("SKILL.md")),
            )?;
            let root = path.parent().context("skill directory missing")?;
            ensure!(
                path.file_name().and_then(|v| v.to_str()) == Some("SKILL.md")
                    && root.file_name().and_then(|v| v.to_str()) == Some(&name),
                "SKILL.md must be under the same-named directory"
            );
            fs::create_dir_all(root)?;
            let root = root.canonicalize()?;
            let path = root.join("SKILL.md");
            let body = format!(
                "---\nname: {name}\ndescription: Describe when to use this skill.\n---\n\n# {name}\n"
            );
            publish(&root, &path, &body, true)?;
            crate::emit(&json!({"created":true,"path":path}))
        }
    }
}

fn publish(root: &Path, path: &Path, content: &str, new: bool) -> Result<()> {
    let parent = path.parent().context("missing skill directory")?;
    ensure!(
        parent.canonicalize()? == parent && parent.starts_with(root),
        "skill directory moved"
    );
    match fs::symlink_metadata(path) {
        Ok(_) => {
            ensure!(!new, "skill already exists");
            bitrouter_skills::read_regular(root, path, 256 * 1024)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, CWD, Mode, OFlags, linkat, openat, renameat, unlinkat};
        let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW;
        let mut dir = openat(CWD, root, flags, Mode::empty())?;
        for component in parent.strip_prefix(root)?.components() {
            dir = openat(&dir, component.as_os_str(), flags, Mode::empty())?;
        }
        let name = path.file_name().context("missing file name")?;
        let temp = format!(".bro-skill-{}", uuid::Uuid::new_v4());
        let fd = openat(
            &dir,
            temp.as_str(),
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let result = (|| -> Result<()> {
            let mut f = File::from(fd);
            f.write_all(content.as_bytes())?;
            f.sync_all()?;
            if new {
                linkat(&dir, temp.as_str(), &dir, name, AtFlags::empty())?;
                unlinkat(&dir, temp.as_str(), AtFlags::empty())?;
            } else {
                renameat(&dir, temp.as_str(), &dir, name)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(&dir, temp.as_str(), AtFlags::empty());
        }
        result?;
        File::from(dir).sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(content.as_bytes())?;
        temporary.as_file().sync_all()?;
        if new {
            temporary.persist_noclobber(path)?;
        } else {
            temporary.persist(path)?;
        }
    }
    Ok(())
}

/// A user-selected skill is frozen at thread creation. Loading a checkpoint
/// checks the retained bytes; editing the source directory affects future threads.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    name: String,
    directory_id: String,
    sha256: String,
    content: String,
}

pub(crate) fn select(workspace: &Path, names: &[String]) -> Result<Vec<Selection>> {
    ensure!(names.len() <= 16, "at most sixteen skills per thread");
    let scope = ScopeArgs {
        workspace: Some(workspace.to_owned()),
        ..Default::default()
    };
    let mut selected = Vec::new();
    for name in names {
        let row = scope.find(name)?;
        ensure!(row.valid, "invalid skill {}", row.name);
        let content = directory::read(&row, "SKILL.md")?.content;
        let selection = Selection {
            directory_id: directory_id(&row.directory),
            name: row.name,
            sha256: hex::encode(Sha256::digest(content.as_bytes())),
            content,
        };
        selected.push(selection);
        validate(&selected)?;
    }
    Ok(selected)
}

pub(crate) fn validate(skills: &[Selection]) -> Result<()> {
    use std::collections::BTreeSet;
    ensure!(skills.len() <= 16, "invalid skill count");
    let mut seen = BTreeSet::new();
    let mut bytes = 0usize;
    for skill in skills {
        bytes = bytes
            .checked_add(skill.content.len())
            .context("skill size overflow")?;
        ensure!(bytes <= 256 * 1024, "selected skills exceed 256 KiB");
        ensure!(seen.insert(&skill.directory_id), "duplicate selected skill");
        ensure!(
            hex::encode(Sha256::digest(skill.content.as_bytes())) == skill.sha256,
            "skill checkpoint digest mismatch"
        );
        let fm = bitrouter_skills::parse_frontmatter(&skill.content)?;
        fm.validate()?;
        ensure!(fm.name == skill.name, "skill name changed");
    }
    Ok(())
}

impl Selection {
    pub(crate) fn message(&self) -> serde_json::Value {
        json!({"role":"user","content":format!("Selected local skill {} (sha256 {}).\n{}", self.name, self.sha256, self.content)})
    }
    fn identity(&self) -> serde_json::Value {
        json!({"name":self.name,"directory_id":self.directory_id,"sha256":self.sha256})
    }
}
