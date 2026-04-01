//! Filesystem-backed skill registry.
//!
//! Implements [`SkillService`] and [`ToolRegistry`] against an in-memory
//! catalog populated from `SKILL.md` files on disk. Skills can also be
//! installed at runtime from remote registries via HTTP.

use std::path::PathBuf;
use std::sync::Arc;

use bitrouter_core::routers::registry::{SkillEntry, SkillService};
use bitrouter_core::routers::registry::{ToolEntry, ToolRegistry};
use tokio::sync::RwLock;

use super::catalog::SkillCatalogEntry;
use super::installer;
use super::scanner;

/// A filesystem-backed skill registry.
///
/// Follows the [agentskills.io](https://agentskills.io) client-side model:
/// - **Startup**: scans `skills_dir` for `SKILL.md` files, builds in-memory catalog.
/// - **Runtime**: install/uninstall write to/remove from disk and update the catalog.
/// - **No database**: all persistence is the filesystem.
pub struct FilesystemSkillRegistry {
    catalog: Arc<RwLock<Vec<SkillCatalogEntry>>>,
    skills_dir: PathBuf,
    http_client: reqwest::Client,
}

impl Default for FilesystemSkillRegistry {
    fn default() -> Self {
        Self {
            catalog: Arc::new(RwLock::new(Vec::new())),
            skills_dir: PathBuf::from("skills"),
            http_client: reqwest::Client::new(),
        }
    }
}

impl FilesystemSkillRegistry {
    /// Create a registry by scanning an existing skills directory.
    pub async fn from_dir(skills_dir: PathBuf) -> Result<Self, String> {
        let entries = scanner::scan_skills_dir(&skills_dir).await?;
        Ok(Self {
            catalog: Arc::new(RwLock::new(entries)),
            skills_dir,
            http_client: reqwest::Client::new(),
        })
    }

    /// Create a registry from config-declared tools (those with a `skill` field)
    /// merged with a filesystem scan.
    ///
    /// Config tools that don't have a `SKILL.md` on disk get a minimal one
    /// generated and written. Filesystem skills not in config are also included.
    pub async fn from_config_and_dir(
        tool_configs: Vec<(String, bitrouter_config::ToolConfig)>,
        skills_dir: PathBuf,
    ) -> Result<Self, String> {
        // Scan existing filesystem skills first.
        let mut entries = scanner::scan_skills_dir(&skills_dir).await?;

        // Merge config-declared tools: resolve each skill reference and bind.
        //
        // The `skill` string uses a prefix convention:
        //   - `github:owner/repo/skill-name` → remote ref (not fetched, recorded as-is)
        //   - `./path` or `../path` or `/abs/path` → local path to a skill directory
        //   - `bare-name` → resolved against `skills_dir`
        for (tool_name, tool_config) in tool_configs {
            let skill_ref = match tool_config.skill.as_deref() {
                Some(s) => s,
                None => continue,
            };

            if skill_ref.starts_with("github:") {
                // Remote ref — don't fetch, create an in-memory entry.
                let skill_name = skill_ref
                    .rsplit('/')
                    .next()
                    .unwrap_or(skill_ref)
                    .to_string();
                let now = chrono::Utc::now().to_rfc3339();
                entries.push(SkillCatalogEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: skill_name,
                    description: tool_config.description.clone().unwrap_or_default(),
                    source: skill_ref.to_string(),
                    required_apis: tool_config
                        .endpoints
                        .iter()
                        .map(|ep| ep.provider.clone())
                        .collect(),
                    path: PathBuf::new(),
                    created_at: now.clone(),
                    updated_at: now,
                    bound_tool: Some(tool_name),
                });
                continue;
            }

            if skill_ref.starts_with("./")
                || skill_ref.starts_with("../")
                || std::path::Path::new(skill_ref).is_absolute()
            {
                // Local path — read SKILL.md from the specified directory.
                let local_dir = PathBuf::from(skill_ref);
                let local_md = local_dir.join("SKILL.md");
                match scanner::parse_skill_md(&local_md, &local_dir).await {
                    Ok(mut entry) => {
                        entry.bound_tool = Some(tool_name);
                        entries.push(entry);
                    }
                    Err(reason) => {
                        tracing::warn!(
                            tool = %tool_name,
                            path = %local_dir.display(),
                            "failed to load skill from local path: {reason}"
                        );
                    }
                }
                continue;
            }

            // Bare name — resolve against skills_dir.
            let skill_name = skill_ref;

            // Match by frontmatter name or by directory name.
            let existing = entries.iter_mut().find(|e| {
                e.name == skill_name
                    || e.path
                        .parent()
                        .and_then(|p| p.file_name())
                        .is_some_and(|d| d == skill_name)
            });

            if let Some(entry) = existing {
                entry.bound_tool = Some(tool_name);
                continue;
            }

            // Not on disk — create a stub entry (write SKILL.md only with a description).
            let required_apis: Vec<String> = tool_config
                .endpoints
                .iter()
                .map(|ep| ep.provider.clone())
                .collect();

            let description = tool_config.description.clone().unwrap_or_default();

            let skill_dir_path = skills_dir.join(skill_name);
            let skill_md = skill_dir_path.join("SKILL.md");
            if !description.is_empty() {
                let escaped_desc = description.replace('"', "\\\"");
                let mut yaml_parts = vec![
                    format!("name: \"{skill_name}\""),
                    format!("description: \"{escaped_desc}\""),
                ];
                if !required_apis.is_empty() {
                    yaml_parts.push("required_apis:".to_string());
                    for api in &required_apis {
                        yaml_parts.push(format!("  - \"{api}\""));
                    }
                }

                let content = format!("---\n{}\n---\n", yaml_parts.join("\n"));

                tokio::fs::create_dir_all(&skill_dir_path)
                    .await
                    .map_err(|e| format!("failed to create {}: {e}", skill_dir_path.display()))?;
                tokio::fs::write(&skill_md, &content)
                    .await
                    .map_err(|e| format!("failed to write {}: {e}", skill_md.display()))?;
            }

            let now = chrono::Utc::now().to_rfc3339();
            entries.push(SkillCatalogEntry {
                id: uuid::Uuid::new_v4().to_string(),
                name: skill_name.to_string(),
                description,
                source: "config".to_string(),
                required_apis,
                path: skill_md,
                created_at: now.clone(),
                updated_at: now,
                bound_tool: Some(tool_name),
            });
        }

        Ok(Self {
            catalog: Arc::new(RwLock::new(entries)),
            skills_dir,
            http_client: reqwest::Client::new(),
        })
    }
}

impl SkillService for FilesystemSkillRegistry {
    /// Install a skill.
    ///
    /// If `source` is a URL (starts with `http://` or `https://`), fetches the
    /// `SKILL.md` content from that URL. Otherwise generates a minimal `SKILL.md`
    /// from the provided name and description. Writes to disk and adds to catalog.
    async fn create(
        &self,
        name: String,
        description: String,
        source: Option<String>,
        required_apis: Vec<String>,
    ) -> Result<SkillEntry, String> {
        // Check for duplicates.
        {
            let catalog = self.catalog.read().await;
            if catalog.iter().any(|e| e.name == name) {
                return Err(format!("skill '{name}' already exists"));
            }
        }

        let content = match source.as_deref() {
            Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
                installer::fetch_skill(&self.http_client, url).await?
            }
            _ => {
                // Generate a minimal SKILL.md.
                let mut yaml_parts = vec![
                    format!("name: \"{name}\""),
                    format!("description: \"{description}\""),
                ];
                if !required_apis.is_empty() {
                    yaml_parts.push("required_apis:".to_string());
                    for api in &required_apis {
                        yaml_parts.push(format!("  - \"{api}\""));
                    }
                }
                format!("---\n{}\n---\n", yaml_parts.join("\n"))
            }
        };

        // Write to disk.
        let skill_dir = self.skills_dir.join(&name);
        let skill_md = skill_dir.join("SKILL.md");
        tokio::fs::create_dir_all(&skill_dir)
            .await
            .map_err(|e| format!("failed to create {}: {e}", skill_dir.display()))?;
        tokio::fs::write(&skill_md, &content)
            .await
            .map_err(|e| format!("failed to write {}: {e}", skill_md.display()))?;

        let now = chrono::Utc::now().to_rfc3339();
        let entry = SkillCatalogEntry {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            description,
            source: source.unwrap_or_else(|| "manual".to_string()),
            required_apis,
            path: skill_md,
            created_at: now.clone(),
            updated_at: now,
            bound_tool: None,
        };

        let skill_entry = entry.to_skill_entry();
        self.catalog.write().await.push(entry);
        Ok(skill_entry)
    }

    async fn list(&self) -> Result<Vec<SkillEntry>, String> {
        let catalog = self.catalog.read().await;
        Ok(catalog
            .iter()
            .map(SkillCatalogEntry::to_skill_entry)
            .collect())
    }

    async fn get(&self, name: &str) -> Result<Option<SkillEntry>, String> {
        let catalog = self.catalog.read().await;
        Ok(catalog
            .iter()
            .find(|e| e.name == name)
            .map(SkillCatalogEntry::to_skill_entry))
    }

    /// Uninstall a skill: remove from catalog and delete from disk.
    async fn delete(&self, name: &str) -> Result<bool, String> {
        let removed = {
            let mut catalog = self.catalog.write().await;
            catalog
                .iter()
                .position(|e| e.name == name)
                .map(|idx| catalog.remove(idx))
        };

        let Some(entry) = removed else {
            return Ok(false);
        };

        // Remove the skill directory from disk.
        let skill_dir = entry
            .path
            .parent()
            .ok_or_else(|| format!("invalid skill path: {}", entry.path.display()))?;

        if skill_dir.exists() {
            tokio::fs::remove_dir_all(skill_dir)
                .await
                .map_err(|e| format!("failed to remove {}: {e}", skill_dir.display()))?;
        }

        Ok(true)
    }
}

impl ToolRegistry for FilesystemSkillRegistry {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        let catalog = self.catalog.read().await;
        catalog
            .iter()
            .map(SkillCatalogEntry::to_tool_entry)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn from_dir_empty() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");
        let list = reg.list().await.expect("list");
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn create_and_list() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        let entry = reg
            .create(
                "test-skill".to_string(),
                "A test skill".to_string(),
                None,
                vec!["openai".to_string()],
            )
            .await
            .expect("create");

        assert_eq!(entry.name, "test-skill");
        assert_eq!(entry.description, "A test skill");
        assert_eq!(entry.required_apis, vec!["openai"]);

        // Verify file was written.
        let skill_md = tmp.path().join("test-skill").join("SKILL.md");
        assert!(skill_md.exists());

        // Verify it appears in list.
        let list = reg.list().await.expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test-skill");
    }

    #[tokio::test]
    async fn get_existing_and_missing() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        reg.create(
            "my-skill".to_string(),
            "Does things".to_string(),
            None,
            vec![],
        )
        .await
        .expect("create");

        let found = reg.get("my-skill").await.expect("get");
        assert!(found.is_some());
        assert_eq!(found.as_ref().map(|e| e.name.as_str()), Some("my-skill"));

        let missing = reg.get("nonexistent").await.expect("get");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn delete_existing() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        reg.create(
            "doomed".to_string(),
            "Will be deleted".to_string(),
            None,
            vec![],
        )
        .await
        .expect("create");

        let skill_dir = tmp.path().join("doomed");
        assert!(skill_dir.exists());

        let deleted = reg.delete("doomed").await.expect("delete");
        assert!(deleted);
        assert!(!skill_dir.exists());

        let list = reg.list().await.expect("list");
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn delete_nonexistent() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        let deleted = reg.delete("nope").await.expect("delete");
        assert!(!deleted);
    }

    #[tokio::test]
    async fn create_duplicate_rejected() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        reg.create("dup".to_string(), "first".to_string(), None, vec![])
            .await
            .expect("create");

        let err = reg
            .create("dup".to_string(), "second".to_string(), None, vec![])
            .await;
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("already exists"));
    }

    #[tokio::test]
    async fn tool_registry_integration() {
        let tmp = TempDir::new().expect("tempdir");
        let reg = FilesystemSkillRegistry::from_dir(tmp.path().to_path_buf())
            .await
            .expect("from_dir");

        reg.create("my-tool".to_string(), "A tool".to_string(), None, vec![])
            .await
            .expect("create");

        let tools = reg.list_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].id, "skill/my-tool");
        assert_eq!(tools[0].provider, "skill");
        assert_eq!(tools[0].definition.name, "my-tool");
    }

    #[tokio::test]
    async fn from_config_and_dir_bare_name() {
        let tmp = TempDir::new().expect("tempdir");

        // Pre-create a filesystem skill.
        let fs_skill_dir = tmp.path().join("fs-skill");
        std::fs::create_dir(&fs_skill_dir).expect("mkdir");
        std::fs::write(
            fs_skill_dir.join("SKILL.md"),
            "---\nname: fs-skill\ndescription: From filesystem\n---\n",
        )
        .expect("write");

        // Bare name: tool references a skill not yet on disk.
        let configs = vec![(
            "cfg-tool".to_string(),
            bitrouter_config::ToolConfig {
                skill: Some("cfg-skill".to_string()),
                description: Some("A config skill".to_string()),
                ..Default::default()
            },
        )];

        let reg = FilesystemSkillRegistry::from_config_and_dir(configs, tmp.path().to_path_buf())
            .await
            .expect("from_config_and_dir");

        let list = reg.list().await.expect("list");
        assert_eq!(list.len(), 2);

        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"fs-skill"));
        assert!(names.contains(&"cfg-skill"));

        // Config skill should have been written to disk.
        assert!(tmp.path().join("cfg-skill").join("SKILL.md").exists());

        // Config skill should have bound_tool set; filesystem skill should not.
        let cfg = list.iter().find(|e| e.name == "cfg-skill");
        assert_eq!(cfg.map(|e| e.bound_tool.as_deref()), Some(Some("cfg-tool")));
        let fs = list.iter().find(|e| e.name == "fs-skill");
        assert_eq!(fs.map(|e| e.bound_tool.as_deref()), Some(None));
    }

    #[tokio::test]
    async fn from_config_and_dir_local_path() {
        let tmp = TempDir::new().expect("tempdir");

        // Create a skill at a custom local path (outside skills_dir).
        let custom_dir = tmp.path().join("custom").join("my-local-skill");
        std::fs::create_dir_all(&custom_dir).expect("mkdir");
        std::fs::write(
            custom_dir.join("SKILL.md"),
            "---\nname: my-local-skill\ndescription: A local path skill\n---\n",
        )
        .expect("write");

        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir(&skills_dir).expect("mkdir skills");

        let configs = vec![(
            "local-tool".to_string(),
            bitrouter_config::ToolConfig {
                skill: Some(custom_dir.to_string_lossy().to_string()),
                ..Default::default()
            },
        )];

        let reg = FilesystemSkillRegistry::from_config_and_dir(configs, skills_dir)
            .await
            .expect("from_config_and_dir");

        let list = reg.list().await.expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "my-local-skill");
        assert_eq!(list[0].bound_tool.as_deref(), Some("local-tool"));
    }

    #[tokio::test]
    async fn from_config_and_dir_github_remote() {
        let tmp = TempDir::new().expect("tempdir");

        let configs = vec![(
            "remote-tool".to_string(),
            bitrouter_config::ToolConfig {
                skill: Some("github:vercel-labs/agent-skills/react-best-practices".to_string()),
                description: Some("React best practices".to_string()),
                ..Default::default()
            },
        )];

        let reg = FilesystemSkillRegistry::from_config_and_dir(configs, tmp.path().to_path_buf())
            .await
            .expect("from_config_and_dir");

        let list = reg.list().await.expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "react-best-practices");
        assert_eq!(list[0].description, "React best practices");
        assert_eq!(
            list[0].source,
            "github:vercel-labs/agent-skills/react-best-practices"
        );
        assert_eq!(list[0].bound_tool.as_deref(), Some("remote-tool"));
    }
}
