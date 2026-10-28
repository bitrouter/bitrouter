//! Direct file operations in a selected local skill directory.
use crate::skills::format;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Default, clap::Args)]
pub struct ScopeArgs {
    /// Inspect user-global harness skill directories instead of this project.
    #[arg(long, short='g', conflicts_with_all=["workspace","root"])]
    pub global: bool,
    /// Project directory whose conventional skill directories are inspected.
    #[arg(long, conflicts_with_all=["global","root"])]
    pub workspace: Option<PathBuf>,
    /// One explicit skills directory; no configuration or index is written.
    #[arg(long, conflicts_with_all=["global","workspace"])]
    pub root: Option<PathBuf>,
}
impl ScopeArgs {
    fn roots(&self) -> Result<Vec<PathBuf>> {
        if let Some(root) = &self.root {
            return Ok(vec![root.canonicalize()?]);
        }
        if self.global {
            return super::native::global_roots();
        }
        Ok(vec![
            self.workspace
                .as_deref()
                .map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))?
                .canonicalize()?,
        ])
    }
    pub fn list(&self) -> Result<Report> {
        let mut rows = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for root in self.roots()? {
            if !root.try_exists()? {
                continue;
            }
            for found in format::discover_bounded(&root)? {
                let directory = found.dir.canonicalize()?;
                if !seen.insert(directory.clone()) {
                    continue;
                }
                rows.push(Row {
                    name: found.name(),
                    description: found.description().into(),
                    directory_id: Some(super::native::directory_id(&directory)),
                    directory,
                    valid: found.problem().is_none(),
                    problem: found.problem(),
                });
            }
        }
        rows.sort_by(|a, b| (&a.name, &a.directory).cmp(&(&b.name, &b.directory)));
        Ok(Report {
            operation: "list",
            skills: rows,
        })
    }
    pub fn show(&self, name: &str) -> Result<Report> {
        Ok(Report {
            operation: "show",
            skills: vec![self.find(name)?],
        })
    }
    pub fn find(&self, name: &str) -> Result<Row> {
        let matches: Vec<_> = self
            .list()?
            .skills
            .into_iter()
            .filter(|row| {
                row.name == name
                    || row.directory_id.as_deref() == Some(name)
                    || row.directory.to_str() == Some(name)
            })
            .collect();
        ensure!(
            matches.len() == 1,
            "skill name is missing or ambiguous; select --root or an exact directory identity"
        );
        matches.into_iter().next().context("skill not found")
    }
}
#[derive(Serialize)]
pub struct Row {
    pub name: String,
    pub description: String,
    pub directory: PathBuf,
    pub directory_id: Option<String>,
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}
#[derive(Serialize)]
pub struct Report {
    operation: &'static str,
    skills: Vec<Row>,
}
impl crate::output::CliReport for Report {
    fn render(&self, h: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        for row in &self.skills {
            h.line(&format!(
                "{}  {}  {}{}",
                row.name,
                row.directory_id.as_deref().unwrap_or_default(),
                row.directory.display(),
                row.problem
                    .as_ref()
                    .map(|problem| format!(" (unusable: {problem})"))
                    .unwrap_or_default()
            ))?;
        }
        Ok(())
    }
}
#[derive(Serialize)]
pub struct FileReport {
    pub operation: &'static str,
    pub directory: PathBuf,
    pub file: String,
    pub content: String,
}
impl crate::output::CliReport for FileReport {
    fn render(&self, h: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        h.line(&self.content)
    }
}
pub fn read(row: &Row, reference: &str) -> Result<FileReport> {
    let path = row.directory.join(super::files::relative(reference)?);
    let bytes = super::files::read_regular(&row.directory, &path, 256 * 1024)?;
    Ok(FileReport {
        operation: "read",
        directory: row.directory.clone(),
        file: reference.into(),
        content: String::from_utf8(bytes).context("skill file requires UTF-8")?,
    })
}
pub fn write(row: &Row, reference: &str, content: &str) -> Result<WriteReport> {
    ensure!(content.len() <= 256 * 1024, "skill file exceeds 256 KiB");
    let relative = super::files::relative(reference)?;
    let path = row.directory.join(relative);
    let parent = path.parent().context("skill file parent")?;
    ensure!(
        parent.canonicalize()?.starts_with(&row.directory),
        "skill file escapes its directory"
    );
    if reference == "SKILL.md" {
        let fm = format::parse_frontmatter(content)?;
        fm.validate()?;
        ensure!(
            row.directory.file_name().and_then(|name| name.to_str()) == Some(fm.name.as_str()),
            "skill directory and frontmatter name must match"
        );
    }
    let _lock = crate::policy_lock::acquire_publication_lock(&path)?;
    if path.try_exists()? {
        let before = read(row, reference)?;
        crate::policy_lock::write_text_atomic_unlocked(&path, &before.content, content)?;
    } else {
        crate::policy_lock::write_new_file_atomic(&path, content.as_bytes())?;
    }
    Ok(WriteReport {
        operation: "write",
        directory: row.directory.clone(),
        file: reference.into(),
        bytes: content.len(),
    })
}

#[derive(Serialize)]
pub struct WriteReport {
    operation: &'static str,
    directory: PathBuf,
    file: String,
    bytes: usize,
}
impl crate::output::CliReport for WriteReport {
    fn render(&self, h: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        h.line(&format!(
            "Wrote {} ({} bytes)",
            self.directory.join(&self.file).display(),
            self.bytes
        ))
    }
}
