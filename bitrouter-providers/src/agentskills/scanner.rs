//! Filesystem scanner for `SKILL.md` files.
//!
//! Scans a directory for subdirectories containing `SKILL.md`, parses YAML
//! frontmatter, and builds [`SkillCatalogEntry`] values for the in-memory catalog.

use std::path::Path;

use serde::Deserialize;

use super::catalog::SkillCatalogEntry;

/// YAML frontmatter fields extracted from a `SKILL.md` file.
#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    required_apis: Vec<String>,
}

/// Scan a directory for skill subdirectories containing `SKILL.md`.
///
/// Creates the directory if it does not exist. Returns an empty list on I/O errors
/// rather than propagating — skill scanning is best-effort.
pub(crate) async fn scan_skills_dir(dir: &Path) -> Result<Vec<SkillCatalogEntry>, String> {
    if !dir.exists() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| format!("failed to create skills directory {}: {e}", dir.display()))?;
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| format!("failed to read skills directory {}: {e}", dir.display()))?;

    while let Some(dir_entry) = read_dir
        .next_entry()
        .await
        .map_err(|e| format!("failed to read directory entry: {e}"))?
    {
        let path = dir_entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        match parse_skill_md(&skill_md, &path).await {
            Ok(entry) => entries.push(entry),
            Err(reason) => {
                tracing::warn!(
                    path = %skill_md.display(),
                    "skipping malformed skill: {reason}"
                );
            }
        }
    }

    Ok(entries)
}

/// Parse a single `SKILL.md` file into a catalog entry.
pub(crate) async fn parse_skill_md(
    skill_md: &Path,
    skill_dir: &Path,
) -> Result<SkillCatalogEntry, String> {
    let content = tokio::fs::read_to_string(skill_md)
        .await
        .map_err(|e| format!("read error: {e}"))?;

    let (frontmatter, _body) = split_frontmatter(&content);

    let fm: SkillFrontmatter = match frontmatter {
        Some(yaml) => serde_saphyr::from_str(yaml).map_err(|e| format!("YAML parse error: {e}"))?,
        None => {
            return Err("missing YAML frontmatter".to_string());
        }
    };

    // Fall back to directory name if name is not in frontmatter.
    let name = fm.name.unwrap_or_else(|| {
        skill_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });

    let description = fm
        .description
        .filter(|d| !d.is_empty())
        .ok_or_else(|| "missing or empty description".to_string())?;

    // Use file metadata for timestamps.
    let metadata = tokio::fs::metadata(skill_md)
        .await
        .map_err(|e| format!("metadata error: {e}"))?;

    let updated_at = metadata
        .modified()
        .ok()
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_default();

    let created_at = metadata
        .created()
        .ok()
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_else(|| updated_at.clone());

    let id = uuid::Uuid::new_v4().to_string();

    Ok(SkillCatalogEntry {
        id,
        name,
        description,
        source: "filesystem".to_string(),
        required_apis: fm.required_apis,
        path: skill_md.to_path_buf(),
        created_at,
        updated_at,
        bound_tool: None,
    })
}

/// Split a SKILL.md file into optional YAML frontmatter and body.
///
/// Frontmatter is delimited by `---` at the start and a second `---`.
fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, content);
    }

    // Skip the opening `---` line.
    let after_open = match trimmed[3..].find('\n') {
        Some(pos) => &trimmed[3 + pos + 1..],
        None => return (None, content),
    };

    // Find the closing `---`.
    match after_open.find("\n---") {
        Some(pos) => {
            let yaml = &after_open[..pos];
            let body_start = pos + 4; // skip "\n---"
            let body = if body_start < after_open.len() {
                // Skip the newline after closing ---
                let rest = &after_open[body_start..];
                rest.strip_prefix('\n').unwrap_or(rest)
            } else {
                ""
            };
            (Some(yaml), body)
        }
        None => (None, content),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn split_frontmatter_valid() {
        let content = "---\nname: test\ndescription: hello\n---\n# Body\nsome content";
        let (fm, body) = split_frontmatter(content);
        assert_eq!(fm, Some("name: test\ndescription: hello"));
        assert_eq!(body, "# Body\nsome content");
    }

    #[test]
    fn split_frontmatter_missing() {
        let content = "# No frontmatter\njust body";
        let (fm, body) = split_frontmatter(content);
        assert!(fm.is_none());
        assert_eq!(body, content);
    }

    #[test]
    fn split_frontmatter_unclosed() {
        let content = "---\nname: test\nno closing delimiter";
        let (fm, _body) = split_frontmatter(content);
        assert!(fm.is_none());
    }

    #[tokio::test]
    async fn scan_empty_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let entries = scan_skills_dir(tmp.path()).await.expect("scan");
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn scan_creates_missing_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let skills_dir = tmp.path().join("skills");
        let entries = scan_skills_dir(&skills_dir).await.expect("scan");
        assert!(entries.is_empty());
        assert!(skills_dir.exists());
    }

    #[tokio::test]
    async fn scan_valid_skill() {
        let tmp = TempDir::new().expect("tempdir");
        let skill_dir = tmp.path().join("code-review");
        std::fs::create_dir(&skill_dir).expect("mkdir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: code-review\ndescription: Reviews code for quality\n---\n# Instructions\nReview the code.",
        )
        .expect("write");

        let entries = scan_skills_dir(tmp.path()).await.expect("scan");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "code-review");
        assert_eq!(entries[0].description, "Reviews code for quality");
        assert_eq!(entries[0].source, "filesystem");
    }

    #[tokio::test]
    async fn scan_falls_back_to_dir_name() {
        let tmp = TempDir::new().expect("tempdir");
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir(&skill_dir).expect("mkdir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: A skill without a name field\n---\nBody.",
        )
        .expect("write");

        let entries = scan_skills_dir(tmp.path()).await.expect("scan");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "my-skill");
    }

    #[tokio::test]
    async fn scan_skips_missing_description() {
        let tmp = TempDir::new().expect("tempdir");
        let skill_dir = tmp.path().join("bad-skill");
        std::fs::create_dir(&skill_dir).expect("mkdir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: bad-skill\n---\nNo description in frontmatter.",
        )
        .expect("write");

        let entries = scan_skills_dir(tmp.path()).await.expect("scan");
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn scan_skips_non_skill_dirs() {
        let tmp = TempDir::new().expect("tempdir");
        // Regular file, not a directory
        std::fs::write(tmp.path().join("README.md"), "not a skill").expect("write");
        // Directory without SKILL.md
        std::fs::create_dir(tmp.path().join("empty-dir")).expect("mkdir");

        let entries = scan_skills_dir(tmp.path()).await.expect("scan");
        assert!(entries.is_empty());
    }
}
