//! The skills-introspection adapter — the app side of the orchestrator
//! profile's `skills_search` / `skills_get` tools (TUI_SPEC §4, PR-2 B2).
//!
//! Implements `bitrouter-mcp`'s
//! [`SkillsQuery`](bitrouter_mcp::capabilities::skills::SkillsQuery) port over
//! the installed-skills root, using `bitrouter_skills`' discovery. Read-only.

use std::path::PathBuf;

use bitrouter_mcp::capabilities::skills::SkillsQuery;
use bitrouter_mcp::error::ToolError;
use bitrouter_skills::frontmatter::discover_all_skills;

/// Searches and fetches installed skills under a root directory.
/// `discover_all_skills` searches `root`, `root/skills`, and
/// `root/.claude/skills`, so the project root covers the conventional layouts.
pub struct InstalledSkills {
    root: PathBuf,
}

impl InstalledSkills {
    /// Query skills installed under `root` (typically the base repo).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait::async_trait]
impl SkillsQuery for InstalledSkills {
    async fn search(&self, query: &str) -> Result<serde_json::Value, ToolError> {
        // `discover_all_skills` walks the filesystem synchronously; run it off
        // the async runtime so a large skills tree can't stall the reactor
        // (PR-2 review finding 6).
        let root = self.root.clone();
        let needle = query.to_lowercase();
        let skills = tokio::task::spawn_blocking(move || {
            discover_all_skills(&root)
                .into_iter()
                .filter(|(_, fm)| {
                    fm.name.to_lowercase().contains(&needle)
                        || fm.description.to_lowercase().contains(&needle)
                })
                .map(|(path, fm)| {
                    serde_json::json!({
                        "name": fm.name,
                        "description": fm.description,
                        "path": path.display().to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| ToolError::new(format!("skills discovery task failed: {e}")))?;
        Ok(serde_json::json!({ "skills": skills }))
    }

    async fn get(&self, name: &str) -> Result<serde_json::Value, ToolError> {
        // Both the walk and the `read_to_string` are blocking fs work — do them
        // on the blocking pool (PR-2 review finding 6).
        let root = self.root.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let (path, fm) = discover_all_skills(&root)
                .into_iter()
                .find(|(_, fm)| fm.name == name)
                .ok_or_else(|| ToolError::new(format!("no installed skill named '{name}'")))?;
            let content = std::fs::read_to_string(&path)
                .map_err(|e| ToolError::new(format!("reading {}: {e}", path.display())))?;
            Ok(serde_json::json!({
                "name": fm.name,
                "description": fm.description,
                "metadata": fm.metadata,
                "path": path.display().to_string(),
                "body": skill_body(&content),
            }))
        })
        .await
        .map_err(|e| ToolError::new(format!("skills fetch task failed: {e}")))?
    }
}

/// The SKILL.md markdown body — the content after the leading `---` … `---`
/// YAML frontmatter fence. Falls back to the whole file when there is no
/// recognizable frontmatter (a superset beats a lost body).
fn skill_body(content: &str) -> String {
    let mut lines = content.lines();
    if lines.next().map(|l| l.trim_end_matches('\r')) != Some("---") {
        return content.to_string();
    }
    let mut closed = false;
    let mut body = String::new();
    for line in lines {
        if !closed {
            if line.trim_end_matches('\r') == "---" {
                closed = true;
            }
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    if closed {
        body.trim_start_matches(['\n', '\r']).to_string()
    } else {
        content.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_skill(root: &std::path::Path, name: &str, description: &str, body: &str) {
        let dir = root.join(".claude").join("skills").join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}"),
        )
        .expect("write skill");
    }

    #[tokio::test]
    async fn search_matches_name_and_description() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_skill(dir.path(), "refactor-rust", "Rustacean refactors", "# Body");
        install_skill(dir.path(), "write-docs", "Author documentation", "# Body");
        let skills = InstalledSkills::new(dir.path().to_path_buf());

        // Match by name.
        let by_name = skills.search("refactor").await.expect("search");
        let hits = by_name["skills"].as_array().expect("array");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["name"], "refactor-rust");

        // Match by description (case-insensitive).
        let by_desc = skills.search("DOCUMENTATION").await.expect("search");
        assert_eq!(by_desc["skills"].as_array().expect("array").len(), 1);
    }

    #[tokio::test]
    async fn get_returns_frontmatter_and_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_skill(
            dir.path(),
            "alpha",
            "Does alpha",
            "# Alpha\n\nRun the thing.",
        );
        let skills = InstalledSkills::new(dir.path().to_path_buf());

        let got = skills.get("alpha").await.expect("get");
        assert_eq!(got["name"], "alpha");
        assert_eq!(got["description"], "Does alpha");
        let body = got["body"].as_str().expect("body");
        assert!(
            body.starts_with("# Alpha"),
            "body without frontmatter: {body:?}"
        );
        assert!(
            !body.contains("description:"),
            "frontmatter stripped: {body:?}"
        );
    }

    #[tokio::test]
    async fn get_missing_skill_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skills = InstalledSkills::new(dir.path().to_path_buf());
        let err = skills.get("nope").await.expect_err("not found");
        assert!(err.0.contains("nope"), "names the missing skill: {}", err.0);
    }
}
