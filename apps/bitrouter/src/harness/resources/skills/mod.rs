//! Local skills are harness resources. Discovery publishes metadata; activation
//! and instruction placement are decisions of the managed core.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::MaterialRef;
use super::sha256;

pub mod format;

const MAX_SKILL_BYTES: u64 = 256 * 1024;
const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;
const MAX_SKILLS: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("SKILL.md has no YAML frontmatter block")]
    MissingFrontmatter,
    #[error("frontmatter parse error: {0}")]
    Frontmatter(String),
    #[error("invalid skill name {0:?}")]
    InvalidSkillName(String),
    #[error("io error: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn is_valid_skill_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

fn is_valid_skill_description(description: &str) -> bool {
    !description.trim().is_empty() && description.chars().count() <= 1024
}

pub fn validate_skill_name(name: &str) -> Result<()> {
    if !is_valid_skill_name(name) {
        return Err(Error::InvalidSkillName(name.into()));
    }
    Ok(())
}

fn read_skill(path: &Path) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("SKILL.md must be a regular file"));
    }
    file.take(MAX_SKILL_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SKILL_BYTES {
        return Err(std::io::Error::other("SKILL.md exceeds 256 KiB"));
    }
    String::from_utf8(bytes).map_err(|error| std::io::Error::other(error.to_string()))
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub material: MaterialRef,
}

/// A bounded, immutable metadata snapshot. Duplicate names from different
/// roots remain distinct; callers select by material ID, never by name alone.
#[derive(Default)]
pub struct SkillCatalog {
    pub skills: Vec<SkillMetadata>,
    pub problems: Vec<String>,
}

impl SkillCatalog {
    /// Roots are selected by the local host, not by model output. Each root
    /// includes its own SKILL.md and the conventional local skills layouts.
    pub fn discover(roots: &[PathBuf]) -> Result<Self> {
        if roots.len() > 32 {
            return Err(Error::Io(
                "skills discovery exceeds 32 selected roots".into(),
            ));
        }
        let mut catalog = Self::default();
        let mut seen = std::collections::BTreeSet::new();
        let mut bytes = 0usize;
        for root in roots {
            match root.canonicalize() {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(Error::Io(error.to_string())),
            }
            for found in format::discover_bounded(root)? {
                let path = found
                    .skill_md
                    .canonicalize()
                    .map_err(|e| Error::Io(e.to_string()))?;
                if !seen.insert(path.clone()) {
                    continue;
                }
                if seen.len() > MAX_SKILLS {
                    return Err(Error::Io(
                        "skills catalog exceeds 256 discovered entries".into(),
                    ));
                }
                if let Some(problem) = found.problem() {
                    catalog.problems.push(
                        format!("{}: {problem}", path.display())
                            .chars()
                            .take(1024)
                            .collect(),
                    );
                    continue;
                }
                // Read the exact version being advertised and validate those
                // exact bytes. A discovery/read race cannot mismatch metadata.
                if !format::is_safe_installed_path(root, &found.skill_md) {
                    return Err(Error::Io("skill path changed during discovery".into()));
                }
                let content = read_skill(&path).map_err(|e| Error::Io(e.to_string()))?;
                let parsed = format::parse_frontmatter(&content)?;
                let frozen = format::DiscoveredSkill {
                    dir: found.dir,
                    skill_md: found.skill_md,
                    frontmatter: Ok(parsed),
                };
                if let Some(problem) = frozen.problem() {
                    return Err(Error::Frontmatter(problem));
                }
                bytes = bytes.saturating_add(content.len());
                if bytes > MAX_CATALOG_BYTES {
                    return Err(Error::Io("skills catalog exceeds 16 MiB".into()));
                }
                let path_text = path
                    .to_str()
                    .ok_or_else(|| Error::Io("skill paths must be UTF-8".into()))?;
                let material_id = format!("skill_{}", sha256(path_text.as_bytes()));
                let digest = sha256(content.as_bytes());
                let material = MaterialRef {
                    material_id: material_id.clone(),
                    version: digest.clone(),
                    sha256: digest,
                    media_type: "text/markdown".into(),
                    provenance: format!("local_skill:{}", path.display()),
                };
                catalog.skills.push(SkillMetadata {
                    name: frozen.name(),
                    description: frozen.description().into(),
                    material,
                });
            }
        }
        catalog.skills.sort_by(|a, b| {
            (&a.name, &a.material.material_id).cmp(&(&b.name, &b.material.material_id))
        });
        Ok(catalog)
    }
}
